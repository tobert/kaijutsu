//! Context characters: the dynamic layer of the senpai/kohai model.
//!
//! A context character is one character in one context: the `performer`,
//! `senpai_ctx` and `persona` columns of the `contexts` row. A context whose
//! `performer` is NULL runs no turns but still answers to a senpai. Its
//! `context_slots` are a copy of the character's own slots as they were when
//! the context was made, with every parameter the slot leaves unset filled
//! from the model's `backend_models` row. Nothing but `senpai_ctx` changes
//! after the context is made. `docs/senpai-kohai.md`, "The model" and "Where a
//! model comes from".

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, Result as SqliteResult, params};

use kaijutsu_types::{BackendId, ContextId, PrincipalId};

use super::{
    CharacterSlotRow, KernelDb, KernelDbError, KernelDbResult, ModelTunablesRow, blob_param, read_backend_id,
    read_context_id, read_opt_context_id, read_opt_principal_id, read_principal_id,
};

/// The performer of a context, with the context's senpai and persona.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextCharacterRow {
    pub context_id: ContextId,
    pub principal_id: PrincipalId,
    pub senpai_ctx: Option<ContextId>,
    pub persona: Option<String>,
    pub created_at: i64,
}

/// What a context is, read from its performer and the character pointers
/// (`docs/character.md`, "A person, the console, MCP sessions, the judge").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextFacts {
    /// The performer's name; `None` for a context with no performer.
    pub performer: Option<String>,
    /// The performer is a root character, a person.
    pub performed_by_person: bool,
    /// Some character's `root_ctx` names the context.
    pub is_root_console: bool,
    /// Some character's `console_ctx` names the context.
    pub is_console: bool,
    /// The context's parent (`forked_from`) is the MCP hub some character's
    /// `mcp_hub_ctx` names.
    pub is_mcp_session: bool,
}

/// What [`KernelDb::archive_uncommitted_root_successors`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StraySweep {
    /// The strays it archived, oldest first.
    pub archived: Vec<ContextId>,
    /// The strays it could not archive, with the error.
    pub failed: Vec<(ContextId, String)>,
}

/// The facts columns, selected from `contexts c` left-joined to its
/// performer's sheet `p`.
const CONTEXT_FACTS_SELECT: &str = "SELECT c.context_id, p.name, COALESCE(p.root, 0),
        EXISTS(SELECT 1 FROM characters r WHERE r.root_ctx = c.context_id),
        EXISTS(SELECT 1 FROM characters k WHERE k.console_ctx = c.context_id),
        EXISTS(SELECT 1 FROM characters h WHERE h.mcp_hub_ctx = c.forked_from)
     FROM contexts c LEFT JOIN characters p ON p.principal_id = c.performer";

fn read_context_facts(r: &rusqlite::Row<'_>) -> SqliteResult<(ContextId, ContextFacts)> {
    Ok((read_context_id(r, 0)?, ContextFacts {
        performer: r.get(1)?,
        performed_by_person: r.get(2)?,
        is_root_console: r.get(3)?,
        is_console: r.get(4)?,
        is_mcp_session: r.get(5)?,
    }))
}

/// A copy of one character slot, tunables filled from the model's row.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextSlotRow {
    pub context_id: ContextId,
    pub role: String,
    pub backend_id: BackendId,
    pub model: String,
    pub max_tokens: Option<i64>,
    pub context_window: Option<i64>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub effort: Option<String>,
    pub thinking_budget: Option<i64>,
    pub thinking_style: Option<String>,
}

/// One line of `context_character_migration_report`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextCharacterMigrationNote {
    pub kind: String,
    pub context_id: Option<ContextId>,
    /// The character the note is about, for a note that is not about a
    /// context (`rc_dir_unset`).
    pub principal_id: Option<PrincipalId>,
    pub note: String,
}

/// `own` over `floor`, field by field.
pub(super) fn over(own: &ModelTunablesRow, floor: &ModelTunablesRow) -> ModelTunablesRow {
    ModelTunablesRow {
        max_tokens: own.max_tokens.or(floor.max_tokens),
        temperature: own.temperature.or(floor.temperature),
        top_p: own.top_p.or(floor.top_p),
        effort: own.effort.clone().or_else(|| floor.effort.clone()),
        thinking_budget: own.thinking_budget.or(floor.thinking_budget),
        thinking_style: own.thinking_style.clone().or_else(|| floor.thinking_style.clone()),
    }
}

fn slot_tunables(slot: &CharacterSlotRow) -> ModelTunablesRow {
    ModelTunablesRow {
        max_tokens: slot.max_tokens,
        temperature: slot.temperature,
        top_p: slot.top_p,
        effort: slot.effort.clone(),
        thinking_budget: slot.thinking_budget,
        thinking_style: slot.thinking_style.clone(),
    }
}

