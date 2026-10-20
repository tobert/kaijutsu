//! Character instances: the dynamic layer of the senpai/kohai model.
//!
//! A character instance is one run of one character. Its bindings are the
//! settings it runs on: a copy of its cast's slots, with every parameter the
//! slot leaves unset filled from the model's `backend_models` row, written once
//! per `seq` and never changed. A context belongs to at most one instance and
//! runs on one binding `seq`, both fixed when the context is made.
//! `docs/senpai-kohai.md`, "The model" and "Where a model comes from".

use std::collections::{BTreeMap, HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension, Result as SqliteResult, params};
use tracing::{info, warn};

use kaijutsu_types::{BackendId, CastId, CharacterInstanceId, ContextId, PrincipalId};

use super::{
    BackendModelRow, CAST_SLOT_COLS, CastRow, CastSlotRow, KernelDb, KernelDbError, KernelDbResult,
    ModelTunablesRow, blob_param, now_millis, read_backend_id, read_context_id,
    read_opt_cast_id, read_opt_context_id, read_opt_principal_id, read_principal_id, row_to_cast_slot,
};

/// One run of one character.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CharacterInstanceRow {
    pub instance_id: CharacterInstanceId,
    pub principal_id: PrincipalId,
    pub senpai_instance: Option<CharacterInstanceId>,
    pub started_at: i64,
    pub created_by: PrincipalId,
}

/// The settings an instance runs on from one conversation onward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceBindingRow {
    pub instance_id: CharacterInstanceId,
    pub seq: i64,
    pub conversation_ctx: ContextId,
    pub bound_at: i64,
    pub cast_label: Option<String>,
    pub persona: Option<String>,
}

/// A copy of one cast slot, tunables filled from the model's row.
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceSlotRow {
    pub instance_id: CharacterInstanceId,
    pub seq: i64,
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

/// One line of `instance_migration_report`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceMigrationNote {
    pub kind: String,
    pub context_id: Option<ContextId>,
    pub instance_id: Option<CharacterInstanceId>,
    pub note: String,
}

/// `max_tokens` the old model path sent when the slot, the model row and
/// `llm_defaults` all left it unset (`LlmRegistry::max_output_tokens`).
const UNSET_MAX_TOKENS: i64 = 64_000;

/// Roles whose slot names a council server, not a provider request. The
/// served model carries no `max_tokens`, so these slots may leave it unset.
fn names_council_server(role: &str) -> bool {
    matches!(role, "gate" | "judge")
}

fn read_instance_id(row: &rusqlite::Row<'_>, idx: usize) -> SqliteResult<CharacterInstanceId> {
    let bytes: Vec<u8> = row.get(idx)?;
    CharacterInstanceId::try_from_slice(&bytes).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Blob, "invalid CharacterInstanceId bytes".into())
    })
}

pub(super) fn read_opt_instance_id(row: &rusqlite::Row<'_>, idx: usize) -> SqliteResult<Option<CharacterInstanceId>> {
    let bytes: Option<Vec<u8>> = row.get(idx)?;
    match bytes {
        Some(b) => CharacterInstanceId::try_from_slice(&b).map(Some).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Blob, "invalid CharacterInstanceId bytes".into())
        }),
        None => Ok(None),
    }
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

fn insert_instance(
    conn: &Connection,
    instance_id: CharacterInstanceId,
    principal_id: PrincipalId,
    senpai: Option<CharacterInstanceId>,
    started_at: i64,
    created_by: PrincipalId,
) -> KernelDbResult<()> {
    conn.execute(
        "INSERT INTO character_instances (instance_id, principal_id, senpai_instance, started_at, created_by)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            blob_param(instance_id.as_bytes()),
            blob_param(principal_id.as_bytes()),
            senpai.as_ref().map(|s| s.as_bytes().to_vec()),
            started_at,
            blob_param(created_by.as_bytes()),
        ],
    )?;
    Ok(())
}

fn insert_binding(
    conn: &Connection,
    instance_id: CharacterInstanceId,
    seq: i64,
    conversation_ctx: ContextId,
    bound_at: i64,
    cast_label: Option<&str>,
    persona: Option<&str>,
) -> KernelDbResult<()> {
    conn.execute(
        "INSERT INTO instance_bindings (instance_id, seq, conversation_ctx, bound_at, cast_label, persona)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            blob_param(instance_id.as_bytes()),
            seq,
            blob_param(conversation_ctx.as_bytes()),
            bound_at,
            cast_label,
            persona,
        ],
    )?;
    Ok(())
}

