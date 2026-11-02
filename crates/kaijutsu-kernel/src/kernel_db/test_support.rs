//! Test fixtures shared by the kernel_db tests.

use super::*;

/// Give a fresh database the schema one written before context characters
/// had: the columns and table `drop_pre_context_character_columns` removes,
/// and the `context_characters` table `context-fold-v1` folds away.
/// Idempotent. Rows in those columns are written with raw SQL, since no
/// `KernelDb` method writes them.
#[cfg(test)]
pub(super) fn into_pre_context_character(db: &KernelDb) {
    let c = &db.conn;
    KernelDb::ensure_legacy_context_characters(c).unwrap();
    KernelDb::ensure_legacy_cast_tables(c).unwrap();
    for (table, column, definition) in [
        ("contexts", "provider", "TEXT"),
        ("contexts", "model", "TEXT"),
        ("presets", "cast_id", "BLOB REFERENCES casts(cast_id) ON DELETE SET NULL"),
        ("presets", "system_prompt", "TEXT"),
    ].into_iter().chain(super::migrate_senpai::PRE_CONTEXT_CHARACTER_COLUMNS) {
        if !KernelDb::has_column(c, table, column).unwrap() {
            c.execute(&format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"), []).unwrap();
        }
    }
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS llm_defaults (
            id               INTEGER NOT NULL PRIMARY KEY DEFAULT 1 CHECK (id = 1),
            default_backend  TEXT    NOT NULL,
            default_model    TEXT    NOT NULL CHECK (default_model != ''),
            max_tokens       INTEGER CHECK (max_tokens IS NULL OR max_tokens > 0),
            temperature      REAL    CHECK (temperature IS NULL OR (temperature >= 0.0 AND temperature <= 2.0)),
            top_p            REAL    CHECK (top_p IS NULL OR (top_p > 0.0 AND top_p <= 1.0)),
            effort           TEXT,
            thinking_budget  INTEGER CHECK (thinking_budget IS NULL OR thinking_budget > 0),
            thinking_style   TEXT
        );",
    ).unwrap();
}

/// Put a database built through the current API back on the shape before
/// `context-fold-v1`: the performer, senpai and persona of each performed
/// context move into a `context_characters` table, `context_slots` references
/// it, and `contexts` loses the three columns. A performer-less context loses
/// its senpai with them, as it had none then. The next
/// `apply_additive_migrations` is the fold.
#[cfg(test)]
pub(super) fn into_pre_fold(db: &KernelDb) {
    let c = &db.conn;
    KernelDb::ensure_legacy_context_characters(c).unwrap();
    c.execute_batch(
        "INSERT INTO context_characters (context_id, principal_id, senpai_ctx, persona, created_at)
             SELECT context_id, performer, senpai_ctx, persona, created_at FROM contexts WHERE performer IS NOT NULL;
         DROP TRIGGER IF EXISTS contexts_performer_fixed;
         DROP TRIGGER IF EXISTS contexts_senpai_required;
         DROP INDEX IF EXISTS idx_contexts_performer;
         DROP INDEX IF EXISTS idx_contexts_senpai;
         PRAGMA foreign_keys = OFF;
         CREATE TABLE context_slots_old (
             context_id      BLOB    NOT NULL REFERENCES context_characters(context_id),
             role            TEXT    NOT NULL,
             backend_id      BLOB    NOT NULL REFERENCES backends(backend_id),
             model           TEXT    NOT NULL CHECK (model != ''),
             max_tokens      INTEGER CHECK (max_tokens IS NULL OR max_tokens > 0),
             context_window  INTEGER CHECK (context_window IS NULL OR context_window > 0),
             temperature     REAL    CHECK (temperature IS NULL OR (temperature >= 0.0 AND temperature <= 2.0)),
             top_p           REAL    CHECK (top_p IS NULL OR (top_p > 0.0 AND top_p <= 1.0)),
             effort          TEXT,
             thinking_budget INTEGER CHECK (thinking_budget IS NULL OR thinking_budget > 0),
             thinking_style  TEXT,
             PRIMARY KEY (context_id, role)
         );
         INSERT INTO context_slots_old SELECT context_id, role, backend_id, model, max_tokens, context_window,
             temperature, top_p, effort, thinking_budget, thinking_style FROM context_slots;
         DROP TABLE context_slots;
         ALTER TABLE context_slots_old RENAME TO context_slots;
         ALTER TABLE contexts DROP COLUMN performer;
         ALTER TABLE contexts DROP COLUMN senpai_ctx;
         ALTER TABLE contexts DROP COLUMN persona;
         PRAGMA foreign_keys = ON;
         DELETE FROM kernel_migrations WHERE name = 'context-fold-v1';",
    ).unwrap();
}

