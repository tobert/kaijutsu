use super::*;

/// A DB whose `hooks` table predates `action_kaish_path` still opens:
/// the retired-hook cleanup runs after the column backfill it names.
#[test]
fn retired_classifier_cleanup_opens_a_hooks_table_without_kaish_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    {
        let db = KernelDb::open(&path).unwrap();
        db.conn.execute_batch("ALTER TABLE hooks DROP COLUMN action_kaish_path").unwrap();
    }
    let db = KernelDb::open(&path).expect("a legacy hooks table must still open");
    assert!(db.load_all_hooks().unwrap().is_empty());
}

/// The retired classifier's global pre_call hook row and the per-context
/// env its rc exported are removed at open. A hook with the same id that
/// runs a different body, and every other env key, stay.
#[test]
fn retired_classifier_hook_and_env_are_removed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    let (hook_id, hook_path) = RETIRED_CLASSIFIER_HOOK;
    let context_id = {
        let db = KernelDb::open(&path).unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("retired-classifier"));
        insert_context_with_doc(&db, &ctx, ws_id);
        let mut hook = minimal_hook_row(hook_id, "pre_call", 0);
        hook.match_tool = Some("shell_write".into());
        hook.action_kind = "kaish_path".into();
        hook.action_kaish_path = Some(hook_path.into());
        hook.action_log_target = None;
        hook.action_log_level = None;
        db.insert_hook(&hook).unwrap();
        db.insert_hook(&minimal_hook_row("operator-hook", "pre_call", 0)).unwrap();
        for key in RETIRED_CLASSIFIER_ENV {
            db.set_context_env(ctx.context_id, key, "escalate").unwrap();
        }
        db.set_context_env(ctx.context_id, "EDITOR", "vim").unwrap();
        ctx.context_id
    };
    let db = KernelDb::open(&path).unwrap();
    let hooks: Vec<String> = db.load_all_hooks().unwrap().into_iter().map(|h| h.hook_id).collect();
    assert_eq!(hooks, vec!["operator-hook".to_string()]);
    let env: Vec<String> = db.get_context_env(context_id).unwrap().into_iter().map(|e| e.key).collect();
    assert_eq!(env, vec!["EDITOR".to_string()]);
    drop(db);

    let db = KernelDb::open(&path).unwrap();
    let mut same_id_other_body = minimal_hook_row(hook_id, "pre_call", 0);
    same_id_other_body.action_kind = "kaish_path".into();
    same_id_other_body.action_kaish_path = Some("/config/rc/lib/hooks/mine.kai".into());
    db.insert_hook(&same_id_other_body).unwrap();
    drop(db);
    assert_eq!(KernelDb::open(&path).unwrap().load_all_hooks().unwrap().len(), 2);
}

/// A row a deleted template-subtree fork left behind with
/// `fork_kind = 'subtree'` must not survive a reopen —
/// `fork_kind_from_sql` treats any unrecognized value as corruption, so
/// `open()` clears the retired tag on every open before any reader can
/// trip over it.
#[test]
fn open_normalizes_a_stored_subtree_fork_kind_to_none() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("kernel.db");
    let context_id;
    {
        let db = KernelDb::open(&db_path).unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("old-subtree-fork"));
        context_id = row.context_id;
        insert_context_with_doc(&db, &row, ws_id);
        db.conn
            .execute(
                "UPDATE contexts SET fork_kind = 'subtree' WHERE context_id = ?1",
                params![blob_param(context_id.as_bytes())],
            )
            .unwrap();
    }
    // Drop closed the connection; re-open the same file. Without the
    // migration this read fails: `get_context` runs `fork_kind_from_sql`
    // over the stored 'subtree' value and errors.
    let db = KernelDb::open(&db_path).unwrap();
    let loaded = db.get_context(context_id).unwrap().unwrap();
    assert_eq!(loaded.fork_kind, None);
}

