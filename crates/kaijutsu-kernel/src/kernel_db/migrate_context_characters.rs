//! Migration `context-characters-v1`: the context character table of the senpai/kohai model,
//! written from the columns an older `contexts` row carried. `context-fold-v1`
//! (`migrate_fold.rs`) moves its rows into `contexts`.

use std::collections::BTreeMap;

use super::*;
use super::context_characters::{SlotValues, insert_slot, over};

/// `max_tokens` the old model path sent when the slot, the model row and
/// `llm_defaults` all left it unset (`LlmRegistry::max_output_tokens`).
const UNSET_MAX_TOKENS: i64 = 64_000;

/// The pre-fold `context_characters` table, created empty when a database old
/// enough to lack it is about to be read by the migrations that fill it.
/// `context-fold-v1` moves its rows into `contexts` and drops it.
const LEGACY_CONTEXT_CHARACTERS: &str = "
CREATE TABLE IF NOT EXISTS context_characters (
    context_id   BLOB    NOT NULL PRIMARY KEY REFERENCES contexts(context_id),
    principal_id BLOB    NOT NULL,
    senpai_ctx   BLOB    REFERENCES contexts(context_id),
    persona      TEXT,
    created_at   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_context_characters_principal
    ON context_characters(principal_id);
CREATE INDEX IF NOT EXISTS idx_context_characters_senpai
    ON context_characters(senpai_ctx) WHERE senpai_ctx IS NOT NULL;
";

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

/// One slot of a cast as the migration reads it from the table that
/// `character-slots-v1` later drops.
struct LegacyCastSlot {
    cast_id: CastId,
    role: String,
    backend_id: BackendId,
    model: String,
    tunables: ModelTunablesRow,
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
    slots: HashMap<CastId, Vec<LegacyCastSlot>>,
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
        let mut slots: HashMap<CastId, Vec<LegacyCastSlot>> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT cast_id, role, backend_id, model, max_tokens, temperature, top_p, effort,
                        thinking_budget, thinking_style FROM cast_slots ORDER BY cast_id, role",
            )?;
            for slot in stmt.query_map([], |r| {
                Ok(LegacyCastSlot {
                    cast_id: read_cast_id_col(r, 0)?,
                    role: r.get(1)?,
                    backend_id: read_backend_id(r, 2)?,
                    model: r.get(3)?,
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
            // The roles that named a council server in the schema this
            // migration reads.
            None if matches!(role, "gate" | "judge") => None,
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
    pub(super) const CONTEXT_CHARACTERS_MIGRATION: &'static str = "context-characters-v1";

    /// Create the pre-fold `context_characters` table when it is absent, for
    /// a database old enough that `context-characters-v1` is about to fill it.
    pub(super) fn ensure_legacy_context_characters(conn: &Connection) -> KernelDbResult<()> {
        conn.execute_batch(LEGACY_CONTEXT_CHARACTERS)?;
        Ok(())
    }

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
        Self::ensure_legacy_context_characters(conn)?;
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
                    text: format!("no performer: left without a context character; its senpai_ctx {} is kept", name_of(senpai)),
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
                        catalog.slot_values(&slot.role, slot.backend_id, &slot.model, &slot.tunables, &mut pinned),
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

