//! Context characters: the dynamic layer of the senpai/kohai model.
//!
//! A context character is one character in one context: the principal that
//! performs it, the context it answers to (its senpai), and the persona. Its
//! `context_slots` are a copy of the character's cast slots as they were when
//! the context was made, with every parameter the slot leaves unset filled
//! from the model's `backend_models` row. Nothing but `senpai_ctx` changes
//! after the context is made. `docs/senpai-kohai.md`, "The model" and "Where a
//! model comes from".

use std::collections::{BTreeMap, HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension, Result as SqliteResult, params};
use tracing::{info, warn};

use kaijutsu_types::{BackendId, CastId, ContextId, PrincipalId};

use super::{
    BackendModelRow, CAST_SLOT_COLS, CastRow, CastSlotRow, KernelDb, KernelDbError, KernelDbResult,
    ModelTunablesRow, blob_param, now_millis, read_backend_id, read_context_id, read_opt_cast_id,
    read_opt_context_id, read_opt_principal_id, read_principal_id, row_to_cast_slot,
};

/// One character in one context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextCharacterRow {
    pub context_id: ContextId,
    pub principal_id: PrincipalId,
    pub senpai_ctx: Option<ContextId>,
    pub persona: Option<String>,
    pub created_at: i64,
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
    pub note: String,
}

/// `max_tokens` the old model path sent when the slot, the model row and
/// `llm_defaults` all left it unset (`LlmRegistry::max_output_tokens`).
const UNSET_MAX_TOKENS: i64 = 64_000;

/// Roles whose slot names a council server. The served model carries no
/// `max_tokens`, so these slots may leave it unset.
fn names_council_server(role: &str) -> bool {
    matches!(role, "gate" | "judge")
}

/// `own` over `floor`, field by field.
fn over(own: &ModelTunablesRow, floor: &ModelTunablesRow) -> ModelTunablesRow {
    ModelTunablesRow {
        max_tokens: own.max_tokens.or(floor.max_tokens),
        temperature: own.temperature.or(floor.temperature),
        top_p: own.top_p.or(floor.top_p),
        effort: own.effort.clone().or_else(|| floor.effort.clone()),
        thinking_budget: own.thinking_budget.or(floor.thinking_budget),
        thinking_style: own.thinking_style.clone().or_else(|| floor.thinking_style.clone()),
    }
}