fn insert_slot(conn: &Connection, instance_id: CharacterInstanceId, seq: i64, role: &str, slot: &SlotValues) -> KernelDbResult<()> {
    conn.execute(
        "INSERT INTO instance_slots (instance_id, seq, role, backend_id, model, max_tokens, context_window,
                                     temperature, top_p, effort, thinking_budget, thinking_style)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            blob_param(instance_id.as_bytes()),
            seq,
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

/// Fix `context_id` to `(instance_id, seq)`. Refuses a context that is missing
/// or already belongs to an instance.
fn assign_context(conn: &Connection, context_id: ContextId, instance_id: CharacterInstanceId, seq: i64) -> KernelDbResult<()> {
    let changed = conn.execute(
        "UPDATE contexts SET instance_id = ?1, binding_seq = ?2 WHERE context_id = ?3 AND instance_id IS NULL",
        params![blob_param(instance_id.as_bytes()), seq, blob_param(context_id.as_bytes())],
    )?;
    if changed == 1 {
        return Ok(());
    }
    let existing: Option<Option<Vec<u8>>> = conn
        .query_row(
            "SELECT instance_id FROM contexts WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes())],
            |r| r.get(0),
        )
        .optional()?;
    Err(match existing {
        None => KernelDbError::NotFound(format!("context {}", context_id.short())),
        Some(_) => KernelDbError::Validation(format!(
            "context {} already belongs to a character instance, which is fixed when the context is made",
            context_id.short()
        )),
    })
}

/// Make `child` join the instance and binding `seq` its `source` runs on. A
/// source with no instance gives the child none.
pub(super) fn join_source_instance(conn: &Connection, source: ContextId, child: ContextId) -> KernelDbResult<()> {
    let row: Option<(Option<Vec<u8>>, Option<i64>)> = conn
        .query_row(
            "SELECT instance_id, binding_seq FROM contexts WHERE context_id = ?1",
            params![blob_param(source.as_bytes())],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match row {
        None => Err(KernelDbError::NotFound(format!("context {}", source.short()))),
        Some((None, None)) => Ok(()),
        Some((Some(bytes), Some(seq))) => {
            let instance_id = CharacterInstanceId::try_from_slice(&bytes)
                .ok_or_else(|| KernelDbError::Validation(format!("context {} has an invalid instance id", source.short())))?;
            assign_context(conn, child, instance_id, seq)
        }
        Some(_) => Err(KernelDbError::Validation(format!(
            "context {} has only one of instance_id and binding_seq",
            source.short()
        ))),
    }
}

fn row_to_slot(row: &rusqlite::Row<'_>) -> SqliteResult<InstanceSlotRow> {
    Ok(InstanceSlotRow {
        instance_id: read_instance_id(row, 0)?,
        seq: row.get(1)?,
        role: row.get(2)?,
        backend_id: read_backend_id(row, 3)?,
        model: row.get(4)?,
        max_tokens: row.get(5)?,
        context_window: row.get(6)?,
        temperature: row.get(7)?,
        top_p: row.get(8)?,
        effort: row.get(9)?,
        thinking_budget: row.get(10)?,
        thinking_style: row.get(11)?,
    })
}

const SLOT_COLS: &str = "instance_id, seq, role, backend_id, model, max_tokens, context_window, \
                         temperature, top_p, effort, thinking_budget, thinking_style";

impl KernelDb {
    pub(super) const INSTANCES_MIGRATION: &'static str = "instances-v1";

    /// Triggers that keep the dynamic layer append-only. They name columns
    /// that exist only after the additive ALTERs, so they are created here
    /// rather than in `SCHEMA`.
    pub(super) fn ensure_instance_guards(conn: &Connection) -> KernelDbResult<()> {
        conn.execute_batch(
            "CREATE TRIGGER IF NOT EXISTS instance_bindings_never_changed_u
                 BEFORE UPDATE ON instance_bindings
                 BEGIN SELECT RAISE(ABORT, 'instance_bindings rows are never changed'); END;
             CREATE TRIGGER IF NOT EXISTS instance_bindings_never_changed_d
                 BEFORE DELETE ON instance_bindings
                 BEGIN SELECT RAISE(ABORT, 'instance_bindings rows are never changed'); END;
             CREATE TRIGGER IF NOT EXISTS instance_slots_never_changed_u
                 BEFORE UPDATE ON instance_slots
                 BEGIN SELECT RAISE(ABORT, 'instance_slots rows are never changed'); END;
             CREATE TRIGGER IF NOT EXISTS instance_slots_never_changed_d
                 BEFORE DELETE ON instance_slots
                 BEGIN SELECT RAISE(ABORT, 'instance_slots rows are never changed'); END;
             CREATE TRIGGER IF NOT EXISTS contexts_instance_set_once
                 BEFORE UPDATE OF instance_id, binding_seq ON contexts
                 WHEN (OLD.instance_id IS NOT NULL AND NEW.instance_id IS NOT OLD.instance_id)
                   OR (OLD.binding_seq IS NOT NULL AND NEW.binding_seq IS NOT OLD.binding_seq)
                 BEGIN SELECT RAISE(ABORT, 'contexts.instance_id and contexts.binding_seq are set once'); END;
             CREATE INDEX IF NOT EXISTS idx_contexts_instance
                 ON contexts(instance_id) WHERE instance_id IS NOT NULL;",
        )?;
        Ok(())
    }

    /// Copy `cast`'s slots into binding `(instance_id, seq)`. A parameter the
    /// slot leaves unset takes the model's `backend_models` value; nothing
    /// else supplies one. A slot with no `max_tokens` in either place refuses, and so does
    /// `thinking_style=budget` with no `thinking_budget` below `max_tokens`.
    fn copy_cast_slots(&self, instance_id: CharacterInstanceId, seq: i64, cast: &CastRow) -> KernelDbResult<()> {
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
                    "cast {} slot {} runs model {}, which has no max_tokens in the slot or in its backend_models row; \
                     every request carries max_tokens, so set it on one of them",
                    cast.label, slot.role, slot.model
                )));
            }
            if !names_council_server(&slot.role) && tunables.thinking_style.as_deref() == Some("budget") {
                let Some(budget) = tunables.thinking_budget else {
                    return Err(KernelDbError::Validation(format!(
                        "cast {} slot {} runs model {} with thinking_style=budget and no thinking_budget in the slot or in its \
                         backend_models row; set one of them",
                        cast.label, slot.role, slot.model
                    )));
                };
                if let Some(max) = max_tokens
                    && budget >= max
                {
                    return Err(KernelDbError::Validation(format!(
                        "cast {} slot {} runs model {} with thinking_budget ({budget}) not below max_tokens ({max}); \
                         lower the budget or raise max_tokens",
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
            insert_slot(&self.conn, instance_id, seq, &slot.role, &values)?;
        }
        Ok(())
    }

    fn write_instance_binding(
        &self,
        instance_id: CharacterInstanceId,
        seq: i64,
        conversation_ctx: ContextId,
        cast: Option<CastId>,
        persona: Option<&str>,
    ) -> KernelDbResult<InstanceBindingRow> {
        let cast = match cast {
            Some(id) => Some(self.get_cast(id)?.ok_or_else(|| KernelDbError::NotFound(format!("cast {}", id.short())))?),
            None => None,
        };
        let bound_at = now_millis();
        insert_binding(
            &self.conn,
            instance_id,
            seq,
            conversation_ctx,
            bound_at,
            cast.as_ref().map(|c| c.label.as_str()),
            persona,
        )?;
        if let Some(cast) = &cast {
            self.copy_cast_slots(instance_id, seq, cast)?;
        }
        assign_context(&self.conn, conversation_ctx, instance_id, seq)?;
        Ok(InstanceBindingRow {
            instance_id,
            seq,
            conversation_ctx,
            bound_at,
            cast_label: cast.map(|c| c.label),
            persona: persona.map(str::to_string),
        })
    }

    /// Start an instance of `principal_id` whose conversation is
    /// `conversation_ctx`, writing binding `seq` 1 from `cast` (none = a
    /// binding with no slots) and fixing the conversation to it. Refuses, and
    /// writes nothing, when a slot has no `max_tokens` or the context already
    /// belongs to an instance.
    pub fn start_instance(
        &self,
        principal_id: PrincipalId,
        senpai: Option<CharacterInstanceId>,
        cast: Option<CastId>,
        persona: Option<&str>,
        conversation_ctx: ContextId,
        created_by: PrincipalId,
    ) -> KernelDbResult<CharacterInstanceRow> {
        self.atomically(|db| {
            let instance_id = CharacterInstanceId::new();
            let started_at = now_millis();
            insert_instance(&db.conn, instance_id, principal_id, senpai, started_at, created_by)?;
            db.write_instance_binding(instance_id, 1, conversation_ctx, cast, persona)?;
            Ok(CharacterInstanceRow { instance_id, principal_id, senpai_instance: senpai, started_at, created_by })
        })
    }

    /// Start an instance of `principal_id` on the cast it names today
    /// (`characters.default_cast_id`; none gives a binding with no slots).
    /// The persona is unset until characters carry one.
    pub fn start_character_instance(
        &self,
        principal_id: PrincipalId,
        senpai: Option<CharacterInstanceId>,
        conversation_ctx: ContextId,
        created_by: PrincipalId,
    ) -> KernelDbResult<CharacterInstanceRow> {
        let cast = self.character_cast(principal_id)?.map(|cast| cast.cast_id);
        self.start_instance(principal_id, senpai, cast, None, conversation_ctx, created_by)
    }

    /// Run `f` atomically: its own transaction when none is open, else a
    /// savepoint inside the caller's, so a refusal inside `f` leaves none of
    /// its writes behind.
    fn atomically<T>(&self, f: impl FnOnce(&Self) -> KernelDbResult<T>) -> KernelDbResult<T> {
        if self.conn.is_autocommit() {
            return self.in_transaction(f);
        }
        self.conn.execute_batch("SAVEPOINT instance_write")?;
        match f(self) {
            Ok(out) => {
                self.conn.execute_batch("RELEASE SAVEPOINT instance_write")?;
                Ok(out)
            }
            Err(error) => {
                self.conn.execute_batch("ROLLBACK TO SAVEPOINT instance_write; RELEASE SAVEPOINT instance_write")?;
                Err(error)
            }
        }
    }

    /// Append the next binding to `instance_id`, copied fresh from `cast`, with
    /// `conversation_ctx` as the instance's new conversation. Earlier bindings
    /// are not touched.
    pub fn append_binding(
        &self,
        instance_id: CharacterInstanceId,
        cast: Option<CastId>,
        persona: Option<&str>,
        conversation_ctx: ContextId,
    ) -> KernelDbResult<InstanceBindingRow> {
        self.atomically(|db| {
            if db.get_instance(instance_id)?.is_none() {
                return Err(KernelDbError::NotFound(format!("character instance {}", instance_id.short())));
            }
            let seq: i64 = db.conn.query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM instance_bindings WHERE instance_id = ?1",
                params![blob_param(instance_id.as_bytes())],
                |r| r.get(0),
            )?;
            db.write_instance_binding(instance_id, seq, conversation_ctx, cast, persona)
        })
    }

    /// Fix `context_id` to binding `(instance_id, seq)`, once. A fork joins
    /// its source's instance this way.
    pub fn join_instance(&self, context_id: ContextId, instance_id: CharacterInstanceId, seq: i64) -> KernelDbResult<()> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM instance_bindings WHERE instance_id = ?1 AND seq = ?2)",
            params![blob_param(instance_id.as_bytes()), seq],
            |r| r.get(0),
        )?;
        if !exists {
            return Err(KernelDbError::NotFound(format!(
                "binding {seq} of character instance {}",
                instance_id.short()
            )));
        }
        assign_context(&self.conn, context_id, instance_id, seq)
    }

    /// The instance and binding `seq` a context runs on, or `None`.
    pub fn context_instance(&self, context_id: ContextId) -> KernelDbResult<Option<(CharacterInstanceId, i64)>> {
        let row = self
            .conn
            .query_row(
                "SELECT instance_id, binding_seq FROM contexts WHERE context_id = ?1",
                params![blob_param(context_id.as_bytes())],
                |r| Ok((read_opt_instance_id(r, 0)?, r.get::<_, Option<i64>>(1)?)),
            )
            .optional()?;
        match row {
            None => Err(KernelDbError::NotFound(format!("context {}", context_id.short()))),
            Some((Some(instance), Some(seq))) => Ok(Some((instance, seq))),
            Some((None, None)) => Ok(None),
            Some(_) => Err(KernelDbError::Validation(format!(
                "context {} has only one of instance_id and binding_seq",
                context_id.short()
            ))),
        }
    }

    /// The `conversation_ctx` of the instance's newest binding.
    pub fn instance_conversation(&self, instance_id: CharacterInstanceId) -> KernelDbResult<Option<ContextId>> {
        Ok(self
            .conn
            .query_row(
                "SELECT conversation_ctx FROM instance_bindings WHERE instance_id = ?1 ORDER BY seq DESC LIMIT 1",
                params![blob_param(instance_id.as_bytes())],
                |r| read_context_id(r, 0),
            )
            .optional()?)
    }

    /// The instance a context belongs to, or `None`.
    pub fn context_instance_row(&self, context_id: ContextId) -> KernelDbResult<Option<CharacterInstanceRow>> {
        match self.context_instance(context_id)? {
            Some((instance_id, _)) => Ok(Some(self.get_instance(instance_id)?.ok_or_else(|| {
                KernelDbError::NotFound(format!("character instance {} of context {}", instance_id.short(), context_id.short()))
            })?)),
            None => Ok(None),
        }
    }

    /// The instance a context belongs to; a context with none is refused,
    /// naming it.
    pub(crate) fn require_context_instance(&self, context_id: ContextId) -> KernelDbResult<CharacterInstanceRow> {
        self.context_instance_row(context_id)?.ok_or_else(|| {
            KernelDbError::Validation(format!(
                "context {} belongs to no character instance, so it has no senpai and routes nothing; \
                 a context gets its instance from `kj kohai create --as <character>`, or by forking one that has it",
                self.context_name(context_id)
            ))
        })
    }

    /// Every context that runs on `instance_id`, oldest first: its
    /// conversations, forks, judge shadows and rc-made contexts.
    pub fn instance_contexts(&self, instance_id: CharacterInstanceId) -> KernelDbResult<Vec<ContextId>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id FROM contexts WHERE instance_id = ?1 ORDER BY created_at, context_id",
        )?;
        let rows = stmt.query_map(params![blob_param(instance_id.as_bytes())], |r| read_context_id(r, 0))?;
        Ok(rows.collect::<SqliteResult<_>>()?)
    }

    /// `label (short-id)` of a context, or its short id when it has no row or label.
    pub fn context_name(&self, context_id: ContextId) -> String {
        match self.get_context(context_id) {
            Ok(Some(row)) => Self::context_display(&row),
            _ => context_id.short(),
        }
    }

    /// An instance named by its conversation: `label (short-id)`. An instance
    /// has no label of its own.
    pub fn instance_name(&self, instance_id: CharacterInstanceId) -> String {
        match self.instance_conversation(instance_id) {
            Ok(Some(conversation)) => self.context_name(conversation),
            _ => instance_id.short(),
        }
    }

    /// Whether the instance is live: its conversation, the `conversation_ctx`
    /// of its newest binding, is not archived. Archiving the conversation
    /// makes the instance inactive; promoting it makes the instance live again.
    pub fn instance_is_active(&self, instance_id: CharacterInstanceId) -> KernelDbResult<bool> {
        let conversation = self.instance_conversation(instance_id)?.ok_or_else(|| {
            KernelDbError::NotFound(format!("character instance {} has no binding", instance_id.short()))
        })?;
        let row = self.get_context(conversation)?.ok_or_else(|| {
            KernelDbError::NotFound(format!("conversation {} of character instance {}", conversation.short(), instance_id.short()))
        })?;
        Ok(!row.is_archived())
    }

    /// The live (active) instances of a character, oldest first.
    pub fn character_live_instances(&self, principal_id: PrincipalId) -> KernelDbResult<Vec<CharacterInstanceRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT instance_id, principal_id, senpai_instance, started_at, created_by
             FROM character_instances WHERE principal_id = ?1 ORDER BY started_at, instance_id",
        )?;
        let rows = stmt.query_map(params![blob_param(principal_id.as_bytes())], |r| {
            Ok(CharacterInstanceRow {
                instance_id: read_instance_id(r, 0)?,
                principal_id: read_principal_id(r, 1)?,
                senpai_instance: read_opt_instance_id(r, 2)?,
                started_at: r.get(3)?,
                created_by: read_principal_id(r, 4)?,
            })
        })?;
        let mut live = Vec::new();
        for row in rows.collect::<SqliteResult<Vec<_>>>()? {
            if self.instance_is_active(row.instance_id)? {
                live.push(row);
            }
        }
        Ok(live)
    }

    /// The instances whose senpai is `instance_id`, oldest first, active or not.
    pub fn senpai_edge_kohai(&self, instance_id: CharacterInstanceId) -> KernelDbResult<Vec<CharacterInstanceRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT instance_id, principal_id, senpai_instance, started_at, created_by
             FROM character_instances WHERE senpai_instance = ?1 ORDER BY started_at, instance_id",
        )?;
        let rows = stmt.query_map(params![blob_param(instance_id.as_bytes())], |r| {
            Ok(CharacterInstanceRow {
                instance_id: read_instance_id(r, 0)?,
                principal_id: read_principal_id(r, 1)?,
                senpai_instance: read_opt_instance_id(r, 2)?,
                started_at: r.get(3)?,
                created_by: read_principal_id(r, 4)?,
            })
        })?;
        Ok(rows.collect::<SqliteResult<_>>()?)
    }

    /// The instance at the top of `instance_id`'s senpai chain (the one with no
    /// senpai). A cycle or a senpai that has no row is corruption and refuses.
    pub fn lineage_top_instance(&self, instance_id: CharacterInstanceId) -> KernelDbResult<CharacterInstanceRow> {
        let mut current = instance_id;
        let mut visited = HashSet::new();
        loop {
            if !visited.insert(current) {
                return Err(KernelDbError::Validation(format!(
                    "instance {} is its own senpai ancestor; the senpai chain has a cycle",
                    self.instance_name(current)
                )));
            }
            let row = self.get_instance(current)?.ok_or_else(|| {
                KernelDbError::NotFound(format!("character instance {} is missing, so its lineage root cannot be found", current.short()))
            })?;
            match row.senpai_instance {
                Some(senpai) => current = senpai,
                None => return Ok(row),
            }
        }
    }

    /// Point `instance_id` at a new senpai instance, or make it a top with
    /// `None`. Refuses an instance that is its own senpai, a cycle, and a
    /// missing row. Who may move it, and the pending-ask rule, belong to the
    /// caller. The edge is the one mutable column of the dynamic layer.
    pub fn set_senpai_instance(&self, instance_id: CharacterInstanceId, senpai: Option<CharacterInstanceId>) -> KernelDbResult<()> {
        if self.get_instance(instance_id)?.is_none() {
            return Err(KernelDbError::NotFound(format!("character instance {} has no row to move", instance_id.short())));
        }
        if let Some(senpai) = senpai {
            if senpai == instance_id {
                return Err(KernelDbError::Validation(format!(
                    "instance {} cannot be its own senpai",
                    self.instance_name(instance_id)
                )));
            }
            let target = self.get_instance(senpai)?.ok_or_else(|| {
                KernelDbError::NotFound(format!("senpai instance {} has no row", senpai.short()))
            })?;
            let mut visited = HashSet::new();
            let mut walk = target.senpai_instance;
            while let Some(above) = walk {
                if above == instance_id {
                    return Err(KernelDbError::Validation(format!(
                        "making {} the senpai of {} would close a senpai cycle",
                        self.instance_name(senpai),
                        self.instance_name(instance_id)
                    )));
                }
                if !visited.insert(above) {
                    break; // an existing loop above; not ours to report here
                }
                walk = self.get_instance(above)?.and_then(|r| r.senpai_instance);
            }
        }
        self.conn.execute(
            "UPDATE character_instances SET senpai_instance = ?1 WHERE instance_id = ?2",
            params![senpai.as_ref().map(|s| s.as_bytes().to_vec()), blob_param(instance_id.as_bytes())],
        )?;
        Ok(())
    }

    /// The slot for `role` in binding `(instance_id, seq)`. A binding with no
    /// slot for a role gives that role no model.
    pub fn binding_slot(&self, instance_id: CharacterInstanceId, seq: i64, role: &str) -> KernelDbResult<Option<InstanceSlotRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {SLOT_COLS} FROM instance_slots WHERE instance_id = ?1 AND seq = ?2 AND role = ?3"),
                params![blob_param(instance_id.as_bytes()), seq, role],
                row_to_slot,
            )
            .optional()?)
    }

    /// Every slot of a binding, ordered by role.
    pub fn list_binding_slots(&self, instance_id: CharacterInstanceId, seq: i64) -> KernelDbResult<Vec<InstanceSlotRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SLOT_COLS} FROM instance_slots WHERE instance_id = ?1 AND seq = ?2 ORDER BY role"
        ))?;
        let rows = stmt.query_map(params![blob_param(instance_id.as_bytes()), seq], row_to_slot)?;
        Ok(rows.collect::<SqliteResult<_>>()?)
    }

    pub fn get_instance(&self, instance_id: CharacterInstanceId) -> KernelDbResult<Option<CharacterInstanceRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT instance_id, principal_id, senpai_instance, started_at, created_by
                 FROM character_instances WHERE instance_id = ?1",
                params![blob_param(instance_id.as_bytes())],
                |r| {
                    Ok(CharacterInstanceRow {
                        instance_id: read_instance_id(r, 0)?,
                        principal_id: read_principal_id(r, 1)?,
                        senpai_instance: read_opt_instance_id(r, 2)?,
                        started_at: r.get(3)?,
                        created_by: read_principal_id(r, 4)?,
                    })
                },
            )
            .optional()?)
    }

    /// An instance's bindings, oldest first.
    pub fn get_binding(&self, instance_id: CharacterInstanceId, seq: i64) -> KernelDbResult<Option<InstanceBindingRow>> {
        Ok(self.list_instance_bindings(instance_id)?.into_iter().find(|b| b.seq == seq))
    }

    pub fn list_instance_bindings(&self, instance_id: CharacterInstanceId) -> KernelDbResult<Vec<InstanceBindingRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT instance_id, seq, conversation_ctx, bound_at, cast_label, persona
             FROM instance_bindings WHERE instance_id = ?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![blob_param(instance_id.as_bytes())], |r| {
            Ok(InstanceBindingRow {
                instance_id: read_instance_id(r, 0)?,
                seq: r.get(1)?,
                conversation_ctx: read_context_id(r, 2)?,
                bound_at: r.get(3)?,
                cast_label: r.get(4)?,
                persona: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<SqliteResult<_>>()?)
    }

    /// What `instances-v1` did not carry over or could not resolve.
    pub fn instance_migration_report(&self) -> KernelDbResult<Vec<InstanceMigrationNote>> {
        let mut stmt = self.conn.prepare(
            "SELECT kind, context_id, instance_id, note FROM instance_migration_report ORDER BY report_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(InstanceMigrationNote {
                kind: r.get(0)?,
                context_id: read_opt_context_id(r, 1)?,
                instance_id: read_opt_instance_id(r, 2)?,
                note: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<SqliteResult<_>>()?)
    }
}