/// The retired `gate_actions` / `gate_action_argv` tables are dropped by
/// `SCHEMA` on every open, so a database that ran the old code sheds them
/// rather than carrying debris no code knows about. Simulates that old
/// database by hand (`gate_action_argv` first: it foreign-keys onto
/// `gate_actions`) and asserts both are gone after the drop runs.
///
/// Falsified by deleting the `DROP TABLE IF EXISTS gate_actions;`
/// statement from `SCHEMA`: `gate_actions` survived (`gate_action_argv`
/// still went, since its own DROP is independent). Reverted.
#[test]
fn retired_gate_action_tables_are_dropped_on_open() {
    let db = KernelDb::temporary().unwrap();
    db.conn
        .execute_batch(
            "CREATE TABLE gate_actions (request_id TEXT NOT NULL PRIMARY KEY);
             CREATE TABLE gate_action_argv (
                 request_id TEXT NOT NULL REFERENCES gate_actions(request_id),
                 seq INTEGER NOT NULL,
                 arg TEXT NOT NULL,
                 PRIMARY KEY (request_id, seq)
             );",
        )
        .unwrap();

    db.conn.execute_batch(SCHEMA).unwrap();

    let table_count = |name: &str| -> i64 {
        db.conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                params![name],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(table_count("gate_actions"), 0, "gate_actions must be dropped");
    assert_eq!(table_count("gate_action_argv"), 0, "gate_action_argv must be dropped");
}

/// A database carrying `backends`' pre-`codex-app` CHECK must lose it on
/// the next `apply_additive_migrations` (the real "next open" path),
/// keep the row it already had, and accept a `kind` the old CHECK
/// rejected.
#[test]
fn drop_legacy_value_enum_checks_drops_the_pre_codex_backends_check_and_keeps_rows() {
    let db = KernelDb::temporary().unwrap();
    put_backends_back_on_its_pre_codex_shape(&db.conn);
    let kept_id = BackendId::new();
    db.conn
        .execute(
            &format!(
                "INSERT INTO backends ({BACKENDS_LEGACY_COLS}) VALUES (?1, 'kept', 'anthropic', 0, 1000, ?2)"
            ),
            params![blob_param(kept_id.as_bytes()), blob_param(PrincipalId::system().as_bytes())],
        )
        .unwrap();

    KernelDb::apply_additive_migrations(&db.conn).unwrap();

    let sql: String = db
        .conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'backends'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!sql.contains("CHECK (kind IN ("), "the value-enum CHECK must be gone, got: {sql}");
    assert_eq!(db.get_backend_by_name("kept").unwrap().unwrap().kind, "anthropic");

    // `codex-app` is the kind whose arrival needed the CHECK widened in
    // the first place: the fixture's constraint rejects it, and
    // `BackendKind::parse` accepts it. Writing it here is what proves the
    // old constraint is gone rather than merely absent from `SCHEMA`.
    let codex = BackendRow {
        backend_id: BackendId::new(),
        name: "codex".into(),
        kind: "codex-app".into(),
        base_url: None,
        api_key_env: None,
        api_key_file: None,
        key_optional: false,
        request_timeout_secs: None,
        idle_timeout_secs: None,
        max_concurrent: Some(3),
        created_at: now_millis(),
        created_by: PrincipalId::system(),
    };
    db.upsert_backend(&codex).unwrap();
    let stored = db.get_backend_by_name("codex").unwrap().unwrap();
    assert_eq!(stored.kind, "codex-app");
    // The rebuild carries the column the additive ladder added first.
    assert_eq!(stored.max_concurrent, Some(3));
}