/// One slot's values, ready to write.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct SlotValues {
    pub(super) backend_id: BackendId,
    pub(super) model: String,
    pub(super) max_tokens: Option<i64>,
    pub(super) context_window: Option<i64>,
    pub(super) temperature: Option<f64>,
    pub(super) top_p: Option<f64>,
    pub(super) effort: Option<String>,
    pub(super) thinking_budget: Option<i64>,
    pub(super) thinking_style: Option<String>,
}

impl SlotValues {
    pub(super) fn new(backend_id: BackendId, model: &str, tunables: ModelTunablesRow, max_tokens: Option<i64>, context_window: Option<i64>) -> Self {
        Self {
            backend_id,
            model: model.to_string(),
            max_tokens,
            context_window,
            temperature: tunables.temperature,
            top_p: tunables.top_p,
            effort: tunables.effort,
            thinking_budget: tunables.thinking_budget,
            thinking_style: tunables.thinking_style,
        }
    }
}

pub(super) fn insert_slot(conn: &Connection, context_id: ContextId, role: &str, slot: &SlotValues) -> KernelDbResult<()> {
    conn.execute(
        "INSERT INTO context_slots (context_id, role, backend_id, model, max_tokens, context_window,
                                    temperature, top_p, effort, thinking_budget, thinking_style)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            blob_param(context_id.as_bytes()),
            role,
            blob_param(slot.backend_id.as_bytes()),
            slot.model,
            slot.max_tokens,
            slot.context_window,
            slot.temperature,
            slot.top_p,
            slot.effort,
            slot.thinking_budget,
            slot.thinking_style,
        ],
    )?;
    Ok(())
}

fn row_to_character(row: &rusqlite::Row<'_>) -> SqliteResult<ContextCharacterRow> {
    Ok(ContextCharacterRow {
        context_id: read_context_id(row, 0)?,
        principal_id: read_principal_id(row, 1)?,
        senpai_ctx: read_opt_context_id(row, 2)?,
        persona: row.get(3)?,
        created_at: row.get(4)?,
    })
}

/// The columns of a performed context, in the order `row_to_character` reads.
const CHARACTER_COLS: &str = "context_id, performer, senpai_ctx, persona, created_at";

const PERFORMED: &str = "FROM contexts WHERE context_id = ?1 AND performer IS NOT NULL";

fn row_to_slot(row: &rusqlite::Row<'_>) -> SqliteResult<ContextSlotRow> {
    Ok(ContextSlotRow {
        context_id: read_context_id(row, 0)?,
        role: row.get(1)?,
        backend_id: read_backend_id(row, 2)?,
        model: row.get(3)?,
        max_tokens: row.get(4)?,
        context_window: row.get(5)?,
        temperature: row.get(6)?,
        top_p: row.get(7)?,
        effort: row.get(8)?,
        thinking_budget: row.get(9)?,
        thinking_style: row.get(10)?,
    })
}

const SLOT_COLS: &str = "context_id, role, backend_id, model, max_tokens, context_window, \
                         temperature, top_p, effort, thinking_budget, thinking_style";

/// Give `child` a copy of `source`'s performance: the same performer and
/// persona and every slot verbatim, with `source` as its senpai. A source with
/// no performer gives the child only the senpai edge. Returns whether a
/// performer was copied.
/// Refuse a performer with no character sheet: every performer the kernel
/// writes names one, and `rc` and `kj` read the sheet to run the context.
pub(super) fn require_character_sheet(conn: &Connection, performer: PrincipalId) -> KernelDbResult<()> {
    let sheet: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM characters WHERE principal_id = ?1)",
        params![blob_param(performer.as_bytes())],
        |r| r.get(0),
    )?;
    if !sheet {
        return Err(KernelDbError::Validation(format!(
            "performer {} has no character sheet; `kj character create` makes one",
            performer.short()
        )));
    }
    Ok(())
}