// ============================================================================
// Migration `instances-v1`
// ============================================================================

/// A context as the migration reads it.
#[derive(Debug, Clone)]
struct MigCtx {
    id: ContextId,
    label: Option<String>,
    context_type: String,
    created_at: i64,
    created_by: PrincipalId,
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

/// What a context's next turn, judge and gate would have run on.
#[derive(Debug, Clone, PartialEq)]
struct Resolution {
    cast_label: Option<String>,
    slots: BTreeMap<String, SlotValues>,
}

/// A note for `instance_migration_report`. `group` is the leader context of
/// the instance it is about, turned into the instance id when written.
struct Note {
    kind: &'static str,
    context: Option<ContextId>,
    group: Option<ContextId>,
    text: String,
}

/// Which instance each context joins, by the context that starts the group.
/// `Ok(None)` is a context that belongs to no instance; `Err` is one the
/// links cannot place.
struct Grouper {
    contexts: HashMap<ContextId, MigCtx>,
    holders: HashMap<ContextId, PrincipalId>,
    handoff: HashSet<ContextId>,
    rotated_from: HashMap<ContextId, ContextId>,
    memo: HashMap<ContextId, Result<Option<ContextId>, String>>,
    visiting: Vec<ContextId>,
}

impl Grouper {
    fn name(&self, id: ContextId) -> String {
        self.contexts.get(&id).map_or_else(|| id.short(), MigCtx::name)
    }