/// Write the stored `characters.root_ctx` of a character.
#[cfg(test)]
pub(super) fn set_stored_root_ctx(db: &KernelDb, character: PrincipalId, ctx: ContextId) {
    db.conn.execute(
        "UPDATE characters SET root_ctx = ?1 WHERE principal_id = ?2",
        params![blob_param(ctx.as_bytes()), blob_param(character.as_bytes())],
    ).unwrap();
}

/// Write the stored `contexts.played_by` of a context that predates context characters.
#[cfg(test)]
pub(super) fn set_stored_played_by(db: &KernelDb, ctx: ContextId, played_by: Option<PrincipalId>) {
    db.conn.execute(
        "UPDATE contexts SET played_by = ?1 WHERE context_id = ?2",
        params![played_by.as_ref().map(|p| p.as_bytes().to_vec()), blob_param(ctx.as_bytes())],
    ).unwrap();
}

#[cfg(test)]
pub(super) fn make_context_row(label: Option<&str>) -> ContextRow {
    ContextRow {
        context_id: ContextId::new(),
        label: label.map(String::from),
        system_prompt: None,
        context_state: ContextState::Live,
        context_type: "default".to_string(),
        created_at: now_millis(),
        created_by: PrincipalId::new(),
        forked_from: None,
        fork_kind: None,
        archived_at: None,
        workspace_id: None,
        preset_id: None,
        concluded_at: None,
        last_activity_at: None,
        promoted_at: None,
        demoted_at: None,
        paused_at: None,
        origin_host: None,
        performer: None, senpai_ctx: None, persona: None,
    }
}

/// Insert both a document row and context row for a context.
/// Tests need this because contexts FK to documents.
#[cfg(test)]
pub(super) fn insert_context_with_doc(db: &KernelDb, row: &ContextRow, ws_id: WorkspaceId) {
    db.insert_document(&DocumentRow {
        document_id: row.context_id,
        workspace_id: ws_id,
        doc_kind: DocKind::Conversation,
        language: None,
        path: None,
        created_at: row.created_at,
        created_by: row.created_by,
    })
    .unwrap();
    db.insert_context(row).unwrap();
}

/// Set up a test DB with the default workspace. Returns its WorkspaceId.
#[cfg(test)]
pub(super) fn setup_test_db(db: &KernelDb) -> WorkspaceId {
    db.get_or_create_default_workspace(PrincipalId::system())
        .unwrap()
}

#[cfg(test)]
pub(super) fn make_edge(source: ContextId, target: ContextId) -> ContextEdgeRow {
    ContextEdgeRow {
        edge_id: uuid::Uuid::now_v7(),
        source_id: source,
        target_id: target,
        metadata: None,
        created_at: now_millis(),
    }
}

pub(super) fn insert_test_preset(db: &KernelDb, label: &str) -> PresetId {
    let pid = PresetId::new();
    db.insert_preset(&PresetRow {
        preset_id: pid,
        label: label.into(),
        description: None,
        created_at: now_millis(),
        created_by: PrincipalId::new(),
    })
    .unwrap();
    pid
}

/// Column list shared by every shape `backends` has ever had — legacy
/// fixtures below insert against this list so the rebuild's `INSERT ...
/// SELECT` has something to carry across.
pub(super) const BACKENDS_LEGACY_COLS: &str = "backend_id, name, kind, key_optional, created_at, created_by";

pub(super) fn put_backends_back_on_its_pre_codex_shape(conn: &Connection) {
    conn.execute_batch(
        "PRAGMA foreign_keys = OFF;
         DROP TABLE backends;
         CREATE TABLE backends (
             backend_id BLOB NOT NULL PRIMARY KEY,
             name TEXT NOT NULL UNIQUE,
             kind TEXT NOT NULL CHECK (kind IN ('anthropic', 'deepseek', 'openai', 'mock')),
             base_url TEXT,
             api_key_env TEXT,
             api_key_file TEXT,
             key_optional INTEGER NOT NULL DEFAULT 0,
             request_timeout_secs INTEGER
                 CHECK (request_timeout_secs IS NULL OR request_timeout_secs > 0),
             created_at INTEGER NOT NULL,
             created_by BLOB NOT NULL
         );
         PRAGMA foreign_keys = ON;",
    ).unwrap();
}

