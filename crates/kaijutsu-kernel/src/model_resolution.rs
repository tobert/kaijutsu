//! "What model does this context play?" Read the context's character
//! instance binding; nothing else answers.
//!
//! A model call in context K runs on the `instance_slots` row for K's context
//! type in the binding K was made with (`contexts.instance_id` and
//! `contexts.binding_seq`). A context with no instance, or a binding with no
//! slot for the type, has no model. No cast, `backend_models` row or registry
//! default stands in.

use kaijutsu_types::{CharacterInstanceId, ContextId};

use crate::kernel_db::{InstanceSlotRow, KernelDb, KernelDbError, KernelDbResult};
use crate::llm::SlotTunables;

/// Cast slot roles that are not context types. The `gate` slot names the
/// council server for gate decisions, `judge` the server for the judge
/// shadow, and `distill` the model that summarizes a fork. No context takes a
/// chat model from them, and no context may have one as its type.
pub const RESERVED_ROLES: &[&str] = &[crate::council::gate::GATE, crate::council::shadow::JUDGE, DISTILL];

/// The cast slot role a fork's distillation runs on.
pub const DISTILL: &str = "distill";

/// Refuses `context_type` when it names a reserved cast role.
pub fn refuse_reserved_role(context_type: &str) -> Result<(), String> {
    if !RESERVED_ROLES.contains(&context_type) {
        return Ok(());
    }
    Err(format!(
        "'{context_type}' is a cast role, not a context type: a cast's `{context_type}` slot configures \
         a council or distillation model for the instances that play the cast; choose another --type"
    ))
}

/// The model a context runs, copied from its binding.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextModel {
    pub instance: CharacterInstanceId,
    /// The binding `seq` the context runs on.
    pub seq: i64,
    pub character: String,
    pub cast: Option<String>,
    pub context_type: String,
    /// Backend NAME, the registry key.
    pub backend: String,
    pub model: String,
    pub max_tokens: u64,
    pub context_window: Option<u64>,
    /// The slot's optional parameters. `max_tokens` is not repeated here.
    pub tunables: SlotTunables,
}

/// Why a context has no model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoModelReason {
    /// The context belongs to no character instance.
    NoInstance,
    /// The binding was written from a character with no cast.
    NoCast,
    /// The binding's cast has no slot for the role.
    NoSlot { cast: String },
}

/// A context, or a role of its instance, that has no model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoModel {
    pub context: String,
    /// The instance's name; `None` when the context has no instance.
    pub instance: Option<String>,
    pub character: Option<String>,
    /// The role looked up: the context's type, or `gate`, `judge`, `distill`.
    pub role: String,
    pub reason: NoModelReason,
}

impl NoModel {
    /// One line for `kj kohai create`, `kj context rotate` and `kj context info`.
    pub fn line(&self) -> String {
        let instance = self.instance.as_deref().unwrap_or(&self.context);
        match &self.reason {
            NoModelReason::NoInstance => format!("{} has no character instance, so it runs no model", self.context),
            NoModelReason::NoCast => format!(
                "{instance} has no model for {}: {} has no cast",
                self.role,
                self.character.as_deref().unwrap_or("its character")
            ),
            NoModelReason::NoSlot { cast } => {
                format!("{instance} has no model for {}: cast {cast} has no {} slot", self.role, self.role)
            }
        }
    }

    /// The refusal for a model turn: the character, the cast, the role and
    /// the fix.
    pub fn refusal(&self) -> String {
        self.refusal_as("runs no model")
    }

    /// The refusal for a call that needs the role's slot, with `what` saying
    /// what the context cannot do ("runs no model", "cannot distill"). It
    /// leads with the cause and the fix: an error block's summary keeps only
    /// the first 200 characters.
    pub fn refusal_as(&self, what: &str) -> String {
        let character = self.character.as_deref().unwrap_or("(none)");
        match &self.reason {
            NoModelReason::NoInstance => format!(
                "{} belongs to no character instance, so it {what}. Start one: \
                 `kj kohai create <label> --as <character> --type <type>`",
                self.context
            ),
            NoModelReason::NoCast => format!(
                "{} {what}: character {character} has no cast, so no {} slot. Fix: \
                 `kj character set {character} --cast <cast>`, then `kj context rotate`",
                self.context, self.role
            ),
            NoModelReason::NoSlot { cast } => format!(
                "{} {what}: character {character}'s cast {cast} has no {} slot. Fix: \
                 `kj cast slot set {cast} {} ...` or `kj character set {character} --cast <cast>`, \
                 then `kj context rotate`",
                self.context, self.role, self.role
            ),
        }
    }
}

/// A role's answer on a binding.
#[derive(Debug, Clone, PartialEq)]
pub enum ContextModelLookup {
    Model(ContextModel),
    None(NoModel),
}