    fn resolve(&mut self, id: ContextId) -> Result<Option<ContextId>, String> {
        if let Some(done) = self.memo.get(&id) {
            return done.clone();
        }
        if let Some(at) = self.visiting.iter().position(|v| *v == id) {
            let ring: Vec<String> = self.visiting[at..].iter().map(|c| self.name(*c)).collect();
            return Err(format!("its links form a cycle through {}", ring.join(" -> ")));
        }
        self.visiting.push(id);
        let answer = self.place(id);
        self.visiting.pop();
        self.memo.insert(id, answer.clone());
        answer
    }

    fn place(&mut self, id: ContextId) -> Result<Option<ContextId>, String> {
        let ctx = self.contexts.get(&id).cloned().expect("a context the grouper was built with");
        if self.handoff.contains(&id) {
            return Ok(None);
        }
        if ctx.is_judge_shadow() {
            let source = ctx.forked_from.expect("a judge shadow has a source");
            if !self.contexts.contains_key(&source) {
                return Err(format!("its forked_from names {}, which has no row", source.short()));
            }
            return self.resolve(source);
        }
        if let Some(performer) = self.holders.get(&id).copied() {
            for link in [self.rotated_from.get(&id).copied(), ctx.forked_from].into_iter().flatten() {
                if self.holders.get(&link) == Some(&performer) && self.contexts.contains_key(&link) {
                    if let Some(leader) = self.resolve(link)? {
                        return Ok(Some(leader));
                    }
                }
            }
            return Ok(Some(id));
        }
        match ctx.senpai {
            None => Ok(None),
            Some(senpai) if !self.contexts.contains_key(&senpai) => {
                Err(format!("its senpai_ctx names {}, which has no row", senpai.short()))
            }
            Some(senpai) => self.resolve(senpai),
        }
    }
}

/// The context that is an instance's conversation among `members` (creation
/// order): the newest live one that is not a fork child of another of the
/// instance's contexts or a judge shadow; failing that the newest such one;
/// failing that the newest member.
fn conversation_of<'a>(members: &[&'a MigCtx], all: &HashSet<ContextId>) -> &'a MigCtx {
    let candidate = |c: &&&MigCtx| !c.is_judge_shadow() && !c.forked_from.is_some_and(|f| all.contains(&f));
    members
        .iter()
        .rev()
        .filter(candidate)
        .find(|c| !c.archived)
        .or_else(|| members.iter().rev().find(candidate))
        .or_else(|| members.last())
        .copied()
        .expect("an instance has a member")
}