/// `rebuild_table_dropping_check` guards on the retired CHECK's own text,
/// never the bare word "CHECK" — `backends` keeps its
/// `request_timeout_secs` CHECK, so a naive guard would call every open a
/// legacy shape forever. First call on a legacy `backends` rebuilds and
/// reports `true`; the second, against the now-checkless table, must
/// report `false` without touching the table again. Falsified by
/// guarding on the bare substring `"CHECK"` instead: the second call also
/// returned `true` (`request_timeout_secs`'s CHECK is still there).
#[test]
fn rebuild_table_dropping_check_is_a_true_no_op_on_its_second_call() {
    let db = KernelDb::temporary().unwrap();
    put_backends_back_on_its_pre_codex_shape(&db.conn);

    let column_defs = "backend_id           BLOB NOT NULL PRIMARY KEY,
             name                 TEXT NOT NULL UNIQUE,
             kind                 TEXT NOT NULL,
             base_url             TEXT,
             api_key_env          TEXT,
             api_key_file         TEXT,
             key_optional         INTEGER NOT NULL DEFAULT 0,
             request_timeout_secs INTEGER
                 CHECK (request_timeout_secs IS NULL OR request_timeout_secs > 0),
             created_at           INTEGER NOT NULL
                 DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
             created_by           BLOB NOT NULL";
    let shared_columns = "backend_id, name, kind, base_url, api_key_env, api_key_file, \
             key_optional, request_timeout_secs, created_at, created_by";

    let first = KernelDb::rebuild_table_dropping_check(
        &db.conn,
        "backends",
        "CHECK (kind IN (",
        column_defs,
        shared_columns,
    )
    .unwrap();
    assert!(first, "the legacy shape must be rebuilt on the first call");

    let second = KernelDb::rebuild_table_dropping_check(
        &db.conn,
        "backends",
        "CHECK (kind IN (",
        column_defs,
        shared_columns,
    )
    .unwrap();
    assert!(!second, "an already-checkless table must not be rebuilt again");
}

/// Stage 1 (time-well): `last_activity_at` is added to `SCHEMA` directly
/// AND listed in `apply_additive_migrations` for pre-migration DBs. On a
/// fresh DB (column already in `SCHEMA`), re-running the migration must be
/// a guarded no-op — the same idempotency contract as `concluded_at`.
#[test]
fn last_activity_at_migration_is_idempotent_no_op_on_fresh_db() {
    let db = KernelDb::temporary().unwrap();
    // temporary() already ran apply_additive_migrations() once; running it
    // again must not error (duplicate-column guard) and the column must
    // still be queryable.
    KernelDb::apply_additive_migrations(&db.conn).unwrap();
    KernelDb::apply_additive_migrations(&db.conn).unwrap();

    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("mig-check"));
    insert_context_with_doc(&db, &row, ws_id);
    let loaded = db.get_context(row.context_id).unwrap().unwrap();
    assert_eq!(loaded.last_activity_at, None, "fresh row: never touched");
}

/// Same idempotency contract as `last_activity_at_migration_...` above,
/// for the `origin_host` column this change adds. A pre-migration row
/// (simulated here by simply never calling `set_origin_host`) must still
/// round-trip as `None` — "old rows without the field still render" is
/// the same fact as "a fresh, never-set row reads back as None".
#[test]
fn origin_host_migration_is_idempotent_no_op_on_fresh_db() {
    let db = KernelDb::temporary().unwrap();
    KernelDb::apply_additive_migrations(&db.conn).unwrap();
    KernelDb::apply_additive_migrations(&db.conn).unwrap();

    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("origin-host-mig-check"));
    insert_context_with_doc(&db, &row, ws_id);
    let loaded = db.get_context(row.context_id).unwrap().unwrap();
    assert_eq!(
        loaded.origin_host, None,
        "fresh/pre-migration row: never set, must read back as None (\"-\" on display), \
         not error or default to some fabricated string"
    );
}

/// A real `ALTER TABLE` failure — not the expected/ignorable "duplicate
/// column name" case — must propagate loudly, not vanish behind a
/// swallowed `let _ = conn.execute(...)`. Drop a table one of the
/// migrations targets so its ALTER fails for a genuine reason ("no such
/// table"), and confirm `apply_additive_migrations` surfaces it as an
/// `Err` instead of silently continuing.
#[test]
fn apply_additive_migrations_propagates_real_errors() {
    let db = KernelDb::temporary().unwrap();
    db.conn.execute_batch("DROP TABLE tracks").unwrap();
    let result = KernelDb::apply_additive_migrations(&db.conn);
    match result {
        Err(KernelDbError::Db(rusqlite::Error::SqliteFailure(_, Some(msg)))) => {
            assert!(
                msg.contains("no such table"),
                "expected a no-such-table failure, got: {msg}"
            );
        }
        other => panic!("a real ALTER TABLE failure must propagate: {other:?}"),
    }
}

