//! "What model does this context play?" Read the context's own slots;
//! nothing else answers.
//!
//! A model call in context K runs on the `context_slots` row for K's context
//! type. A context with no context character, or one with no slot for the
//! type, has no model. No character slot, `backend_models` row or registry default
//! stands in.

use kaijutsu_types::ContextId;

use crate::kernel_db::{ContextSlotRow, KernelDb, KernelDbError, KernelDbResult};
use crate::llm::SlotTunables;

/// Cast slot roles that are not context types. The `gate` slot names the
/// council server for gate decisions, `judge` the server for the judge
/// shadow, and `distill` the model that summarizes a fork. No context takes a
/// chat model from them, and no context may have one as its type.
pub const RESERVED_ROLES: &[&str] = &[crate::council::gate::GATE, crate::council::shadow::JUDGE, DISTILL];

/// The slot role a fork's distillation runs on.
pub const DISTILL: &str = "distill";

/// Refuses `context_type` when it names a reserved slot role.
pub fn refuse_reserved_role(context_type: &str) -> Result<(), String> {
    if !RESERVED_ROLES.contains(&context_type) {
        return Ok(());
    }
    Err(format!(
        "'{context_type}' is a slot role, not a context type: a character's `{context_type}` slot configures \
         a council or distillation model for the contexts it plays; choose another --type"
    ))
}

/// The model a context runs, copied from its slot.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextModel {
    pub character: String,
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
    /// The context has no context character.
    NoContextCharacter,
    /// The context's slots have none for the role.
    NoSlot,
}

/// A context, or a role of it, that has no model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoModel {
    pub context: String,
    pub character: Option<String>,
    /// The role looked up: the context's type, or `gate`, `judge`, `distill`.
    pub role: String,
    pub reason: NoModelReason,
}

impl NoModel {
    /// One line for `kj context create`, `kj context rotate` and `kj context info`.
    pub fn line(&self) -> String {
        match &self.reason {
            NoModelReason::NoContextCharacter => {
                format!("{} has no context character, so it runs no model", self.context)
            }
            NoModelReason::NoSlot => format!(
                "{} has no model for {}: {} has no {} slot",
                self.context,
                self.role,
                self.character.as_deref().unwrap_or("its character"),
                self.role
            ),
        }
    }

    /// The refusal for a model turn: the character, the role and the fix.
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
            NoModelReason::NoContextCharacter => format!(
                "{} has no context character, so it {what}. Start one: \
                 `kj context create <label> --as <character> --type <type>`",
                self.context
            ),
            NoModelReason::NoSlot => format!(
                "{} {what}: character {character} has no {} slot. Fix: give the character a {} slot with `kj character slot set`, \
                 then `kj context rotate`",
                self.context, self.role, self.role
            ),
        }
    }
}

/// A role's answer on a context.
#[derive(Debug, Clone, PartialEq)]
pub enum ContextModelLookup {
    Model(ContextModel),
    None(NoModel),
}

impl ContextModelLookup {
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

/// The slot of `role` in context `context_id`, with the names around it.
/// `role` is the context's type for a chat model, or a reserved role.
pub fn role_slot(db: &KernelDb, context_id: ContextId, role: &str) -> KernelDbResult<RoleSlot> {
    let context = db.context_name(context_id);
    let Some(row) = db.context_character(context_id)? else {
        return Ok(RoleSlot::None(NoModel {
            context,
            character: None,
            role: role.to_string(),
            reason: NoModelReason::NoContextCharacter,
        }));
    };
    let character = db
        .get_character(row.principal_id)?
        .map(|c| c.name)
        .unwrap_or_else(|| row.principal_id.to_string());
    let Some(slot) = db.context_slot(context_id, role)? else {
        return Ok(RoleSlot::None(NoModel {
            context,
            character: Some(character),
            role: role.to_string(),
            reason: NoModelReason::NoSlot,
        }));
    };
    let backend = db
        .list_backends()?
        .into_iter()
        .find(|b| b.backend_id == slot.backend_id)
        .ok_or_else(|| corrupt(format!("context {context} slot {role} names a backend that does not exist")))?
        .name;
    Ok(RoleSlot::Slot(BoundSlot { context, character, backend, slot }))
}

/// A context slot with the names a caller reports.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundSlot {
    /// `label (short-id)` of the context.
    pub context: String,
    pub character: String,
    /// Backend NAME.
    pub backend: String,
    pub slot: ContextSlotRow,
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

/// The model context `context_id` runs, from its slot. `Err` is a fault
/// reading the slot, or a chat slot with no `max_tokens` (which a context
/// character write refuses).
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
                    "context {} slot {context_type} has no max_tokens, which every request carries",
                    bound.context
                ))
            })?;
            Ok(ContextModelLookup::Model(ContextModel {
                character: bound.character.clone(),
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