impl KernelDb {
    /// Group the contexts into character instances and write each one's
    /// bindings (`docs/senpai-kohai.md`, "Migration").
    ///
    /// Runs once, in one transaction, and writes the old columns' answers into
    /// the dynamic layer; it never invents a value. A binding is what the old
    /// resolver (override, the context's or its performer's cast, `llm_defaults`)
    /// answered for the context, written with every cast slot: `max_tokens` is
    /// the slot's, the model row's, `llm_defaults`' or 64000. A live context
    /// that cannot be placed refuses the whole migration, naming it; an
    /// archived one is left without an instance and listed.
    pub(super) fn migrate_instances(conn: &Connection) -> KernelDbResult<()> {
        let done: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)",
            [Self::INSTANCES_MIGRATION],
            |row| row.get(0),
        )?;
        if done {
            return Ok(());
        }
        let tx = conn.unchecked_transaction()?;

        let mut ordered: Vec<MigCtx> = {
            let mut stmt = tx.prepare(
                "SELECT context_id, label, context_type, created_at, created_by, forked_from,
                        archived_at, provider, model, cast_id, played_by, senpai_ctx
                 FROM contexts",
            )?;
            stmt.query_map([], |r| {
                Ok(MigCtx {
                    id: read_context_id(r, 0)?,
                    label: r.get(1)?,
                    context_type: r.get(2)?,
                    created_at: r.get(3)?,
                    created_by: read_principal_id(r, 4)?,
                    forked_from: read_opt_context_id(r, 5)?,
                    archived: r.get::<_, Option<i64>>(6)?.is_some(),
                    provider: r.get(7)?,
                    model: r.get(8)?,
                    cast_id: read_opt_cast_id(r, 9)?,
                    played_by: read_opt_principal_id(r, 10)?,
                    senpai: read_opt_context_id(r, 11)?,
                })
            })?
            .collect::<SqliteResult<_>>()?
        };
        ordered.sort_by_key(|c| (c.created_at, c.id));
        let by_id: HashMap<ContextId, MigCtx> = ordered.iter().map(|c| (c.id, c.clone())).collect();

        let characters: HashMap<PrincipalId, MigCharacter> = {
            let mut stmt = tx.prepare("SELECT principal_id, handoff_ctx, root_ctx, default_cast_id FROM characters")?;
            stmt.query_map([], |r| {
                Ok((
                    read_principal_id(r, 0)?,
                    MigCharacter {
                        handoff_ctx: read_opt_context_id(r, 1)?,
                        root_ctx: read_opt_context_id(r, 2)?,
                        default_cast: read_opt_cast_id(r, 3)?,
                    },
                ))
            })?
            .collect::<SqliteResult<_>>()?
        };
        let handoff: HashSet<ContextId> = characters.values().filter_map(|c| c.handoff_ctx).collect();
        let root_owner: HashMap<ContextId, PrincipalId> = characters
            .iter()
            .filter_map(|(p, c)| c.root_ctx.map(|ctx| (ctx, *p)))
            .collect();
        let holders: HashMap<ContextId, PrincipalId> = ordered
            .iter()
            .filter_map(|c| c.played_by.or_else(|| root_owner.get(&c.id).copied()).map(|p| (c.id, p)))
            .collect();

        let mut notes: Vec<Note> = Vec::new();

        // Rotation links: a successor's `forked_from` is its predecessor's
        // parent, so the predecessor is named only by ROTATED_FROM.
        let mut rotated_from: HashMap<ContextId, ContextId> = HashMap::new();
        {
            let mut stmt = tx.prepare("SELECT context_id, value FROM context_env WHERE key = 'ROTATED_FROM'")?;
            let rows: Vec<(ContextId, String)> = stmt
                .query_map([], |r| Ok((read_context_id(r, 0)?, r.get::<_, String>(1)?)))?
                .collect::<SqliteResult<_>>()?;
            for (ctx, value) in rows {
                match ContextId::parse(value.trim()) {
                    Err(_) => notes.push(Note {
                        kind: "dropped",
                        context: Some(ctx),
                        group: None,
                        text: format!("ROTATED_FROM \"{value}\" does not parse as a context id: ignored"),
                    }),
                    Ok(from) if !by_id.contains_key(&from) => notes.push(Note {
                        kind: "dropped",
                        context: Some(ctx),
                        group: None,
                        text: format!("ROTATED_FROM names context {}, which has no row: ignored", from.short()),
                    }),
                    Ok(from) => {
                        let performer = holders.get(&ctx);
                        if performer.is_some() && holders.get(&from) != performer {
                            notes.push(Note {
                                kind: "dropped",
                                context: Some(ctx),
                                group: None,
                                text: format!(
                                    "ROTATED_FROM names {}, which another performer plays: not followed",
                                    by_id[&from].name()
                                ),
                            });
                        } else {
                            rotated_from.insert(ctx, from);
                        }
                    }
                }
            }
        }

        // Rule 2: which instance each context joins.
        let mut grouper = Grouper {
            contexts: by_id.clone(),
            holders: holders.clone(),
            handoff,
            rotated_from,
            memo: HashMap::new(),
            visiting: Vec::new(),
        };
        let mut refused: Vec<String> = Vec::new();
        let mut unmapped: Vec<(ContextId, String)> = Vec::new();
        let mut placement: HashMap<ContextId, ContextId> = HashMap::new();
        for c in &ordered {
            match grouper.resolve(c.id) {
                Ok(Some(leader)) => {
                    placement.insert(c.id, leader);
                }
                Ok(None) => {}
                Err(why) if c.archived => unmapped.push((c.id, why)),
                Err(why) => refused.push(format!("{}: {why}", c.name())),
            }
        }

        // Groups, members in creation order, groups in order of first member.
        let mut group_order: Vec<ContextId> = Vec::new();
        let mut groups: HashMap<ContextId, Vec<&MigCtx>> = HashMap::new();
        for c in &ordered {
            if let Some(leader) = placement.get(&c.id) {
                if !groups.contains_key(leader) {
                    group_order.push(*leader);
                }
                groups.entry(*leader).or_default().push(c);
            }
        }

        // Rule 3: an instance's senpai is the instance of its first context's
        // senpai_ctx. A root console has none.
        let mut senpai_of: HashMap<ContextId, Option<ContextId>> = HashMap::new();
        let mut bad_groups: HashMap<ContextId, String> = HashMap::new();
        for leader in &group_order {
            let members = &groups[leader];
            let first = members[0];
            let is_root = members.iter().any(|m| root_owner.contains_key(&m.id));
            let mut senpai_group = None;
            if let Some(senpai_ctx) = first.senpai {
                if is_root {
                    notes.push(Note {
                        kind: "dropped",
                        context: Some(first.id),
                        group: Some(*leader),
                        text: format!(
                            "a root console has no senpai: senpai_ctx {} is not carried",
                            by_id.get(&senpai_ctx).map_or_else(|| senpai_ctx.short(), MigCtx::name)
                        ),
                    });
                } else {
                    match grouper.resolve(senpai_ctx) {
                        Ok(Some(target)) => senpai_group = Some(target),
                        Ok(None) => notes.push(Note {
                            kind: "dropped",
                            context: Some(first.id),
                            group: Some(*leader),
                            text: format!(
                                "senpai_ctx {} belongs to no instance: the instance has no senpai",
                                by_id.get(&senpai_ctx).map_or_else(|| senpai_ctx.short(), MigCtx::name)
                            ),
                        }),
                        Err(why) => {
                            bad_groups.insert(*leader, format!("its senpai_ctx cannot be placed: {why}"));
                        }
                    }
                }
            }
            if senpai_group == Some(*leader) {
                bad_groups.insert(
                    *leader,
                    format!(
                        "its first context {} has a senpai_ctx in the same instance, so the instance would be its own senpai",
                        first.name()
                    ),
                );
            }
            // Contexts after the first carry a senpai_ctx too; the instance has one senpai.
            for later in &members[1..] {
                let Some(senpai_ctx) = later.senpai else { continue };
                let target = placement.get(&senpai_ctx).copied();
                if target != senpai_group && target != Some(*leader) {
                    notes.push(Note {
                        kind: "dropped",
                        context: Some(later.id),
                        group: Some(*leader),
                        text: format!(
                            "senpai_ctx {} is not carried: an instance has one senpai, taken from its first context",
                            by_id.get(&senpai_ctx).map_or_else(|| senpai_ctx.short(), MigCtx::name)
                        ),
                    });
                }
            }
            senpai_of.insert(*leader, senpai_group);
        }
        // A chain of senpai instances that returns to itself.
        for start in &group_order {
            let mut seen = vec![*start];
            let mut at = senpai_of.get(start).copied().flatten();
            while let Some(next) = at {
                if next == *start {
                    for g in &seen {
                        bad_groups.entry(*g).or_insert_with(|| {
                            let ring: Vec<String> = seen.iter().map(|l| groups[l][0].name()).collect();
                            format!("its senpai chain loops back through {}", ring.join(" -> "))
                        });
                    }
                    break;
                }
                if seen.contains(&next) {
                    break;
                }
                seen.push(next);
                at = senpai_of.get(&next).copied().flatten();
            }
        }
        for leader in &group_order {
            let Some(why) = bad_groups.get(leader) else { continue };
            let members = &groups[leader];
            if members.iter().any(|m| !m.archived) {
                for m in members.iter().filter(|m| !m.archived) {
                    refused.push(format!("{}: {why}", m.name()));
                }
            } else {
                for m in members {
                    unmapped.push((m.id, why.clone()));
                }
            }
        }

        if !refused.is_empty() {
            refused.sort();
            refused.dedup();
            return Err(KernelDbError::Validation(format!(
                "the instance migration cannot place {} live context{} and will not guess; nothing was changed. \
                 Fix each link (ROTATED_FROM, forked_from, senpai_ctx) with the previous kernel, then start again:\n  {}",
                refused.len(),
                if refused.len() == 1 { "" } else { "s" },
                refused.join("\n  ")
            )));
        }
        let removed: HashSet<ContextId> = unmapped.iter().map(|(id, _)| *id).collect();
        group_order.retain(|l| !groups[l].iter().any(|m| removed.contains(&m.id)));
        for (ctx, why) in &unmapped {
            notes.push(Note {
                kind: "unmapped",
                context: Some(*ctx),
                group: None,
                text: format!("archived and left without an instance: {why}"),
            });
        }
        for c in &ordered {
            if !placement.contains_key(&c.id) && !removed.contains(&c.id) && (c.cast_id.is_some() || c.model.is_some()) {
                notes.push(Note {
                    kind: "dropped",
                    context: Some(c.id),
                    group: None,
                    text: "belongs to no instance, so its cast and model override run nothing and are not carried".into(),
                });
            }
        }

        // Rules 4 and 5: bindings and conversations.
        let catalog = Catalog::load(&tx)?;
        let effective_cast = |c: &MigCtx| -> Option<CastId> {
            let id = match c.cast_id {
                Some(id) => id,
                None => characters.get(&c.played_by?)?.default_cast?,
            };
            catalog.casts.contains_key(&id).then_some(id)
        };
        let resolve = |c: &MigCtx, pinned: &mut u32| -> (Resolution, Option<String>) {
            let cast = effective_cast(c);
            let mut slots = BTreeMap::new();
            if let Some(cast) = cast {
                for slot in catalog.slots.get(&cast).into_iter().flatten() {
                    slots.insert(slot.role.clone(), catalog.slot_values(&slot.role, slot.backend_id, &slot.model, &slot_tunables(slot), pinned));
                }
            }
            let own_role = c.context_type.clone();
            let mut unresolved = None;
            let reserved = own_role == "gate";
            if let Some(model) = &c.model {
                let backend = c.provider.clone().or_else(|| catalog.defaults.as_ref().map(|d| d.default_backend.clone()));
                match backend.as_deref().map(|b| (b, catalog.backend_by_name.get(b))) {
                    Some((_, Some(id))) => {
                        slots.insert(own_role.clone(), catalog.slot_values(&own_role, *id, model, &ModelTunablesRow::default(), pinned));
                    }
                    Some((name, None)) => unresolved = Some(format!("its override names backend {name}, which has no row")),
                    None => unresolved = Some("its override names no backend and llm_defaults has none".into()),
                }
            } else if reserved || !slots.contains_key(&own_role) {
                match &catalog.defaults {
                    None => unresolved = Some("it has no override, no cast slot for its type and no llm_defaults row".into()),
                    Some(d) => match catalog.backend_by_name.get(&d.default_backend) {
                        None => {
                            unresolved = Some(format!("llm_defaults names backend {}, which has no row", d.default_backend))
                        }
                        Some(id) => {
                            slots.insert(own_role.clone(), catalog.slot_values(&own_role, *id, &d.default_model, &ModelTunablesRow::default(), pinned));
                        }
                    },
                }
            }
            (Resolution { cast_label: cast.and_then(|id| catalog.casts.get(&id).cloned()), slots }, unresolved)
        };

        struct Plan {
            leader: ContextId,
            instance: CharacterInstanceId,
            principal: PrincipalId,
            started_at: i64,
            created_by: PrincipalId,
            resolutions: Vec<Resolution>,
            /// Per member (creation order): its 1-based binding seq.
            seqs: Vec<i64>,
            /// Per binding: (conversation context, bound_at).
            bindings: Vec<(ContextId, i64)>,
        }
        let mut plans: Vec<Plan> = Vec::new();
        let mut pinned = 0u32;
        for leader in &group_order {
            let members = &groups[leader];
            let member_ids: HashSet<ContextId> = members.iter().map(|m| m.id).collect();
            let mut resolutions: Vec<Resolution> = Vec::new();
            let mut seqs = Vec::new();
            for m in members {
                let (resolution, unresolved) = resolve(m, &mut pinned);
                if let Some(why) = unresolved {
                    notes.push(Note {
                        kind: "unresolved",
                        context: Some(m.id),
                        group: Some(*leader),
                        text: format!("the old resolver had no model for type {}: {why}", m.context_type),
                    });
                }
                let at = match resolutions.iter().position(|r| *r == resolution) {
                    Some(at) => at,
                    None => {
                        resolutions.push(resolution);
                        resolutions.len() - 1
                    }
                };
                seqs.push(at as i64 + 1);
            }
            // The conversation's resolution takes the highest seq, so the newest
            // binding's conversation is the instance's conversation.
            let conversation = conversation_of(members, &member_ids);
            let conversation_index = members.iter().position(|m| m.id == conversation.id).expect("a member");
            let last = seqs[conversation_index];
            if last != resolutions.len() as i64 {
                let moved = resolutions.remove(last as usize - 1);
                resolutions.push(moved);
                for seq in seqs.iter_mut() {
                    *seq = match *seq {
                        s if s == last => resolutions.len() as i64,
                        s if s > last => s - 1,
                        s => s,
                    };
                }
            }
            let bindings: Vec<(ContextId, i64)> = (1..=resolutions.len() as i64)
                .map(|seq| {
                    let of_seq: Vec<&MigCtx> = members.iter().zip(&seqs).filter(|(_, s)| **s == seq).map(|(m, _)| *m).collect();
                    let bound_at = of_seq.iter().map(|m| m.created_at).min().expect("a binding has a context");
                    (conversation_of(&of_seq, &member_ids).id, bound_at)
                })
                .collect();
            let newest = bindings.last().expect("an instance has a binding").0;
            if newest != conversation.id {
                return Err(KernelDbError::Validation(format!(
                    "the newest binding of the instance of {} names {}, not its conversation {}",
                    members[0].name(),
                    by_id[&newest].name(),
                    conversation.name()
                )));
            }
            let instance = CharacterInstanceId::new();
            if !conversation.archived {
                let newest_slots = &resolutions[resolutions.len() - 1].slots;
                let cast = resolutions[resolutions.len() - 1].cast_label.as_deref().unwrap_or("no cast");
                for (role, kind) in [("gate", "no_gate_slot"), ("distill", "no_distill_slot")] {
                    if !newest_slots.contains_key(role) {
                        notes.push(Note {
                            kind,
                            context: None,
                            group: Some(*leader),
                            text: format!("live instance of {} has no {role} slot ({cast})", members[0].name()),
                        });
                    }
                }
            }
            plans.push(Plan {
                leader: *leader,
                instance,
                principal: holders[leader],
                started_at: members.iter().map(|m| m.created_at).min().expect("members"),
                created_by: members[0].created_by,
                resolutions,
                seqs,
                bindings,
            });
        }

        let instance_of_leader: HashMap<ContextId, CharacterInstanceId> =
            plans.iter().map(|p| (p.leader, p.instance)).collect();
        for plan in &plans {
            if let Some(Some(target)) = senpai_of.get(&plan.leader)
                && !instance_of_leader.contains_key(target)
            {
                notes.push(Note {
                    kind: "dropped",
                    context: Some(groups[&plan.leader][0].id),
                    group: Some(plan.leader),
                    text: "its senpai instance is archived and could not be placed: the instance has no senpai".into(),
                });
            }
        }

        // Writes.
        for plan in &plans {
            insert_instance(&tx, plan.instance, plan.principal, None, plan.started_at, plan.created_by)?;
        }
        for plan in &plans {
            for (index, resolution) in plan.resolutions.iter().enumerate() {
                let seq = index as i64 + 1;
                let (conversation, bound_at) = plan.bindings[index];
                insert_binding(&tx, plan.instance, seq, conversation, bound_at, resolution.cast_label.as_deref(), None)?;
                for (role, slot) in &resolution.slots {
                    insert_slot(&tx, plan.instance, seq, role, slot)?;
                }
            }
            for (member, seq) in groups[&plan.leader].iter().zip(&plan.seqs) {
                assign_context(&tx, member.id, plan.instance, *seq)?;
            }
            let senpai = senpai_of.get(&plan.leader).copied().flatten().and_then(|l| instance_of_leader.get(&l).copied());
            if let Some(senpai) = senpai {
                tx.execute(
                    "UPDATE character_instances SET senpai_instance = ?1 WHERE instance_id = ?2",
                    params![blob_param(senpai.as_bytes()), blob_param(plan.instance.as_bytes())],
                )?;
            }
        }
        let recorded_at = now_millis();
        for note in &notes {
            let instance = note.group.and_then(|l| instance_of_leader.get(&l).copied());
            match note.kind {
                "dropped" => info!(kind = note.kind, context = ?note.context, "instances-v1: {}", note.text),
                _ => warn!(kind = note.kind, context = ?note.context, instance = ?instance, "instances-v1: {}", note.text),
            }
            tx.execute(
                "INSERT INTO instance_migration_report (kind, context_id, instance_id, note, recorded_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    note.kind,
                    note.context.as_ref().map(|c| c.as_bytes().to_vec()),
                    instance.as_ref().map(|i| i.as_bytes().to_vec()),
                    note.text,
                    recorded_at,
                ],
            )?;
        }
        tx.execute("INSERT INTO kernel_migrations (name) VALUES (?1)", [Self::INSTANCES_MIGRATION])?;
        tx.commit()?;
        info!(
            instances = plans.len(),
            contexts = placement.len(),
            max_tokens_pinned = pinned,
            reported = notes.len(),
            "mapped every context to a character instance"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests;
