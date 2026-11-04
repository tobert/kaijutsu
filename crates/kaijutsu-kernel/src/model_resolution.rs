//! "What model does this context play?" Read the context's own slots;
//! nothing else answers.
//!
//! A model call in context K runs on K's `model` slot. A context with no
//! performer, or one with no `model` slot, has no model. No character slot,
//! `backend_models` row or registry default stands in.

use kaijutsu_types::ContextId;

use crate::kernel_db::{ContextSlotRow, KernelDb, KernelDbError, KernelDbResult};
use crate::llm::SlotTunables;

/// The slot role a context's chat model runs on: the character's one model,
/// copied when the context is made.
pub const MODEL: &str = "model";

/// The slot role a fork's distillation runs on.
pub const DISTILL: &str = "distill";

/// The roles a character's slots may hold: `distill` (a cheaper model for
/// `kj fork --compact` and `kj drift pull|merge`) and `gate` (the council
/// server that judges the gate). The character's chat model is its one
/// model (`character_models`), not a slot.
pub const CHARACTER_SLOT_ROLES: &[&str] = &[DISTILL, crate::council::gate::GATE];

/// Names no context may take as its type: the slot roles, and `judge`, the
/// type the gate writes on a judge shadow.
pub const RESERVED_ROLES: &[&str] = &[MODEL, crate::council::gate::GATE, DISTILL, crate::council::shadow::JUDGE];

/// The backend kind of a council server. A slot on such a backend names a
/// served model, needs no `max_tokens`, and is never a chat model.
pub const COUNCIL_BACKEND_KIND: &str = "mk";

/// Whether a backend of `kind` is a council server.
pub fn is_council_kind(kind: &str) -> bool {
    kind == COUNCIL_BACKEND_KIND
}

/// Refuses `context_type` when it names a reserved role.
pub fn refuse_reserved_role(context_type: &str) -> Result<(), String> {
    if !RESERVED_ROLES.contains(&context_type) {
        return Ok(());
    }
    if context_type == crate::council::shadow::JUDGE {
        return Err(
            "'judge' is the type of a judge shadow, which the gate makes for the judge character a council \
             block names; choose another --type"
                .to_string(),
        );
    }
    Err(format!(
        "'{context_type}' is a slot role, not a context type: a context's `{context_type}` slot holds \
         its model, its distillation model or its council server; choose another --type"
    ))
}

/// Refuses a character slot `role` other than [`CHARACTER_SLOT_ROLES`],
/// naming the verb that holds what the role meant.
pub fn refuse_character_slot_role(role: &str, character: &str) -> Result<(), String> {
    if CHARACTER_SLOT_ROLES.contains(&role) {
        return Ok(());
    }
    if role == crate::council::shadow::JUDGE {
        return Err(
            "a character holds no `judge` slot: the judge is a character, `judge` on a shipped host. \
             `kj character model set judge <mk-backend>/<model>` gives it its council server, and \
             `judge = \"judge\"` in the council block names it"
                .to_string(),
        );
    }
    Err(format!(
        "a character has one model; `kj character model set {character} <backend>/<model>`. \
         A character's slots are `distill` and `gate`, not `{role}`"
    ))
}

/// The model a context runs, copied from its `model` slot.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextModel {
    pub character: String,
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
    /// The context has no performer.
    NoContextCharacter,
    /// The context's slots have none for the role.
    NoSlot,
    /// The role's slot is on a council server (a backend of kind `mk`): it
    /// names a served model, not a chat model.
    CouncilServer,
}

/// A context, or a role of it, that has no model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoModel {
    pub context: String,
    pub character: Option<String>,
    /// The role looked up: `model`, `gate` or `distill`.
    pub role: String,
    pub reason: NoModelReason,
    /// The context is an MCP session (its parent is an MCP hub), whose client
    /// brings its own model.
    pub mcp_session: bool,
}

impl NoModel {
    /// One line for `kj context create`, `kj context rotate` and `kj context info`.
    /// `None` for an MCP session with no `model` slot, whose client brings its
    /// own model.
    pub fn notice(&self) -> Option<String> {
        if self.mcp_session && self.reason == NoModelReason::NoSlot {
            return None;
        }
        Some(self.line())
    }