pub(super) fn copy_character_into(conn: &Connection, source: ContextId, child: ContextId) -> KernelDbResult<bool> {
    let row = conn
        .query_row(
            "SELECT performer, persona FROM contexts WHERE context_id = ?1",
            params![blob_param(source.as_bytes())],
            |r| Ok((read_opt_principal_id(r, 0)?, r.get::<_, Option<String>>(1)?)),
        )
        .optional()?
        .ok_or_else(|| KernelDbError::NotFound(format!("context {}", source.short())))?;
    let (performer, persona) = row;
    if let Some(performer) = performer {
        require_character_sheet(conn, performer)?;
    }
    conn.execute(
        "UPDATE contexts SET senpai_ctx = ?2, performer = ?3, persona = ?4 WHERE context_id = ?1",
        params![
            blob_param(child.as_bytes()),
            blob_param(source.as_bytes()),
            performer.as_ref().map(|p| p.as_bytes().to_vec()),
            persona,
        ],
    )?;
    if performer.is_none() {
        return Ok(false);
    }
    conn.execute(
        "INSERT INTO context_slots (context_id, role, backend_id, model, max_tokens, context_window,
                                    temperature, top_p, effort, thinking_budget, thinking_style)
         SELECT ?2, role, backend_id, model, max_tokens, context_window,
                temperature, top_p, effort, thinking_budget, thinking_style
         FROM context_slots WHERE context_id = ?1",
        params![blob_param(source.as_bytes()), blob_param(child.as_bytes())],
    )?;
    Ok(true)
}

/// The triggers that refuse an update or a delete of a context slot. A
/// migration that rewrites slots drops them and runs this inside its own
/// transaction.
pub(super) const CONTEXT_SLOTS_NEVER_CHANGED: &str = "
    CREATE TRIGGER IF NOT EXISTS context_slots_never_changed_u
        BEFORE UPDATE ON context_slots
        BEGIN SELECT RAISE(ABORT, 'context_slots rows are never changed'); END;
    CREATE TRIGGER IF NOT EXISTS context_slots_never_changed_d
        BEFORE DELETE ON context_slots
        BEGIN SELECT RAISE(ABORT, 'context_slots rows are never changed'); END;";