/// A character sheet plus the three ids a walk test needs.
pub(super) fn seed_characters(db: &KernelDb, names: &[&str]) -> Vec<PrincipalId> {
    names.iter().map(|name| {
        let principal_id = PrincipalId::new();
        db.insert_character(&CharacterRow { context_type: None,
            principal_id, name: (*name).to_string(), created_at: 0,
            retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
        }).unwrap();
        principal_id
    }).collect()
}

/// Insert `ctx` under `senpai` (`None` = a top), played by `holder`.
pub(super) fn senpai_ctx_row(
    db: &KernelDb,
    ws_id: WorkspaceId,
    label: &str,
    senpai: Option<&ContextRow>,
    holder: Option<PrincipalId>,
) -> ContextRow {
    let mut row = make_context_row(Some(label));
    insert_context_with_doc(db, &row, ws_id);
    if let Some(holder) = holder {
        db.update_performer(row.context_id, Some(holder)).unwrap();
    }
    if let Some(senpai) = senpai {
        db.set_context_senpai(row.context_id, Some(senpai.context_id)).unwrap();
    }
    row.performer = holder;
    row
}

/// Run `context-characters-v1` over the rows a test wrote with the old columns,
/// then read its `context_characters` rows through the `contexts` columns the
/// API reads (what `context-fold-v1` copies, without its senpai assignment
/// or its drop of the table).
pub(super) fn run_context_character_migration(db: &KernelDb) -> KernelDbResult<()> {
    db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::CONTEXT_CHARACTERS_MIGRATION]).unwrap();
    KernelDb::migrate_context_characters(&db.conn)?;
    db.conn.execute_batch(
        "DROP TRIGGER IF EXISTS contexts_performer_fixed;
         DROP TRIGGER IF EXISTS contexts_senpai_required;
         UPDATE contexts SET
             performer  = (SELECT cc.principal_id FROM context_characters cc WHERE cc.context_id = contexts.context_id),
             senpai_ctx = (SELECT cc.senpai_ctx   FROM context_characters cc WHERE cc.context_id = contexts.context_id),
             persona    = (SELECT cc.persona      FROM context_characters cc WHERE cc.context_id = contexts.context_id)
         WHERE context_id IN (SELECT context_id FROM context_characters);",
    )?;
    KernelDb::ensure_context_guards(&db.conn)
}

pub(super) fn turn_start(ctx: ContextId) -> TurnStart {
    TurnStart {
        turn_id: TurnId::new(),
        context_id: ctx,
        performer: PrincipalId::new(),
        role: "coder".into(),
        provider: "anthropic".into(),
        model: "claude-haiku-5-5".into(),
        started_at: 1_000,
    }
}

pub(super) fn binding_with(
    instances: &[&str],
    names: &[(&str, &str, &str)], // (visible, instance, original)
) -> ContextToolBinding {
    let mut b = ContextToolBinding::new();
    b.allowed_instances = instances.iter().map(|s| InstanceId::new(*s)).collect();
    for (visible, inst, tool) in names {
        b.name_map
            .insert((*visible).into(), (InstanceId::new(*inst), (*tool).into()));
    }
    b
}

pub(super) fn minimal_hook_row(id: &str, phase: &str, priority: i32) -> HookRow {
    HookRow {
        hook_id: id.into(),
        phase: phase.into(),
        priority,
        match_instance: None,
        match_tool: None,
        match_context: None,
        match_principal: None,
        action_kind: "log".into(),
        action_builtin_name: None,
        action_kaish_body: None,
        action_kaish_script_id: None,
        action_kaish_path: None,
        action_result_text: None,
        action_is_error: None,
        action_deny_reason: None,
        action_log_target: Some("kaijutsu::hooks".into()),
        action_log_level: Some("info".into()),
        action_ask_description: None,
    }
}

pub(super) fn make_track(track_id: &str, period_ms: u64) -> PersistedTrack {
    PersistedTrack {
        track_id: track_id.to_string(),
        period_ms,
        beats_per_phrase: 16,
        playhead_tick: None,
        playing: false,
        score_context_id: None,
        clock_kind: "system".to_string(),
    }
}

pub(super) fn make_attachment(track_id: &str, context_id: ContextId) -> PersistedAttachment {
    PersistedAttachment {
        track_id: track_id.to_string(),
        context_id,
        wakeup_every: 1,
        rotate_every_phrases: None,
        ooda_armed: false,
    }
}