fn slot_tunables(slot: &CastSlotRow) -> ModelTunablesRow {
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
struct SlotValues {
    backend_id: BackendId,
    model: String,
    max_tokens: Option<i64>,
    context_window: Option<i64>,
    temperature: Option<f64>,
    top_p: Option<f64>,
    effort: Option<String>,
    thinking_budget: Option<i64>,
    thinking_style: Option<String>,
}

impl SlotValues {
    fn new(backend_id: BackendId, model: &str, tunables: ModelTunablesRow, max_tokens: Option<i64>, context_window: Option<i64>) -> Self {
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

fn insert_context_character(
    conn: &Connection,
    context_id: ContextId,
    principal_id: PrincipalId,
    senpai: Option<ContextId>,
    persona: Option<&str>,
    created_at: i64,
) -> KernelDbResult<()> {
    conn.execute(
        "INSERT INTO context_characters (context_id, principal_id, senpai_ctx, persona, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            blob_param(context_id.as_bytes()),
            blob_param(principal_id.as_bytes()),
            senpai.as_ref().map(|s| s.as_bytes().to_vec()),
            persona,
            created_at,
        ],
    )?;
    Ok(())
}

fn insert_slot(conn: &Connection, context_id: ContextId, role: &str, slot: &SlotValues) -> KernelDbResult<()> {
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

const CHARACTER_COLS: &str = "context_id, principal_id, senpai_ctx, persona, created_at";

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

/// Give `child` a copy of `source`'s context character: the same principal and
/// persona, every slot verbatim, and `source` as its senpai. A source with no
/// context character gives the child none. Returns whether a copy was made.
pub(super) fn copy_character_into(conn: &Connection, source: ContextId, child: ContextId) -> KernelDbResult<bool> {
    let row = conn
        .query_row(
            &format!("SELECT {CHARACTER_COLS} FROM context_characters WHERE context_id = ?1"),
            params![blob_param(source.as_bytes())],
            row_to_character,
        )
        .optional()?;
    let Some(row) = row else { return Ok(false) };
    insert_context_character(conn, child, row.principal_id, Some(source), row.persona.as_deref(), now_millis())?;
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

impl KernelDb {
    pub(super) const CONTEXT_CHARACTERS_MIGRATION: &'static str = "context-characters-v1";

    /// Triggers that keep the dynamic layer fixed. A context character's
    /// principal and persona never change; its slots are never changed or
    /// deleted. `senpai_ctx` stays updatable (`kj senpai set`).
    pub(super) fn ensure_context_character_guards(conn: &Connection) -> KernelDbResult<()> {
        conn.execute_batch(
            "CREATE TRIGGER IF NOT EXISTS context_slots_never_changed_u
                 BEFORE UPDATE ON context_slots
                 BEGIN SELECT RAISE(ABORT, 'context_slots rows are never changed'); END;
             CREATE TRIGGER IF NOT EXISTS context_slots_never_changed_d
                 BEFORE DELETE ON context_slots
                 BEGIN SELECT RAISE(ABORT, 'context_slots rows are never changed'); END;
             CREATE TRIGGER IF NOT EXISTS context_characters_fixed
                 BEFORE UPDATE OF context_id, principal_id, persona ON context_characters
                 WHEN NEW.context_id IS NOT OLD.context_id
                   OR NEW.principal_id IS NOT OLD.principal_id
                   OR NEW.persona IS NOT OLD.persona
                 BEGIN SELECT RAISE(ABORT, 'context_characters.principal_id and persona are never changed'); END;",
        )?;
        Ok(())
    }

    /// Copy the slots of `cast` into `context_id`. A parameter the slot leaves
    /// unset takes the model's `backend_models` value; nothing else supplies
    /// one. A slot with no `max_tokens` in either place refuses, and so does
    /// `thinking_style=budget` with no `thinking_budget` below `max_tokens`.
    fn copy_cast_slots(&self, context_id: ContextId, principal_id: PrincipalId, cast: &CastRow) -> KernelDbResult<()> {
        let who = self.name_for(principal_id);
        for slot in self.list_cast_slots(cast.cast_id)? {
            let model_row = self
                .list_backend_models(slot.backend_id)?
                .into_iter()
                .find(|m| m.model_id == slot.model);
            let tunables = over(
                &slot_tunables(&slot),
                &model_row.as_ref().map(|m| m.tunables.clone()).unwrap_or_default(),
            );
            let max_tokens = tunables.max_tokens;
            if max_tokens.is_none() && !names_council_server(&slot.role) {
                return Err(KernelDbError::Validation(format!(
                    "cast {} slot {} of character {who} runs model {}, which has no max_tokens in the slot or in its \
                     backend_models row; every request carries max_tokens, so set it on one of them",
                    cast.label, slot.role, slot.model
                )));
            }
            if !names_council_server(&slot.role) && tunables.thinking_style.as_deref() == Some("budget") {
                let Some(budget) = tunables.thinking_budget else {
                    return Err(KernelDbError::Validation(format!(
                        "cast {} slot {} of character {who} runs model {} with thinking_style=budget and no \
                         thinking_budget in the slot or in its backend_models row; set one of them",
                        cast.label, slot.role, slot.model
                    )));
                };
                if let Some(max) = max_tokens
                    && budget >= max
                {
                    return Err(KernelDbError::Validation(format!(
                        "cast {} slot {} of character {who} runs model {} with thinking_budget ({budget}) not below \
                         max_tokens ({max}); lower the budget or raise max_tokens",
                        cast.label, slot.role, slot.model
                    )));
                }
            }
            let values = SlotValues::new(
                slot.backend_id,
                &slot.model,
                tunables,
                max_tokens,
                model_row.and_then(|m| m.context_window),
            );
            insert_slot(&self.conn, context_id, &slot.role, &values)?;
        }
        Ok(())
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

    /// Make `principal_id` the character of `context_id`, answering to
    /// `senpai`, with slots copied fresh from the character's cast
    /// (`characters.cast_id`; none gives a context with no slots). Refuses,
    /// and writes nothing, when a slot has no `max_tokens` or the context
    /// already has a context character.
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
                    "context {} already has a context character, which is fixed when the context is made",
                    db.context_name(context_id)
                )));
            }
            let created_at = now_millis();
            insert_context_character(&db.conn, context_id, principal_id, senpai, persona, created_at)?;
            if let Some(cast) = db.character_cast(principal_id)? {
                db.copy_cast_slots(context_id, principal_id, &cast)?;
            }
            Ok(ContextCharacterRow {
                context_id,
                principal_id,
                senpai_ctx: senpai,
                persona: persona.map(str::to_string),
                created_at,
            })
        })
    }

    /// Give `child` a copy of `source`'s context character: same principal and
    /// persona, slots verbatim, `source` as senpai. `None` when `source` has
    /// no context character, which gives the child none.
    pub fn copy_context_character(&self, source: ContextId, child: ContextId) -> KernelDbResult<Option<ContextCharacterRow>> {
        self.atomically(|db| {
            if db.context_character(child)?.is_some() {
                return Err(KernelDbError::Validation(format!(
                    "context {} already has a context character, which is fixed when the context is made",
                    db.context_name(child)
                )));
            }
            if !copy_character_into(&db.conn, source, child)? {
                return Ok(None);
            }
            db.context_character(child)
        })
    }

    /// The context character of a context, or `None`.
    pub fn context_character(&self, context_id: ContextId) -> KernelDbResult<Option<ContextCharacterRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {CHARACTER_COLS} FROM context_characters WHERE context_id = ?1"),
                params![blob_param(context_id.as_bytes())],
                row_to_character,
            )
            .optional()?)
    }

    /// The context character of a context; a context with none is refused,
    /// naming it.
    pub(crate) fn require_context_character(&self, context_id: ContextId) -> KernelDbResult<ContextCharacterRow> {
        self.context_character(context_id)?.ok_or_else(|| {
            KernelDbError::Validation(format!(
                "context {} has no context character, so it has no senpai and routes nothing; \
                 a context gets one from `kj kohai create --as <character>`, or by forking one that has it",
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
            "SELECT cc.context_id FROM context_characters cc
             JOIN contexts c ON c.context_id = cc.context_id
             WHERE cc.senpai_ctx = ?1 AND c.archived_at IS NULL
             ORDER BY c.created_at, cc.context_id",
        )?;
        let rows = stmt.query_map(params![blob_param(context_id.as_bytes())], |r| read_context_id(r, 0))?;
        Ok(rows.collect::<SqliteResult<_>>()?)
    }

    /// The live contexts `principal_id` performs, oldest first.
    pub fn character_live_contexts(&self, principal_id: PrincipalId) -> KernelDbResult<Vec<ContextId>> {
        let mut stmt = self.conn.prepare(
            "SELECT cc.context_id FROM context_characters cc
             JOIN contexts c ON c.context_id = cc.context_id
             WHERE cc.principal_id = ?1 AND c.archived_at IS NULL
             ORDER BY c.created_at, cc.context_id",
        )?;
        let rows = stmt.query_map(params![blob_param(principal_id.as_bytes())], |r| read_context_id(r, 0))?;
        Ok(rows.collect::<SqliteResult<_>>()?)
    }

    /// A root character's console: `characters.root_ctx`. `None` for a
    /// character that is not a root, has no console yet, or has no sheet.
    pub fn root_console(&self, principal_id: PrincipalId) -> KernelDbResult<Option<ContextId>> {
        Ok(self.get_character(principal_id)?.filter(|sheet| sheet.root).and_then(|sheet| sheet.root_ctx))
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

    /// The context character at the top of `context_id`'s senpai chain (the
    /// one with no senpai). A cycle, a senpai that has no row and a senpai
    /// with no context character are corruption and refuse.
    pub fn lineage_top(&self, context_id: ContextId) -> KernelDbResult<ContextCharacterRow> {
        let mut current = context_id;
        let mut visited = HashSet::new();
        loop {
            if !visited.insert(current) {
                return Err(KernelDbError::Validation(format!(
                    "context {} is its own senpai ancestor; the senpai chain has a cycle",
                    self.context_name(current)
                )));
            }
            let row = if current == context_id {
                self.require_context_character(current)?
            } else {
                self.context_character(current)?.ok_or_else(|| {
                    KernelDbError::NotFound(format!(
                        "senpai {} has no context character, so the lineage root of {} cannot be found",
                        self.context_name(current),
                        self.context_name(context_id)
                    ))
                })?
            };
            match row.senpai_ctx {
                Some(senpai) => current = senpai,
                None => return Ok(row),
            }
        }
    }

    /// Point `context`'s senpai at `senpai`, or make it a top with `None`.
    /// Refuses a context with no context character, a senpai with none, a
    /// context that is its own senpai and a cycle. Who may move it, and the
    /// pending-ask rule, belong to the caller. The edge is the one mutable
    /// column of the dynamic layer, and the migration's note about it stands
    /// until it is set again.
    pub fn set_context_senpai(&self, context: ContextId, senpai: Option<ContextId>) -> KernelDbResult<()> {
        if self.get_context(context)?.is_none() {
            return Err(KernelDbError::NotFound(format!("context {}", context.short())));
        }
        self.require_context_character(context)?;
        if let Some(senpai) = senpai {
            if senpai == context {
                return Err(KernelDbError::Validation(format!(
                    "context {} cannot be its own senpai",
                    self.context_name(context)
                )));
            }
            if self.get_context(senpai)?.is_none() {
                return Err(KernelDbError::NotFound(format!("senpai context {}", senpai.short())));
            }
            let target = self.require_context_character(senpai)?;
            let mut visited = HashSet::new();
            let mut walk = target.senpai_ctx;
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
                walk = self.context_character(above)?.and_then(|r| r.senpai_ctx);
            }
        }
        self.conn.execute(
            "UPDATE context_characters SET senpai_ctx = ?1 WHERE context_id = ?2",
            params![senpai.as_ref().map(|s| s.as_bytes().to_vec()), blob_param(context.as_bytes())],
        )?;
        self.conn.execute(
            "DELETE FROM senpai_migration_report WHERE context_id = ?1",
            params![blob_param(context.as_bytes())],
        )?;
        Ok(())
    }

    /// What `context-characters-v1` did not carry over or could not resolve.
    pub fn context_character_migration_report(&self) -> KernelDbResult<Vec<ContextCharacterMigrationNote>> {
        let mut stmt = self.conn.prepare(
            "SELECT kind, context_id, note FROM context_character_migration_report ORDER BY report_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ContextCharacterMigrationNote {
                kind: r.get(0)?,
                context_id: read_opt_context_id(r, 1)?,
                note: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<SqliteResult<_>>()?)
    }
}

// ============================================================================
// Migration `context-characters-v1`
// ============================================================================

/// A context as the migration reads it.
#[derive(Debug, Clone)]
struct MigCtx {
    id: ContextId,
    label: Option<String>,
    context_type: String,
    created_at: i64,
    forked_from: Option<ContextId>,
    archived: bool,
    provider: Option<String>,
    model: Option<String>,
    cast_id: Option<CastId>,
    played_by: Option<PrincipalId>,
    senpai: Option<ContextId>,
}

impl MigCtx {
    fn name(&self) -> String {
        self.label.as_ref().map_or_else(|| self.id.short(), |l| format!("{l} ({})", self.id.short()))
    }

    fn is_judge_shadow(&self) -> bool {
        self.context_type == "judge" && self.played_by.is_none() && self.forked_from.is_some()
    }
}

/// The `llm_defaults` row as the migration reads it, where the table still
/// exists.
struct OldLlmDefaults {
    default_backend: String,
    default_model: String,
    max_tokens: Option<i64>,
    temperature: Option<f64>,
    top_p: Option<f64>,
    effort: Option<String>,
    thinking_budget: Option<i64>,
    thinking_style: Option<String>,
}

struct MigCharacter {
    root: bool,
    handoff_ctx: Option<ContextId>,
    root_ctx: Option<ContextId>,
    default_cast: Option<CastId>,
}

/// The catalogs the old model resolver read, as they stand at migration.
struct Catalog {
    casts: HashMap<CastId, String>,
    slots: HashMap<CastId, Vec<CastSlotRow>>,
    backend_by_name: HashMap<String, BackendId>,
    models: HashMap<(BackendId, String), BackendModelRow>,
    defaults: Option<OldLlmDefaults>,
}

impl Catalog {
    fn load(conn: &Connection) -> KernelDbResult<Self> {
        let casts = {
            let mut stmt = conn.prepare("SELECT cast_id, label FROM casts")?;
            stmt.query_map([], |r| Ok((read_cast_id_col(r, 0)?, r.get::<_, String>(1)?)))?
                .collect::<SqliteResult<HashMap<_, _>>>()?
        };
        let mut slots: HashMap<CastId, Vec<CastSlotRow>> = HashMap::new();
        {
            let mut stmt = conn.prepare(&format!("SELECT {CAST_SLOT_COLS} FROM cast_slots ORDER BY cast_id, role"))?;
            for slot in stmt.query_map([], row_to_cast_slot)? {
                let slot = slot?;
                slots.entry(slot.cast_id).or_default().push(slot);
            }
        }
        let backend_by_name = {
            let mut stmt = conn.prepare("SELECT name, backend_id FROM backends")?;
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, read_backend_id(r, 1)?)))?
                .collect::<SqliteResult<HashMap<_, _>>>()?
        };
        let mut models = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT backend_id, model_id, context_window, extra, max_tokens, temperature, top_p,
                        effort, thinking_budget, thinking_style FROM backend_models",
            )?;
            for row in stmt.query_map([], |r| {
                Ok(BackendModelRow {
                    backend_id: read_backend_id(r, 0)?,
                    model_id: r.get(1)?,
                    context_window: r.get(2)?,
                    extra: r.get(3)?,
                    tunables: ModelTunablesRow {
                        max_tokens: r.get(4)?,
                        temperature: r.get(5)?,
                        top_p: r.get(6)?,
                        effort: r.get(7)?,
                        thinking_budget: r.get(8)?,
                        thinking_style: r.get(9)?,
                    },
                })
            })? {
                let row = row?;
                models.insert((row.backend_id, row.model_id.clone()), row);
            }
        }
        let defaults = if KernelDb::has_table(conn, "llm_defaults")? {
            conn
                .query_row(
                    "SELECT default_backend, default_model, max_tokens, temperature, top_p, effort,
                            thinking_budget, thinking_style FROM llm_defaults WHERE id = 1",
                    [],
                    |r| {
                        Ok(OldLlmDefaults {
                            default_backend: r.get(0)?,
                            default_model: r.get(1)?,
                            max_tokens: r.get(2)?,
                            temperature: r.get(3)?,
                            top_p: r.get(4)?,
                            effort: r.get(5)?,
                            thinking_budget: r.get(6)?,
                            thinking_style: r.get(7)?,
                        })
                    },
                )
                .optional()?
        } else {
            None
        };
        Ok(Self { casts, slots, backend_by_name, models, defaults })
    }

    /// The `llm_defaults` tunable floor, empty when the row is absent.
    fn floor(&self) -> ModelTunablesRow {
        self.defaults
            .as_ref()
            .map(|d| ModelTunablesRow {
                max_tokens: d.max_tokens,
                temperature: d.temperature,
                top_p: d.top_p,
                effort: d.effort.clone(),
                thinking_budget: d.thinking_budget,
                thinking_style: d.thinking_style.clone(),
            })
            .unwrap_or_default()
    }

    /// A model's slot values as the old resolver answered: `own` over the
    /// model's row over the `llm_defaults` floor, and 64000 when none sets
    /// `max_tokens`. `pinned` counts the 64000 cases.
    fn slot_values(&self, role: &str, backend_id: BackendId, model: &str, own: &ModelTunablesRow, pinned: &mut u32) -> SlotValues {
        let row = self.models.get(&(backend_id, model.to_string()));
        let tunables = over(&over(own, &row.map(|m| m.tunables.clone()).unwrap_or_default()), &self.floor());
        let max_tokens = match tunables.max_tokens {
            None if names_council_server(role) => None,
            Some(n) => Some(n),
            None => {
                *pinned += 1;
                Some(UNSET_MAX_TOKENS)
            }
        };
        SlotValues::new(backend_id, model, tunables, max_tokens, row.and_then(|m| m.context_window))
    }
}