/// Contexts created before the `house` capability existed keep the kj
/// administration verbs: the migration grants `house` to every existing
/// non-empty binding except the worker types', once.
#[test]
fn house_capability_migrates_onto_pre_existing_bindings_once() {
    use crate::mcp::{Capability, ContextToolBinding};
    let mut db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let mut ids = std::collections::BTreeMap::new();
    for ty in ["default", "mcp", "director", "coder", "toolie", "musician", "empty", "unbound"] {
        let mut row = make_context_row(Some(ty));
        row.context_type = if matches!(ty, "empty" | "unbound") { "default" } else { ty }.to_string();
        insert_context_with_doc(&db, &row, ws_id);
        let mut b = ContextToolBinding::new();
        match ty {
            "empty" => {}
            "unbound" => {
                ids.insert(ty, row.context_id);
                continue;
            }
            _ => {
                b.grant(Capability::Facade("shell".into()));
                b.grant(Capability::Operator);
            }
        }
        db.upsert_context_binding(row.context_id, &b).unwrap();
        ids.insert(ty, row.context_id);
    }
    // The shape of a database from before this migration: no marker.
    db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::HOUSE_MIGRATION]).unwrap();
    fn holds(db: &KernelDb, id: ContextId) -> bool {
        db.get_context_binding(id).unwrap().is_some_and(|b| b.allows(&Capability::House))
    }
    assert!(!holds(&db, ids["default"]), "fixture: old binding has no house");

    KernelDb::apply_additive_migrations(&db.conn).unwrap();
    for ty in ["default", "mcp", "director"] {
        assert!(holds(&db, ids[ty]), "{ty} binding should gain house");
    }
    for ty in ["coder", "toolie", "musician"] {
        assert!(!holds(&db, ids[ty]), "{ty} binding must not gain house");
    }
    // A context that grants nothing stays empty: house must not turn
    // "no usable loadout" into a usable one.
    assert!(db.get_context_binding(ids["empty"]).unwrap().unwrap().is_empty());
    assert!(db.get_context_binding(ids["unbound"]).unwrap().is_none());
    // The grant is additive.
    let kept = db.get_context_binding(ids["default"]).unwrap().unwrap();
    assert!(kept.allows(&Capability::Operator) && kept.allows(&Capability::Facade("shell".into())));

    // One-time: a seat that later drops house is not given it back.
    let mut narrowed = db.get_context_binding(ids["default"]).unwrap().unwrap();
    narrowed.revoke_cap(&Capability::House);
    db.upsert_context_binding(ids["default"], &narrowed).unwrap();
    KernelDb::apply_additive_migrations(&db.conn).unwrap();
    assert!(!holds(&db, ids["default"]), "the migration must run once");
    assert!(holds(&db, ids["mcp"]), "a second run changes nothing else");
}

/// `context_usage.cache_ttl_secs` must land on a DB created before the
/// column existed, via `apply_additive_migrations`'s guarded `ALTER
/// TABLE ... ADD COLUMN`, not just on a fresh `SCHEMA`. Simulates a
/// pre-existing DB by dropping the column back off a freshly-opened
/// (already-migrated) table, then re-running the migration and
/// confirming a full `set_context_usage`/`get_context_usage` round trip
/// carries the value through.
#[test]
fn cache_ttl_secs_migrates_onto_a_pre_existing_context_usage_table() {
    let db = KernelDb::temporary().unwrap();
    db.conn
        .execute_batch("ALTER TABLE context_usage DROP COLUMN cache_ttl_secs")
        .unwrap();

    KernelDb::apply_additive_migrations(&db.conn).unwrap();

    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("cache-ttl-migration"));
    insert_context_with_doc(&db, &ctx, ws_id);
    db.set_context_usage(&ContextUsageRow {
        context_id: ctx.context_id,
        provider: "anthropic".into(),
        model: "claude-sonnet-4-6".into(),
        input_tokens: 100,
        output_tokens: 20,
        cache_read_tokens: 50,
        cache_write_tokens: 10,
        reasoning_tokens: 0,
        cache_ttl_secs: 3600,
        updated_at: 1,
    })
    .unwrap();

    let loaded = db.get_context_usage(ctx.context_id).unwrap().unwrap();
    assert_eq!(loaded.cache_ttl_secs, 3600);
}