impl KernelDb {
    /// Indexes and triggers that keep the dynamic layer fixed, created on
    /// every open once the `contexts` columns exist.
    ///
    /// - A context's performer and persona never change once the performer is
    ///   set; its slots are never changed or deleted.
    /// - A context's senpai never becomes NULL unless the context is a
    ///   character's root console (`characters.root_ctx`). There is no insert
    ///   trigger: a root console is inserted before `root_ctx` points at it,
    ///   so the insert paths check in Rust ([`Self::check_senpai_edge`]).
    ///
    /// `senpai_ctx` stays updatable (`kj context set --senpai`).
    pub(super) fn ensure_context_guards(conn: &Connection) -> KernelDbResult<()> {
        conn.execute_batch(CONTEXT_SLOTS_NEVER_CHANGED)?;
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_contexts_performer
                 ON contexts(performer) WHERE performer IS NOT NULL;
             CREATE INDEX IF NOT EXISTS idx_contexts_senpai
                 ON contexts(senpai_ctx) WHERE senpai_ctx IS NOT NULL;
             CREATE TRIGGER IF NOT EXISTS contexts_performer_fixed
                 BEFORE UPDATE OF performer, persona ON contexts
                 WHEN OLD.performer IS NOT NULL
                   AND (NEW.performer IS NOT OLD.performer OR NEW.persona IS NOT OLD.persona)
                 BEGIN SELECT RAISE(ABORT, 'contexts.performer and persona are never changed'); END;
             CREATE TRIGGER IF NOT EXISTS contexts_senpai_required
                 BEFORE UPDATE OF senpai_ctx ON contexts
                 WHEN NEW.senpai_ctx IS NULL AND OLD.senpai_ctx IS NOT NULL
                   AND NOT EXISTS (SELECT 1 FROM characters WHERE root_ctx = NEW.context_id)
                 BEGIN SELECT RAISE(ABORT, 'only a root console has no senpai'); END;",
        )?;
        Ok(())
    }

    /// The one Rust check on a senpai edge about to be written. `senpai`
    /// must exist and not be `context`; `None` is allowed only for a context
    /// a live root character performs ([`Self::is_live_person`]), which is
    /// that character's root console once `characters.root_ctx` is pointed
    /// at it after the row is written.
    pub fn check_senpai_edge(
        &self,
        context: ContextId,
        performer: Option<PrincipalId>,
        senpai: Option<ContextId>,
    ) -> KernelDbResult<()> {
        let Some(senpai) = senpai else {
            let root = match performer {
                Some(performer) => self.is_live_person(performer)?,
                None => false,
            };
            if !root {
                return Err(KernelDbError::Validation(format!(
                    "context {} needs a senpai: only a live root character's console has none, \
                     and `kj character create --root` makes it",
                    self.context_name(context)
                )));
            }
            return Ok(());
        };
        if senpai == context {
            return Err(KernelDbError::Validation(format!(
                "context {} cannot be its own senpai",
                self.context_name(context)
            )));
        }
        if self.get_context(senpai)?.is_none() {
            return Err(KernelDbError::NotFound(format!("senpai context {}", senpai.short())));
        }
        Ok(())
    }

    /// Copy the model and slots of `principal_id` into `context_id`: the
    /// character's model under `model`, and under `distill` when the
    /// character has no `distill` slot and the model is no council server;
    /// then its `distill` and `gate` slots under their roles. A character
    /// with no model gives the context no `model` slot. A parameter left unset takes the model's
    /// `backend_models` value; nothing else supplies one. A chat model with
    /// no `max_tokens` in either place refuses, and so does
    /// `thinking_style=budget` with no `thinking_budget` below `max_tokens`;
    /// a council server (a backend of kind `mk`) is exempt from both. A
    /// character slot of any other role is corruption and refuses.
    fn copy_character_slots(&self, context_id: ContextId, principal_id: PrincipalId) -> KernelDbResult<()> {
        use crate::model_resolution::{CHARACTER_SLOT_ROLES, DISTILL, MODEL};
        let who = self.name_for(principal_id);
        let slots = self.list_character_slots(principal_id)?;
        if let Some(stray) = slots.iter().find(|slot| !CHARACTER_SLOT_ROLES.contains(&slot.role.as_str())) {
            return Err(KernelDbError::Validation(format!(
                "character {who} holds a `{}` slot, and a character's slots are `distill` and `gate`; \
                 `kj character slot unset {who} {}` removes it",
                stray.role, stray.role
            )));
        }
        if let Some(model) = self.get_character_model(principal_id)? {
            let subject = format!("the model of character {who}");
            let tunables = ModelTunablesRow {
                max_tokens: model.max_tokens,
                temperature: model.temperature,
                top_p: model.top_p,
                effort: model.effort.clone(),
                thinking_budget: model.thinking_budget,
                thinking_style: model.thinking_style.clone(),
            };
            self.copy_one_slot(context_id, MODEL, &subject, model.backend_id, &model.model, tunables.clone())?;
            // A council server is never a chat model, so it distills nothing.
            if !slots.iter().any(|slot| slot.role == DISTILL) && !self.backend_is_council_server(model.backend_id)? {
                self.copy_one_slot(context_id, DISTILL, &subject, model.backend_id, &model.model, tunables)?;
            }
        }
        for slot in &slots {
            let subject = format!("slot {} of character {who}", slot.role);
            self.copy_one_slot(context_id, &slot.role, &subject, slot.backend_id, &slot.model, slot_tunables(slot))?;
        }
        Ok(())
    }

    /// Write one context slot for `role` from a backend, a model and the
    /// tunables its source sets, filling the rest from the model's row.
    /// `subject` names the source in a refusal.
    fn copy_one_slot(
        &self,
        context_id: ContextId,
        role: &str,
        subject: &str,
        backend_id: BackendId,
        model: &str,
        own: ModelTunablesRow,
    ) -> KernelDbResult<()> {
        let model_row = self.list_backend_models(backend_id)?.into_iter().find(|m| m.model_id == model);
        let tunables = over(&own, &model_row.as_ref().map(|m| m.tunables.clone()).unwrap_or_default());
        let max_tokens = tunables.max_tokens;
        let council_server = self.backend_is_council_server(backend_id)?;
        if max_tokens.is_none() && !council_server {
            return Err(KernelDbError::Validation(format!(
                "{subject} runs model {model}, which has no max_tokens in the {} or in its \
                 backend_models row; every request carries max_tokens, so set it on one of them",
                if subject.starts_with("slot") { "slot" } else { "model" }
            )));
        }
        if !council_server && tunables.thinking_style.as_deref() == Some("budget") {
            let Some(budget) = tunables.thinking_budget else {
                return Err(KernelDbError::Validation(format!(
                    "{subject} runs model {model} with thinking_style=budget and no \
                     thinking_budget in its own settings or in its backend_models row; set one of them"
                )));
            };
            if let Some(max) = max_tokens
                && budget >= max
            {
                return Err(KernelDbError::Validation(format!(
                    "{subject} runs model {model} with thinking_budget ({budget}) not below \
                     max_tokens ({max}); lower the budget or raise max_tokens"
                )));
            }
        }
        let values = SlotValues::new(backend_id, model, tunables, max_tokens, model_row.and_then(|m| m.context_window));
        insert_slot(&self.conn, context_id, role, &values)
    }

    /// Run `f` atomically: its own transaction when none is open, else a
    /// savepoint inside the caller's, so a refusal inside `f` leaves none of
    /// its writes behind.
    fn atomically<T>(&self, f: impl FnOnce(&Self) -> KernelDbResult<T>) -> KernelDbResult<T> {
        if self.conn.is_autocommit() {
            return self.in_transaction(f);
        }
        self.conn.execute_batch("SAVEPOINT context_character_write")?;
        match f(self) {
            Ok(out) => {
                self.conn.execute_batch("RELEASE SAVEPOINT context_character_write")?;
                Ok(out)
            }
            Err(error) => {
                self.conn.execute_batch(
                    "ROLLBACK TO SAVEPOINT context_character_write; RELEASE SAVEPOINT context_character_write",
                )?;
                Err(error)
            }
        }
    }

    /// Make `principal_id` the performer of `context_id`, answering to
    /// `senpai`, with slots copied fresh from the character's own slots
    /// (none gives a context with no slots). Refuses, and writes nothing,
    /// when a slot has no `max_tokens`, the performer has no character sheet,
    /// the context already has a performer,
    /// or a `senpai` is not a valid edge ([`Self::check_senpai_edge`]). A
    /// `None` senpai is the creator's to justify: the row was written by
    /// [`Self::insert_placed_context`] or `insert_new_context_rows`, which
    /// refuse it for any context but a root console, and the database refuses
    /// a later update to NULL.
    pub fn write_context_character(
        &self,
        context_id: ContextId,
        principal_id: PrincipalId,
        senpai: Option<ContextId>,
        persona: Option<&str>,
    ) -> KernelDbResult<ContextCharacterRow> {
        self.atomically(|db| {
            if db.get_context(context_id)?.is_none() {
                return Err(KernelDbError::NotFound(format!("context {}", context_id.short())));
            }
            if db.context_character(context_id)?.is_some() {
                return Err(KernelDbError::Validation(format!(
                    "context {} already has a performer, which is fixed when the context is made",
                    db.context_name(context_id)
                )));
            }
            require_character_sheet(&db.conn, principal_id)?;
            if senpai.is_some() {
                db.check_senpai_edge(context_id, Some(principal_id), senpai)?;
            }
            db.conn.execute(
                "UPDATE contexts SET performer = ?2, senpai_ctx = ?3, persona = ?4 WHERE context_id = ?1",
                params![
                    blob_param(context_id.as_bytes()),
                    blob_param(principal_id.as_bytes()),
                    senpai.as_ref().map(|s| s.as_bytes().to_vec()),
                    persona,
                ],
            )?;
            db.copy_character_slots(context_id, principal_id)?;
            db.context_character(context_id)?.ok_or_else(|| {
                KernelDbError::NotFound(format!("context {}", context_id.short()))
            })
        })
    }

    /// Test fixture: give `child` a copy of `source`'s performance, as a
    /// fork does: same performer and persona, slots verbatim, `source` as
    /// senpai. `None` when `source` has no performer, which gives the child
    /// the senpai edge alone.
    #[cfg(any(test, feature = "test-util"))]
    pub fn copy_context_character(&self, source: ContextId, child: ContextId) -> KernelDbResult<Option<ContextCharacterRow>> {
        self.atomically(|db| {
            if db.context_character(child)?.is_some() {
                return Err(KernelDbError::Validation(format!(
                    "context {} already has a performer, which is fixed when the context is made",
                    db.context_name(child)
                )));
            }
            if !copy_character_into(&db.conn, source, child)? {
                return Ok(None);
            }
            db.context_character(child)
        })
    }

    /// The performer of a context with the context's senpai and persona, or
    /// `None` for a context that has no performer.
    pub fn context_character(&self, context_id: ContextId) -> KernelDbResult<Option<ContextCharacterRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {CHARACTER_COLS} {PERFORMED}"),
                params![blob_param(context_id.as_bytes())],
                row_to_character,
            )
            .optional()?)
    }

    /// The performer of a context; a context with none is refused, naming it.
    pub(crate) fn require_context_character(&self, context_id: ContextId) -> KernelDbResult<ContextCharacterRow> {
        self.context_character(context_id)?.ok_or_else(|| {
            KernelDbError::Validation(format!(
                "context {} has no performer, so it runs no turns; a context gets one from \
                 `kj context create <character>`, or by forking one that has it",
                self.context_name(context_id)
            ))
        })
    }

    /// The slot for `role` in a context, or `None`: a context with no slot
    /// for a role has no model for it.
    pub fn context_slot(&self, context_id: ContextId, role: &str) -> KernelDbResult<Option<ContextSlotRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {SLOT_COLS} FROM context_slots WHERE context_id = ?1 AND role = ?2"),
                params![blob_param(context_id.as_bytes()), role],
                row_to_slot,
            )
            .optional()?)
    }

    /// Every slot of a context, ordered by role.
    pub fn list_context_slots(&self, context_id: ContextId) -> KernelDbResult<Vec<ContextSlotRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SLOT_COLS} FROM context_slots WHERE context_id = ?1 ORDER BY role"
        ))?;
        let rows = stmt.query_map(params![blob_param(context_id.as_bytes())], row_to_slot)?;
        Ok(rows.collect::<SqliteResult<_>>()?)
    }

    /// `label (short-id)` of a context, or its short id when it has no row or label.
    pub fn context_name(&self, context_id: ContextId) -> String {
        match self.get_context(context_id) {
            Ok(Some(row)) => Self::context_display(&row),
            _ => context_id.short(),
        }
    }

    /// The live (not archived) contexts whose senpai is `context_id`, oldest
    /// first.
    pub fn kohai_of(&self, context_id: ContextId) -> KernelDbResult<Vec<ContextId>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id FROM contexts
             WHERE senpai_ctx = ?1 AND archived_at IS NULL
             ORDER BY created_at, context_id",
        )?;
        let rows = stmt.query_map(params![blob_param(context_id.as_bytes())], |r| read_context_id(r, 0))?;
        Ok(rows.collect::<SqliteResult<_>>()?)
    }

    /// The live contexts `principal_id` performs, oldest first.
    pub fn character_live_contexts(&self, principal_id: PrincipalId) -> KernelDbResult<Vec<ContextId>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id FROM contexts
             WHERE performer = ?1 AND archived_at IS NULL
             ORDER BY created_at, context_id",
        )?;
        let rows = stmt.query_map(params![blob_param(principal_id.as_bytes())], |r| read_context_id(r, 0))?;
        Ok(rows.collect::<SqliteResult<_>>()?)
    }

    /// A root character's console: `characters.root_ctx`. `None` for a
    /// character that is not a root, has no console yet, or has no sheet.
    pub fn root_console(&self, principal_id: PrincipalId) -> KernelDbResult<Option<ContextId>> {
        Ok(self.get_character(principal_id)?.filter(|sheet| sheet.root).and_then(|sheet| sheet.root_ctx))
    }

    /// Whether `context` is some character's root console: the context
    /// `characters.root_ctx` names.
    pub fn is_root_console(&self, context: ContextId) -> KernelDbResult<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM characters WHERE root_ctx = ?1)",
            params![blob_param(context.as_bytes())],
            |r| r.get(0),
        )?)
    }

    /// Whether `context` is an MCP session: its parent (`forked_from`) is
    /// the MCP hub some character's `mcp_hub_ctx` names.
    pub fn is_mcp_session(&self, context: ContextId) -> KernelDbResult<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM contexts x JOIN characters c ON c.mcp_hub_ctx = x.forked_from
                           WHERE x.context_id = ?1)",
            params![blob_param(context.as_bytes())],
            |r| r.get(0),
        )?)
    }

    /// The live MCP sessions of `hub`, oldest first: the live contexts whose
    /// parent (`forked_from`) it is, when some character's `mcp_hub_ctx`
    /// names it. A context that is no sheet's hub has none.
    pub fn live_mcp_sessions(&self, hub: ContextId) -> KernelDbResult<Vec<ContextId>> {
        let mut stmt = self.conn.prepare(
            "SELECT x.context_id FROM contexts x
             WHERE x.forked_from = ?1 AND x.archived_at IS NULL
               AND EXISTS (SELECT 1 FROM characters h WHERE h.mcp_hub_ctx = ?1)
             ORDER BY x.created_at, x.context_id",
        )?;
        let rows = stmt.query_map(params![blob_param(hub.as_bytes())], |r| read_context_id(r, 0))?;
        Ok(rows.collect::<SqliteResult<_>>()?)
    }

    /// Whether `principal_id` is a person: a character whose sheet has the
    /// `root` flag, retired or not. What a context is (its wire facts, a
    /// draft run as shell) reads this. A principal with no sheet is not one.
    pub fn is_person(&self, principal_id: PrincipalId) -> KernelDbResult<bool> {
        Ok(self.get_character(principal_id)?.is_some_and(|sheet| sheet.root))
    }

    /// Whether `principal_id` is a live person: a root character that is
    /// not retired. What a person may still make or hold (a context with no
    /// senpai, a console) reads this.
    pub fn is_live_person(&self, principal_id: PrincipalId) -> KernelDbResult<bool> {
        Ok(self.get_character(principal_id)?.is_some_and(|sheet| sheet.is_live_root()))
    }

    /// Whether a person performs `context`. Every context a person performs
    /// is alike: it runs no model turns, its draft runs as shell, and the
    /// person may confirm their own commands there.
    pub fn performed_by_person(&self, context: ContextId) -> KernelDbResult<bool> {
        match self.get_context(context)?.and_then(|row| row.performer) {
            Some(performer) => self.is_person(performer),
            None => Ok(false),
        }
    }

    /// The character whose `console_ctx` names `context`: the person whose
    /// console it is. `None` when no sheet points at it. More than one sheet
    /// naming it is corruption and refuses, naming them.
    pub fn console_owner(&self, context: ContextId) -> KernelDbResult<Option<PrincipalId>> {
        let mut stmt = self.conn.prepare(
            "SELECT principal_id, name FROM characters WHERE console_ctx = ?1 ORDER BY name",
        )?;
        let owners = stmt
            .query_map(params![blob_param(context.as_bytes())], |r| Ok((read_principal_id(r, 0)?, r.get::<_, String>(1)?)))?
            .collect::<SqliteResult<Vec<_>>>()?;
        match owners.as_slice() {
            [] => Ok(None),
            [(owner, _)] => Ok(Some(*owner)),
            many => {
                let names = many.iter().map(|(_, name)| name.as_str()).collect::<Vec<_>>();
                Err(KernelDbError::Corrupt(format!(
                    "context {} is named as the console of {}, and a context is the console of one character; \
                     `kj character set <name> --no-console` clears all but one",
                    self.context_name(context),
                    names.join(" and ")
                )))
            }
        }
    }

    /// Whether `context` is a console: the context some character's
    /// `console_ctx` names.
    pub fn is_console(&self, context: ContextId) -> KernelDbResult<bool> {
        Ok(self.console_owner(context)?.is_some())
    }

    /// Every live console: the live contexts that a `console_ctx` names.
    pub fn list_live_consoles(&self) -> KernelDbResult<Vec<ContextId>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT c.context_id FROM contexts c JOIN characters k ON k.console_ctx = c.context_id
             WHERE c.archived_at IS NULL ORDER BY c.context_id",
        )?;
        Ok(stmt.query_map([], |r| read_context_id(r, 0))?.collect::<SqliteResult<_>>()?)
    }

    /// The facts about `context` that clients read to tell what it is
    /// ([`ContextFacts`]). A context with no row has none and refuses.
    pub fn context_facts(&self, context: ContextId) -> KernelDbResult<ContextFacts> {
        self.conn.query_row(
            &format!("{CONTEXT_FACTS_SELECT} WHERE c.context_id = ?1"),
            params![blob_param(context.as_bytes())],
            read_context_facts,
        ).optional()?
            .map(|(_, facts)| facts)
            .ok_or_else(|| KernelDbError::NotFound(format!("context {}", context.short())))
    }

    /// [`Self::context_facts`] for each of `contexts`, in one query per 500
    /// ids. A context with no row has no entry.
    pub fn context_facts_for(&self, contexts: &[ContextId]) -> KernelDbResult<std::collections::HashMap<ContextId, ContextFacts>> {
        let mut facts = std::collections::HashMap::with_capacity(contexts.len());
        for chunk in contexts.chunks(500) {
            let marks = vec!["?"; chunk.len()].join(", ");
            let mut stmt = self.conn.prepare(&format!("{CONTEXT_FACTS_SELECT} WHERE c.context_id IN ({marks})"))?;
            let ids = chunk.iter().map(|id| id.as_bytes().to_vec());
            for row in stmt.query_map(rusqlite::params_from_iter(ids), read_context_facts)? {
                let (id, fact) = row?;
                facts.insert(id, fact);
            }
        }
        Ok(facts)
    }

    /// Archive each live context a live root character performs that has no
    /// senpai and that no character's `root_ctx` names. A root console's
    /// rotation inserts its successor before `commit_rotation` moves
    /// `root_ctx`; a rotation that stopped between the two leaves such a
    /// row, which no verb can place or promote. The predecessor is still the
    /// console. A stray that fails to archive is returned with its error and
    /// the rest are still archived.
    pub fn archive_uncommitted_root_successors(&self) -> KernelDbResult<StraySweep> {
        let strays: Vec<ContextId> = {
            let mut stmt = self.conn.prepare(
                "SELECT c.context_id FROM contexts c JOIN characters p ON p.principal_id = c.performer
                 WHERE p.root = 1 AND p.retired_at IS NULL AND c.senpai_ctx IS NULL AND c.archived_at IS NULL
                   AND c.context_id NOT IN (SELECT root_ctx FROM characters WHERE root_ctx IS NOT NULL)
                 ORDER BY c.created_at, c.context_id",
            )?;
            stmt.query_map([], |r| read_context_id(r, 0))?
                .collect::<SqliteResult<_>>()?
        };
        let mut sweep = StraySweep::default();
        for stray in strays {
            match self.archive_context(stray) {
                Ok(_) => sweep.archived.push(stray),
                Err(error) => sweep.failed.push((stray, error.to_string())),
            }
        }
        Ok(sweep)
    }

    /// Point a character's `root_ctx` at its console, or clear it.
    pub fn set_character_root_ctx(&self, principal_id: PrincipalId, ctx: Option<ContextId>) -> KernelDbResult<()> {
        let updated = self.conn.execute(
            "UPDATE characters SET root_ctx = ?1 WHERE principal_id = ?2",
            params![ctx.as_ref().map(|c| c.as_bytes().to_vec()), blob_param(principal_id.as_bytes())],
        )?;
        if updated == 0 {
            return Err(KernelDbError::NotFound(format!("character {}", principal_id.short())));
        }
        Ok(())
    }

    /// The context at the top of `context_id`'s senpai chain (the one with
    /// no senpai) and its performer. A cycle and a senpai that has no row are
    /// corruption and refuse.
    pub fn lineage_top(&self, context_id: ContextId) -> KernelDbResult<(ContextId, Option<PrincipalId>)> {
        let mut current = context_id;
        let mut visited = HashSet::new();
        loop {
            if !visited.insert(current) {
                return Err(KernelDbError::Validation(format!(
                    "context {} is its own senpai ancestor; the senpai chain has a cycle",
                    self.context_name(current)
                )));
            }
            let row = self.get_context(current)?.ok_or_else(|| {
                KernelDbError::NotFound(format!(
                    "senpai {} has no row, so the lineage root of {} cannot be found",
                    current.short(),
                    self.context_name(context_id)
                ))
            })?;
            match row.senpai_ctx {
                Some(senpai) => current = senpai,
                None => return Ok((current, row.performer)),
            }
        }
    }

    /// Point `context`'s senpai at `senpai`, or make it a root console with
    /// `None`. Refuses a senpai with no row, a context that is its own senpai,
    /// a cycle, and a `None` for a context that is not a root console. Who may
    /// move it, and the pending-ask rule, belong to the caller. The edge is the
    /// one mutable column of the dynamic layer, and the migration's notes about
    /// it (`senpai_migration_report`, and the senpai kinds of
    /// `context_character_migration_report`) stand until it is set again.
    pub fn set_context_senpai(&self, context: ContextId, senpai: Option<ContextId>) -> KernelDbResult<()> {
        let row = self
            .get_context(context)?
            .ok_or_else(|| KernelDbError::NotFound(format!("context {}", context.short())))?;
        if senpai.is_none() && !self.is_root_console(context)? {
            return Err(KernelDbError::Validation(format!(
                "context {} is not a root console, and only a root console has no senpai; \
                 `kj character create` makes a root's console",
                self.context_name(context)
            )));
        }
        self.check_senpai_edge(context, row.performer, senpai)?;
        if let Some(senpai) = senpai {
            let mut visited = HashSet::new();
            let mut walk = self.senpai_of(senpai)?;
            while let Some(above) = walk {
                if above == context {
                    return Err(KernelDbError::Validation(format!(
                        "making {} the senpai of {} would close a senpai cycle",
                        self.context_name(senpai),
                        self.context_name(context)
                    )));
                }
                if !visited.insert(above) {
                    break; // an existing loop above; not ours to report here
                }
                walk = self.senpai_of(above)?;
            }
        }
        self.conn.execute(
            "UPDATE contexts SET senpai_ctx = ?1 WHERE context_id = ?2",
            params![senpai.as_ref().map(|s| s.as_bytes().to_vec()), blob_param(context.as_bytes())],
        )?;
        self.conn.execute(
            "DELETE FROM senpai_migration_report WHERE context_id = ?1",
            params![blob_param(context.as_bytes())],
        )?;
        // `senpai_assigned` and `archived_no_senpai` are about the senpai alone;
        // the older `dropped` and `unmapped` kinds also carry cast and label
        // notes, so only those that name the senpai go.
        self.conn.execute(
            "DELETE FROM context_character_migration_report
             WHERE context_id = ?1
               AND (kind IN ('senpai_assigned', 'archived_no_senpai')
                    OR (kind IN ('dropped', 'unmapped') AND note LIKE '%senpai%'))",
            params![blob_param(context.as_bytes())],
        )?;
        Ok(())
    }

    /// What `context-characters-v1` and `context-fold-v1` did not carry over
    /// or could not resolve.
    pub fn context_character_migration_report(&self) -> KernelDbResult<Vec<ContextCharacterMigrationNote>> {
        let mut stmt = self.conn.prepare(
            "SELECT kind, context_id, note, principal_id FROM context_character_migration_report ORDER BY report_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ContextCharacterMigrationNote {
                kind: r.get(0)?,
                context_id: read_opt_context_id(r, 1)?,
                note: r.get(2)?,
                principal_id: read_opt_principal_id(r, 3)?,
            })
        })?;
        Ok(rows.collect::<SqliteResult<_>>()?)
    }
}

#[cfg(test)]
mod tests;