fn read_cast_id_col(row: &rusqlite::Row<'_>, idx: usize) -> SqliteResult<CastId> {
    let bytes: Vec<u8> = row.get(idx)?;
    CastId::try_from_slice(&bytes).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Blob, "invalid CastId bytes".into())
    })
}

/// A note for `context_character_migration_report`.
struct Note {
    kind: &'static str,
    context: Option<ContextId>,
    text: String,
}

/// The context character the migration will write for one context.
#[derive(Debug, Clone)]
struct Planned {
    principal: PrincipalId,
    senpai: Option<ContextId>,
}

impl KernelDb {
    /// Write a context character and slots for each context that has a
    /// performer (`docs/senpai-kohai.md`, "Migration" items 2 to 6).
    ///
    /// Runs once, in one transaction, and writes the old columns' answers into
    /// the dynamic layer; it never invents a value. A judge shadow's character
    /// is the one exception: the old row had none, the new model requires it,
    /// and it is the character of the context the shadow judges. Slots are what
    /// the old resolver (override, the context's or its performer's cast,
    /// `llm_defaults`) answered for the context: `max_tokens` is the slot's, the
    /// model row's, `llm_defaults`' or 64000. A live context the migration
    /// cannot place refuses the whole migration, naming it; an archived one is
    /// left without a context character and listed.
    pub(super) fn migrate_context_characters(conn: &Connection) -> KernelDbResult<()> {
        let done: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)",
            [Self::CONTEXT_CHARACTERS_MIGRATION],
            |row| row.get(0),
        )?;
        if done {
            return Ok(());
        }
        let tx = conn.unchecked_transaction()?;

        let mut ordered: Vec<MigCtx> = {
            let mut stmt = tx.prepare(
                "SELECT context_id, label, context_type, created_at, forked_from,
                        archived_at, provider, model, cast_id, played_by, senpai_ctx
                 FROM contexts",
            )?;
            stmt.query_map([], |r| {
                Ok(MigCtx {
                    id: read_context_id(r, 0)?,
                    label: r.get(1)?,
                    context_type: r.get(2)?,
                    created_at: r.get(3)?,
                    forked_from: read_opt_context_id(r, 4)?,
                    archived: r.get::<_, Option<i64>>(5)?.is_some(),
                    provider: r.get(6)?,
                    model: r.get(7)?,
                    cast_id: read_opt_cast_id(r, 8)?,
                    played_by: read_opt_principal_id(r, 9)?,
                    senpai: read_opt_context_id(r, 10)?,
                })
            })?
            .collect::<SqliteResult<_>>()?
        };
        ordered.sort_by_key(|c| (c.created_at, c.id));
        let by_id: HashMap<ContextId, MigCtx> = ordered.iter().map(|c| (c.id, c.clone())).collect();
        let name_of = |id: ContextId| by_id.get(&id).map_or_else(|| id.short(), MigCtx::name);

        let characters: HashMap<PrincipalId, MigCharacter> = {
            let mut stmt = tx.prepare("SELECT principal_id, root, handoff_ctx, root_ctx, default_cast_id FROM characters")?;
            stmt.query_map([], |r| {
                Ok((
                    read_principal_id(r, 0)?,
                    MigCharacter {
                        root: r.get(1)?,
                        handoff_ctx: read_opt_context_id(r, 2)?,
                        root_ctx: read_opt_context_id(r, 3)?,
                        default_cast: read_opt_cast_id(r, 4)?,
                    },
                ))
            })?
            .collect::<SqliteResult<_>>()?
        };
        let handoff: HashSet<ContextId> = characters.values().filter_map(|c| c.handoff_ctx).collect();
        let root_console_of: HashMap<ContextId, PrincipalId> = characters
            .iter()
            .filter_map(|(p, c)| c.root_ctx.map(|ctx| (ctx, *p)))
            .collect();

        let mut notes: Vec<Note> = Vec::new();
        let mut refused: Vec<String> = Vec::new();
        // Contexts the migration will not map: archived ones are listed, live ones refuse.
        let mut unmapped: Vec<(ContextId, String)> = Vec::new();

        // Item 2: who performs each context. A performer is the stored
        // `played_by`; a handoff log gets none.
        let mut planned: HashMap<ContextId, Planned> = HashMap::new();
        for c in &ordered {
            if handoff.contains(&c.id) {
                continue;
            }
            if let Some(principal) = c.played_by {
                planned.insert(c.id, Planned { principal, senpai: c.senpai });
            }
        }
        // A judge shadow is the character of the context it judges, answering to it.
        for c in ordered.iter().filter(|c| !handoff.contains(&c.id) && c.is_judge_shadow()) {
            let source = c.forked_from.expect("a judge shadow has a source");
            match planned.get(&source) {
                Some(judged) if by_id.get(&source).is_some_and(|s| !s.is_judge_shadow()) => {
                    let principal = judged.principal;
                    planned.insert(c.id, Planned { principal, senpai: Some(source) });
                }
                _ => unmapped.push((
                    c.id,
                    format!("a judge shadow of {}, which has no context character to share", name_of(source)),
                )),
            }
        }

        // Item 3: edges.
        let mut ids: Vec<ContextId> = ordered.iter().map(|c| c.id).filter(|id| planned.contains_key(id)).collect();
        ids.sort_by_key(|id| (by_id[id].created_at, *id));
        for id in &ids {
            let c = &by_id[id];
            let principal = planned[id].principal;
            let mut senpai = planned[id].senpai;
            if senpai.is_some() && root_console_of.get(id) == Some(&principal) {
                notes.push(Note {
                    kind: "dropped",
                    context: Some(*id),
                    text: format!(
                        "a root console has no senpai: senpai_ctx {} is not carried",
                        name_of(senpai.expect("checked"))
                    ),
                });
                senpai = None;
            }
            if let Some(target) = senpai
                && !planned.contains_key(&target)
            {
                if c.archived {
                    notes.push(Note {
                        kind: "dropped",
                        context: Some(*id),
                        text: format!(
                            "senpai_ctx {} has no context character: the archived context has no senpai",
                            name_of(target)
                        ),
                    });
                    senpai = None;
                } else {
                    refused.push(format!("{}: its senpai {} has no context character", c.name(), name_of(target)));
                }
            }
            planned.get_mut(id).expect("planned").senpai = senpai;
        }
        // A cycle: a live context in it refuses; a cycle of archived contexts is
        // broken at its newest member.
        {
            let mut done: HashSet<ContextId> = HashSet::new();
            for start in &ids {
                if done.contains(start) {
                    continue;
                }
                let mut path: Vec<ContextId> = Vec::new();
                let mut at = Some(*start);
                while let Some(id) = at {
                    if done.contains(&id) {
                        break;
                    }
                    if let Some(pos) = path.iter().position(|p| *p == id) {
                        let ring: Vec<ContextId> = path[pos..].to_vec();
                        let names: Vec<String> = ring.iter().map(|c| name_of(*c)).collect();
                        let live: Vec<&ContextId> = ring.iter().filter(|c| !by_id[*c].archived).collect();
                        if live.is_empty() {
                            let newest = *ring.iter().max_by_key(|c| (by_id[*c].created_at, **c)).expect("a ring");
                            planned.get_mut(&newest).expect("planned").senpai = None;
                            notes.push(Note {
                                kind: "dropped",
                                context: Some(newest),
                                text: format!(
                                    "its senpai chain loops through {}: the archived context has no senpai",
                                    names.join(" -> ")
                                ),
                            });
                        } else {
                            for c in live {
                                refused.push(format!("{}: its senpai chain loops through {}", name_of(*c), names.join(" -> ")));
                            }
                        }
                        break;
                    }
                    path.push(id);
                    at = planned.get(&id).and_then(|p| p.senpai).filter(|s| planned.contains_key(s));
                }
                done.extend(path);
            }
        }
        // A live context with no senpai must be its character's root console.
        for id in &ids {
            let c = &by_id[id];
            let p = &planned[id];
            if !c.archived && p.senpai.is_none() && root_console_of.get(id) != Some(&p.principal) {
                refused.push(format!(
                    "{}: it has no senpai and is not the root console of its character",
                    c.name()
                ));
            }
        }
        for (ctx, why) in &unmapped {
            if !by_id[ctx].archived {
                refused.push(format!("{}: {why}", by_id[ctx].name()));
            }
        }
        if !refused.is_empty() {
            refused.sort();
            refused.dedup();
            return Err(KernelDbError::Validation(format!(
                "the context character migration cannot place {} live context{} and will not guess; nothing was changed. \
                 Fix each link (forked_from, senpai_ctx, root_ctx) with the previous kernel, then start again:\n  {}",
                refused.len(),
                if refused.len() == 1 { "" } else { "s" },
                refused.join("\n  ")
            )));
        }
        for (ctx, why) in &unmapped {
            notes.push(Note {
                kind: "unmapped",
                context: Some(*ctx),
                text: format!("archived and left without a context character: {why}"),
            });
        }
        // A context with no performer that is no judge shadow gets none.
        let unmapped_ids: HashSet<ContextId> = unmapped.iter().map(|(id, _)| *id).collect();
        for c in &ordered {
            if planned.contains_key(&c.id) || unmapped_ids.contains(&c.id) {
                continue;
            }
            if let Some(senpai) = c.senpai {
                notes.push(Note {
                    kind: "unmapped",
                    context: Some(c.id),
                    text: format!("no performer: left without a context character; senpai_ctx {} is not carried", name_of(senpai)),
                });
            }
            if c.cast_id.is_some() || c.model.is_some() {
                notes.push(Note {
                    kind: "dropped",
                    context: Some(c.id),
                    text: "has no context character, so its cast and model override run nothing and are not carried".into(),
                });
            }
        }

        // Item 4: slots.
        let catalog = Catalog::load(&tx)?;
        let effective_cast = |c: &MigCtx| -> Option<CastId> {
            let id = match c.cast_id {
                Some(id) => id,
                None => characters.get(&c.played_by?)?.default_cast?,
            };
            catalog.casts.contains_key(&id).then_some(id)
        };
        let mut pinned = 0u32;
        let mut resolved: Vec<(ContextId, BTreeMap<String, SlotValues>)> = Vec::new();
        for id in &ids {
            let c = &by_id[id];
            let mut slots: BTreeMap<String, SlotValues> = BTreeMap::new();
            if let Some(cast) = effective_cast(c) {
                for slot in catalog.slots.get(&cast).into_iter().flatten() {
                    slots.insert(
                        slot.role.clone(),
                        catalog.slot_values(&slot.role, slot.backend_id, &slot.model, &slot_tunables(slot), &mut pinned),
                    );
                }
            }
            let own_role = c.context_type.clone();
            let mut unresolved = None;
            if let Some(model) = &c.model {
                let backend = c.provider.clone().or_else(|| catalog.defaults.as_ref().map(|d| d.default_backend.clone()));
                match backend.as_deref().map(|b| (b, catalog.backend_by_name.get(b))) {
                    Some((_, Some(backend))) => {
                        slots.insert(
                            own_role.clone(),
                            catalog.slot_values(&own_role, *backend, model, &ModelTunablesRow::default(), &mut pinned),
                        );
                    }
                    Some((name, None)) => unresolved = Some(format!("its override names backend {name}, which has no row")),
                    None => unresolved = Some("its override names no backend and llm_defaults has none".into()),
                }
            } else if own_role == "gate" || !slots.contains_key(&own_role) {
                match &catalog.defaults {
                    None => unresolved = Some("it has no override, no cast slot for its type and no llm_defaults row".into()),
                    Some(d) => match catalog.backend_by_name.get(&d.default_backend) {
                        None => {
                            unresolved = Some(format!("llm_defaults names backend {}, which has no row", d.default_backend))
                        }
                        Some(backend) => {
                            slots.insert(
                                own_role.clone(),
                                catalog.slot_values(&own_role, *backend, &d.default_model, &ModelTunablesRow::default(), &mut pinned),
                            );
                        }
                    },
                }
            }
            if let Some(why) = unresolved {
                notes.push(Note {
                    kind: "no_slot",
                    context: Some(*id),
                    text: format!("the old resolver had no model for type {}: {why}", c.context_type),
                });
            }
            // A live context that runs turns needs a gate and a distill slot; the
            // migration cannot read gate.toml, so it lists the ones without.
            let runs_turns = !c.archived
                && !c.is_judge_shadow()
                && !characters.get(&planned[id].principal).is_some_and(|ch| ch.root);
            if runs_turns {
                for (role, kind) in [("gate", "no_gate_slot"), ("distill", "no_distill_slot")] {
                    if !slots.contains_key(role) {
                        notes.push(Note {
                            kind,
                            context: Some(*id),
                            text: format!("live context {} has no {role} slot", c.name()),
                        });
                    }
                }
            }
            resolved.push((*id, slots));
        }

        // Writes.
        for id in &ids {
            let p = &planned[id];
            insert_context_character(&tx, *id, p.principal, p.senpai, None, by_id[id].created_at)?;
        }
        for (id, slots) in &resolved {
            for (role, slot) in slots {
                insert_slot(&tx, *id, role, slot)?;
            }
        }
        let recorded_at = now_millis();
        for note in &notes {
            match note.kind {
                "dropped" => info!(kind = note.kind, context = ?note.context, "context-characters-v1: {}", note.text),
                _ => warn!(kind = note.kind, context = ?note.context, "context-characters-v1: {}", note.text),
            }
            tx.execute(
                "INSERT INTO context_character_migration_report (kind, context_id, note, recorded_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    note.kind,
                    note.context.as_ref().map(|c| c.as_bytes().to_vec()),
                    note.text,
                    recorded_at,
                ],
            )?;
        }
        tx.execute("INSERT INTO kernel_migrations (name) VALUES (?1)", [Self::CONTEXT_CHARACTERS_MIGRATION])?;
        tx.commit()?;
        info!(
            contexts = ids.len(),
            max_tokens_pinned = pinned,
            reported = notes.len(),
            "gave every performed context a context character"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests;