/// Re-running the migration against an already-migrated DB (the normal
/// case on every `open()`) must be a no-op, not clobber an existing
/// non-default value with the column's `DEFAULT 0` — the ADD COLUMN
/// guard (`is_duplicate_column_error`) exists exactly for this.
#[test]
fn cache_ttl_secs_migration_is_idempotent_and_preserves_existing_value() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("cache-ttl-idempotent"));
    insert_context_with_doc(&db, &ctx, ws_id);
    db.set_context_usage(&ContextUsageRow {
        context_id: ctx.context_id,
        provider: "anthropic".into(),
        model: "claude-sonnet-4-6".into(),
        input_tokens: 100,
        output_tokens: 20,
        cache_read_tokens: 50,
        cache_write_tokens: 10,
        reasoning_tokens: 0,
        cache_ttl_secs: 300,
        updated_at: 1,
    })
    .unwrap();

    KernelDb::apply_additive_migrations(&db.conn).unwrap();

    let loaded = db.get_context_usage(ctx.context_id).unwrap().unwrap();
    assert_eq!(loaded.cache_ttl_secs, 300);
}

/// `KernelDb::open` must migrate the approval-ledger schema into the
/// kernel DB, and a re-open of an already-migrated file must be a no-op
/// (the ledger's `CREATE TABLE IF NOT EXISTS` is idempotent). Guards the
/// boot path the `kj cc send` gate depends on: a live kernel that
/// restarts onto its existing `kernel.db` must find the ledger tables
/// every time.
#[test]
fn open_migrates_the_ledger_schema_and_reopen_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.db");

    {
        let db = KernelDb::open(&db_path).unwrap();
        let n: i64 = db
            .conn_for_ledger()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'approvals'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "open() must create the ledger's approvals table");
    }
    // Drop closed the connection; re-open the same file.
    {
        let db = KernelDb::open(&db_path).unwrap();
        let n: i64 = db
            .conn_for_ledger()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'approvals'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "re-open must be an idempotent no-op");
    }
}

/// A `characters` table on the shape that shipped `accountable_to`
/// comes back with that column dropped and `root` added, and the rows
/// that were in it survive both moves.
#[test]
fn root_migration_adds_the_flag_and_drops_accountable_to() {
    let db = KernelDb::temporary().unwrap();
    db.conn
        .execute_batch(
            "DROP TABLE characters;
             CREATE TABLE characters (
                 principal_id   BLOB NOT NULL PRIMARY KEY,
                 name           TEXT NOT NULL UNIQUE,
                 created_at     INTEGER NOT NULL,
                 retired_at     INTEGER,
                 handoff_ctx    BLOB REFERENCES contexts(context_id) ON DELETE SET NULL,
                 accountable_to BLOB REFERENCES characters(principal_id) ON DELETE RESTRICT
             );",
        )
        .unwrap();
    let principal = PrincipalId::new();
    db.conn
        .execute(
            "INSERT INTO characters (principal_id, name, created_at, retired_at, handoff_ctx, accountable_to) \
             VALUES (?1, ?2, ?3, NULL, NULL, NULL)",
            params![blob_param(principal.as_bytes()), "pre-existing", 1000],
        )
        .unwrap();
    let has = |column: &str| -> bool {
        db.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('characters') WHERE name = ?1)",
            params![column], |row| row.get(0),
        ).unwrap()
    };
    assert!(has("accountable_to"), "test setup must start on the old shape");
    assert!(!has("root"), "test setup must start without the flag");

    KernelDb::apply_additive_migrations(&db.conn).unwrap();

    assert!(!has("accountable_to"), "the guarded DROP must remove accountable_to");
    assert!(has("root"), "the guarded ALTER must add root");
    let row = db.get_character(principal).unwrap().unwrap();
    assert_eq!(row.name, "pre-existing", "an existing row must survive the migration");
    assert!(!row.root, "a pre-migration row is not a root");
    assert!(has("context_type"), "the guarded ALTER must add context_type");
    assert_eq!(row.context_type, None, "a pre-migration row has no context type");

    // A second pass over the already-migrated database is a no-op.
    KernelDb::apply_additive_migrations(&db.conn).unwrap();
    assert!(!has("accountable_to"));
    assert!(has("root"));
}