    /// The line itself, for any context.
    pub fn line(&self) -> String {
        let character = self.character.as_deref().unwrap_or("its character");
        match &self.reason {
            NoModelReason::NoContextCharacter => {
                format!("{} has no performer, so it runs no model", self.context)
            }
            NoModelReason::CouncilServer => format!(
                "{} runs on a council server, not a chat model: its {} slot names the server",
                self.context, self.role
            ),
            NoModelReason::NoSlot if self.role == MODEL => format!(
                "{} has no model: {character} has no model; `kj character model set {character} \
                 <backend>/<model>`, then `kj context rotate`",
                self.context
            ),
            NoModelReason::NoSlot => format!(
                "{} has no {} slot: {character} has no model and no {} slot",
                self.context, self.role, self.role
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
                "{} has no performer, so it {what}. Start one: \
                 `kj context create <character> [--name <label>] [--type <type>]`",
                self.context
            ),
            NoModelReason::CouncilServer => format!(
                "{} {what}: its {} slot names a council server, not a chat model",
                self.context, self.role
            ),
            NoModelReason::NoSlot if self.role == MODEL => format!(
                "{} {what}: character {character} has no model. Fix: `kj character model set {character} \
                 <backend>/<model>`, then `kj context rotate`",
                self.context
            ),
            NoModelReason::NoSlot => format!(
                "{} {what}: character {character} has no {} slot and no model. Fix: `kj character model set`, \
                 then `kj context rotate`",
                self.context, self.role
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
/// `role` is [`MODEL`] for the chat model, or `gate` or `distill`.
pub fn role_slot(db: &KernelDb, context_id: ContextId, role: &str) -> KernelDbResult<RoleSlot> {
    let context = db.context_name(context_id);
    let Some(row) = db.context_character(context_id)? else {
        return Ok(RoleSlot::None(NoModel {
            context,
            character: None,
            role: role.to_string(),
            reason: NoModelReason::NoContextCharacter,
            mcp_session: false,
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
            mcp_session: db.is_mcp_session(context_id)?,
        }));
    };
    let backend = db
        .list_backends()?
        .into_iter()
        .find(|b| b.backend_id == slot.backend_id)
        .ok_or_else(|| corrupt(format!("context {context} slot {role} names a backend that does not exist")))?;
    Ok(RoleSlot::Slot(BoundSlot { context, character, backend: backend.name, council_server: is_council_kind(&backend.kind), slot }))
}

/// A context slot with the names a caller reports.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundSlot {
    /// `label (short-id)` of the context.
    pub context: String,
    pub character: String,
    /// Backend NAME.
    pub backend: String,
    /// The backend is a council server (kind `mk`).
    pub council_server: bool,
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

/// The model context `context_id` runs, from its [`MODEL`] slot. A slot on a
/// council server (a judge shadow's) is no chat model. `Err` is a fault
/// reading the slot, or a chat slot with no `max_tokens` (which a context
/// character write refuses).
pub fn context_model(db: &KernelDb, context_id: ContextId) -> KernelDbResult<ContextModelLookup> {
    match role_slot(db, context_id, MODEL)? {
        RoleSlot::None(none) => Ok(ContextModelLookup::None(none)),
        RoleSlot::Slot(bound) if bound.council_server => Ok(ContextModelLookup::None(NoModel {
            context: bound.context,
            character: Some(bound.character),
            role: MODEL.to_string(),
            reason: NoModelReason::CouncilServer,
            mcp_session: false,
        })),
        RoleSlot::Slot(bound) => {
            let slot = &bound.slot;
            let max_tokens = non_negative(slot.max_tokens).filter(|m| *m > 0).ok_or_else(|| {
                corrupt(format!(
                    "context {} slot {MODEL} has no max_tokens, which every request carries",
                    bound.context
                ))
            })?;
            Ok(ContextModelLookup::Model(ContextModel {
                character: bound.character.clone(),
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