impl ContextModelLookup {
    /// The label of the cast the binding was written from, whether or not it
    /// has a slot for the role.
    pub fn cast(&self) -> Option<&str> {
        match self {
            Self::Model(m) => m.cast.as_deref(),
            Self::None(NoModel { reason: NoModelReason::NoSlot { cast }, .. }) => Some(cast),
            Self::None(_) => None,
        }
    }

    /// The model, or the refusal text.
    pub fn into_model(self) -> Result<ContextModel, String> {
        match self {
            Self::Model(m) => Ok(m),
            Self::None(n) => Err(n.refusal()),
        }
    }
}

fn corrupt(what: String) -> KernelDbError {
    KernelDbError::Validation(what)
}

/// The slot of `role` in the binding context `context_id` runs on, with the
/// names around it. `role` is the context's type for a chat model, or a
/// reserved role.
pub fn role_slot(db: &KernelDb, context_id: ContextId, role: &str) -> KernelDbResult<RoleSlot> {
    let context = db.context_name(context_id);
    let Some((instance, seq)) = db.context_instance(context_id)? else {
        return Ok(RoleSlot::None(NoModel {
            context,
            instance: None,
            character: None,
            role: role.to_string(),
            reason: NoModelReason::NoInstance,
        }));
    };
    let row = db
        .get_instance(instance)?
        .ok_or_else(|| corrupt(format!("context {context} names character instance {instance}, which has no row")))?;
    let character = db
        .get_character(row.principal_id)?
        .map(|c| c.name)
        .unwrap_or_else(|| row.principal_id.to_string());
    let binding = db
        .get_binding(instance, seq)?
        .ok_or_else(|| corrupt(format!("context {context} runs binding {seq} of {instance}, which has no row")))?;
    let instance_name = db.instance_name(instance);
    let Some(slot) = db.binding_slot(instance, seq, role)? else {
        let reason = match &binding.cast_label {
            Some(cast) => NoModelReason::NoSlot { cast: cast.clone() },
            None => NoModelReason::NoCast,
        };
        return Ok(RoleSlot::None(NoModel {
            context,
            instance: Some(instance_name),
            character: Some(character),
            role: role.to_string(),
            reason,
        }));
    };
    let backend = db
        .list_backends()?
        .into_iter()
        .find(|b| b.backend_id == slot.backend_id)
        .ok_or_else(|| corrupt(format!("binding {seq} of {instance} slot {role} names a backend that does not exist")))?
        .name;
    Ok(RoleSlot::Slot(BoundSlot { instance, seq, instance_name, character, cast: binding.cast_label, backend, slot }))
}

/// A binding slot with the names a caller reports.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundSlot {
    pub instance: CharacterInstanceId,
    pub seq: i64,
    pub instance_name: String,
    pub character: String,
    pub cast: Option<String>,
    /// Backend NAME.
    pub backend: String,
    pub slot: InstanceSlotRow,
}

/// What [`role_slot`] found.
#[derive(Debug, Clone, PartialEq)]
pub enum RoleSlot {
    Slot(BoundSlot),
    None(NoModel),
}

fn non_negative(value: Option<i64>) -> Option<u64> {
    value.and_then(|v| u64::try_from(v).ok())
}

/// The model context `context_id` runs, from its binding. `Err` is a fault
/// reading the binding, or a chat slot with no `max_tokens` (which a binding
/// write refuses).
pub fn context_model(db: &KernelDb, context_id: ContextId) -> KernelDbResult<ContextModelLookup> {
    let context_type = db
        .get_context(context_id)?
        .ok_or_else(|| KernelDbError::NotFound(format!("context {}", context_id.short())))?
        .context_type;
    match role_slot(db, context_id, &context_type)? {
        RoleSlot::None(none) => Ok(ContextModelLookup::None(none)),
        RoleSlot::Slot(bound) => {
            let slot = &bound.slot;
            let max_tokens = non_negative(slot.max_tokens).filter(|m| *m > 0).ok_or_else(|| {
                corrupt(format!(
                    "binding {} of {} slot {context_type} has no max_tokens, which every request carries",
                    bound.seq, bound.instance_name
                ))
            })?;
            Ok(ContextModelLookup::Model(ContextModel {
                instance: bound.instance,
                seq: bound.seq,
                character: bound.character.clone(),
                cast: bound.cast.clone(),
                context_type,
                backend: bound.backend.clone(),
                model: slot.model.clone(),
                max_tokens,
                context_window: non_negative(slot.context_window),
                tunables: SlotTunables {
                    max_tokens: None,
                    temperature: slot.temperature,
                    top_p: slot.top_p,
                    effort: slot.effort.clone(),
                    thinking_budget: non_negative(slot.thinking_budget),
                    thinking_style: slot.thinking_style.clone(),
                },
            }))
        }
    }
}