/// Write one `documents` row per legacy tag, bypassing `DocKind` so the
/// retired spellings reach SQLite verbatim. Returns the row ids in the
/// order the tags were given.
fn insert_raw_kind_docs(db: &KernelDb, ws_id: WorkspaceId, tags: &[&str]) -> Vec<ContextId> {
    tags.iter()
        .map(|tag| {
            let id = ContextId::new();
            db.conn
                .execute(
                    "INSERT INTO documents (
                        document_id, workspace_id, doc_kind,
                        language, path, created_at, created_by
                    ) VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6)",
                    params![
                        blob_param(id.as_bytes()),
                        blob_param(ws_id.as_bytes()),
                        tag,
                        if *tag == "code" { Some("rust") } else { None },
                        now_millis(),
                        blob_param(PrincipalId::system().as_bytes()),
                    ],
                )
                .unwrap();
            id
        })
        .collect()
}

fn raw_doc_kind(db: &KernelDb, id: ContextId) -> String {
    db.conn
        .query_row(
            "SELECT doc_kind FROM documents WHERE document_id = ?1",
            params![blob_param(id.as_bytes())],
            |row| row.get(0),
        )
        .unwrap()
}

/// `code`, `text`, and `config` collapse into `file`, and no row is lost:
/// the migration rewrites a tag, it never deletes or inserts.
#[test]
fn migrate_doc_kind_file_collapse_remaps_legacy_tags() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ids = insert_raw_kind_docs(&db, ws_id, &["code", "text", "config"]);
    let before = db.list_documents().unwrap().len();

    KernelDb::migrate_doc_kind_file_collapse(&db.conn).unwrap();

    for id in &ids {
        assert_eq!(raw_doc_kind(&db, *id), "file");
        let row = db.get_document(*id).unwrap().unwrap();
        assert_eq!(row.doc_kind, DocKind::File);
    }
    assert_eq!(
        db.list_documents().unwrap().len(),
        before,
        "the migration rewrites tags; it must not drop a row"
    );
}

/// `conversation` and `symlink` rows survive the collapse untouched — the
/// `WHERE` clause names three tags and only those three.
#[test]
fn migrate_doc_kind_file_collapse_leaves_conversation_and_symlink() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ids = insert_raw_kind_docs(&db, ws_id, &["conversation", "symlink"]);
    let before = db.list_documents().unwrap().len();

    KernelDb::migrate_doc_kind_file_collapse(&db.conn).unwrap();

    assert_eq!(raw_doc_kind(&db, ids[0]), "conversation");
    assert_eq!(raw_doc_kind(&db, ids[1]), "symlink");
    assert_eq!(
        db.get_document(ids[0]).unwrap().unwrap().doc_kind,
        DocKind::Conversation
    );
    assert_eq!(
        db.get_document(ids[1]).unwrap().unwrap().doc_kind,
        DocKind::Symlink
    );
    assert_eq!(db.list_documents().unwrap().len(), before);
}

/// Running the collapse twice is safe — the second run matches zero rows,
/// which is why it carries no `kernel_migrations` marker.
#[test]
fn migrate_doc_kind_file_collapse_is_idempotent() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ids = insert_raw_kind_docs(&db, ws_id, &["code", "text", "config"]);

    KernelDb::migrate_doc_kind_file_collapse(&db.conn).unwrap();
    KernelDb::migrate_doc_kind_file_collapse(&db.conn).unwrap();

    for id in &ids {
        assert_eq!(raw_doc_kind(&db, *id), "file");
    }
    assert_eq!(db.list_documents().unwrap().len(), 3);
}

/// The backfill migration (`migrate_well_known_lost_found`) adopts a
/// pre-existing `label = 'lost+found'` row into the registry, and running
/// it again (as every `open()`/`temporary()` does) is a no-op.
#[test]
fn migrate_well_known_lost_found_backfills_and_is_idempotent() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let legacy = make_context_row(Some("lost+found"));
    insert_context_with_doc(&db, &legacy, ws_id);

    // Pre-migration: nothing in the registry yet (temporary() already ran
    // the migration once before this row existed).
    assert_eq!(db.well_known_context(WellKnownRole::LostFound).unwrap(), None);

    KernelDb::migrate_well_known_lost_found(&db.conn).unwrap();
    assert_eq!(
        db.well_known_context(WellKnownRole::LostFound).unwrap(),
        Some(legacy.context_id)
    );

    // A second run (idempotent — INSERT OR IGNORE) must not disturb the
    // mapping, even if a rotation happened in between.
    let rotated = make_context_row(Some("lost+found-2"));
    insert_context_with_doc(&db, &rotated, ws_id);
    db.set_well_known_context(WellKnownRole::LostFound, rotated.context_id).unwrap();

    KernelDb::migrate_well_known_lost_found(&db.conn).unwrap();
    assert_eq!(
        db.well_known_context(WellKnownRole::LostFound).unwrap(),
        Some(rotated.context_id),
        "re-running the backfill must not clobber a later rotation"
    );
}

/// The backfill for databases written before archiving set the state.
/// The disagreeing row is planted the way those rows were made — the
/// timestamp alone.
#[test]
fn migrate_archived_context_state_backfills_a_disagreeing_row() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("legacy-archived"));
    insert_context_with_doc(&db, &row, ws_id);
    db.conn
        .execute(
            "UPDATE contexts SET archived_at = ?1 WHERE context_id = ?2",
            params![now_millis(), blob_param(row.context_id.as_bytes())],
        )
        .unwrap();
    assert_eq!(
        db.get_context(row.context_id).unwrap().unwrap().context_state,
        ContextState::Live,
        "the planted row must start out disagreeing, or this proves nothing"
    );

    KernelDb::migrate_archived_context_state(&db.conn).unwrap();
    assert_eq!(
        db.get_context(row.context_id).unwrap().unwrap().context_state,
        ContextState::Archived
    );

    // A live context is not swept up by it, and a second run is a no-op.
    let live = make_context_row(Some("still-live"));
    insert_context_with_doc(&db, &live, ws_id);
    KernelDb::migrate_archived_context_state(&db.conn).unwrap();
    assert_eq!(
        db.get_context(live.context_id).unwrap().unwrap().context_state,
        ContextState::Live
    );
    assert_eq!(
        db.get_context(row.context_id).unwrap().unwrap().context_state,
        ContextState::Archived
    );
}

fn index_sql(db: &KernelDb) -> String {
    db.conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'idx_contexts_label'",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn the_old_index_shape_is_rebuilt_at_open() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    db.conn
        .execute_batch(
            "DROP INDEX idx_contexts_label;
             CREATE UNIQUE INDEX idx_contexts_label
                 ON contexts(label) WHERE label IS NOT NULL;",
        )
        .unwrap();
    assert!(!index_sql(&db).contains("archived_at IS NULL"), "the planted index must be the old shape");

    KernelDb::apply_additive_migrations(&db.conn).unwrap();
    assert!(index_sql(&db).contains("archived_at IS NULL"));

    let old = make_context_row(Some("ROOT"));
    insert_context_with_doc(&db, &old, ws_id);
    assert!(db.archive_context(old.context_id).unwrap());
    insert_context_with_doc(&db, &make_context_row(Some("ROOT")), ws_id);

    // A second run is a no-op.
    KernelDb::apply_additive_migrations(&db.conn).unwrap();
    assert!(index_sql(&db).contains("archived_at IS NULL"));
}
