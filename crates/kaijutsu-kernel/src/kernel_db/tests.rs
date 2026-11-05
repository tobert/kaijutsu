use super::*;

#[test]
fn fork_kind_from_sql_fails_loud_on_unknown() {
    // NULL column → None; known value parses.
    assert!(fork_kind_from_sql(None).unwrap().is_none());
    assert_eq!(
        fork_kind_from_sql(Some("filtered".into())).unwrap(),
        Some(ForkKind::Filtered)
    );
    // A retired ('shallow', 'subtree') or corrupt value must ERROR, never
    // silently degrade to None — that would erase fork provenance.
    assert!(fork_kind_from_sql(Some("shallow".into())).is_err());
    assert!(fork_kind_from_sql(Some("subtree".into())).is_err());
    assert!(fork_kind_from_sql(Some("garbage".into())).is_err());
}

// ── 1. Schema idempotent ────────────────────────────────────────────

#[test]
fn schema_idempotent() {
    let db = KernelDb::temporary().unwrap();
    // Apply schema again — should not error.
    db.conn.execute_batch(SCHEMA).unwrap();
}

/// `is_duplicate_column_error` is the guard that lets
/// `apply_additive_migrations` re-run safely against an already-migrated
/// DB — it must match SQLite's actual "duplicate column name" wording and
/// nothing else (a different message must not be misclassified as the
/// ignorable case and swallowed).
#[test]
fn duplicate_column_error_detection_is_specific() {
    let db = KernelDb::temporary().unwrap();
    let dup = db
        .conn
        .execute(
            "ALTER TABLE tracks ADD COLUMN deleted_at INTEGER",
            [],
        )
        .unwrap_err();
    assert!(is_duplicate_column_error(&dup), "expected duplicate-column: {dup}");

    let other = db
        .conn
        .execute("ALTER TABLE no_such_table ADD COLUMN x INTEGER", [])
        .unwrap_err();
    assert!(
        !is_duplicate_column_error(&other),
        "a no-such-table failure must not be misclassified as duplicate-column: {other}"
    );
}

// ── approval-ledger migration ──────────────────────────────────────

// ── synchronous pragma ────────────────────────────────────────────

/// `init_connection` must set `synchronous = NORMAL` (1), not SQLite's
/// default `FULL` (2). Under WAL, NORMAL fsyncs at checkpoint instead of
/// on every commit, and a streaming turn commits once per delta.
#[test]
fn init_connection_sets_synchronous_normal() {
    let db = KernelDb::temporary().unwrap();
    let mode: i64 = db
        .conn_for_ledger()
        .query_row("PRAGMA synchronous", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, 1, "synchronous should be NORMAL (1), got {mode}");
}

// ── journal size limit ───────────────────────────────────────────────

/// `init_connection` must cap the WAL's high-water mark with
/// `journal_size_limit`, or SQLite's default of -1 (no limit) lets a busy
/// kernel's `-wal` file grow without bound and never shrink back down,
/// since a checkpoint only truncates a WAL larger than this limit.
#[test]
fn init_connection_sets_journal_size_limit() {
    let db = KernelDb::temporary().unwrap();
    let limit: i64 = db
        .conn_for_ledger()
        .query_row("PRAGMA journal_size_limit", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        limit, 67_108_864,
        "journal_size_limit should be 67108864 (64 MiB), got {limit}"
    );
}

// ── WAL checkpoint ──────────────────────────────────────────────────

/// `checkpoint()` (TRUNCATE) must flush committed frames into the main
/// file and shrink the `-wal` file back to zero. Guards the proactive
/// checkpoint that compaction relies on so a bare-file read of the main
/// `.db` stops lagging committed history (2026-06-11 forensics). On-disk
/// db required — a `:memory:` connection has no WAL file.
#[test]
fn checkpoint_truncates_wal() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ckpt.db");
    let wal_path = dir.path().join("ckpt.db-wal");

    let db = KernelDb::open(&db_path).unwrap();
    let ws_id = db
        .get_or_create_default_workspace(PrincipalId::system())
        .unwrap();
    let doc = ContextId::new();
    db.insert_document(&DocumentRow {
        document_id: doc,
        workspace_id: ws_id,
        doc_kind: DocKind::Conversation,
        language: None,
        path: None,
        created_at: now_millis(),
        created_by: PrincipalId::system(),
    })
    .unwrap();

    // 200 × 4 KiB ops ≈ 800 KiB — grows the WAL but stays under SQLite's
    // 1000-page (~4 MiB) auto-checkpoint, so the WAL is non-empty and
    // un-truncated when we reach the manual checkpoint below.
    let payload = vec![0xABu8; 4096];
    for seq in 1..=200 {
        db.append_op(doc, seq, &payload).unwrap();
    }

    let wal_before = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
    assert!(
        wal_before > 0,
        "WAL should have grown after 200 appends, got {wal_before} bytes",
    );

    let (busy, _log, _checkpointed) = db.checkpoint().unwrap();
    assert_eq!(busy, 0, "single-connection checkpoint must not be busy");

    // The behavioral proof: TRUNCATE shrinks the -wal file back to zero,
    // so a bare-file read of the main .db now reflects all 200 ops. (The
    // log/checkpointed frame counts are version-dependent for TRUNCATE and
    // not asserted.)
    let wal_after = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
    assert_eq!(
        wal_after, 0,
        "TRUNCATE checkpoint should zero the -wal file, got {wal_after} bytes",
    );
}

// ── VACUUM INTO backup ──────────────────────────────────────────────

/// `vacuum_into` must produce a real, non-empty file — the behavioral
/// guard against a silent no-op (e.g. a typo'd pragma or a swallowed
/// error that still returns `Ok`). A bug that made `vacuum_into` write
/// nothing would still pass a test that only checked `is_ok()`; this one
/// also asserts the file exists and has SQLite's magic header size.
#[test]
fn vacuum_into_produces_nonempty_file() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("source.db");
    let backup_path = dir.path().join("backup.db");

    let db = KernelDb::open(&db_path).unwrap();
    db.get_or_create_default_workspace(PrincipalId::system())
        .unwrap();

    assert!(!backup_path.exists(), "precondition: backup must not exist yet");
    db.vacuum_into(&backup_path).unwrap();

    let meta = std::fs::metadata(&backup_path)
        .expect("vacuum_into must create the backup file");
    assert!(
        meta.len() >= 100,
        "backup file too small to be a real SQLite db ({} bytes) — vacuum_into may have \
         written nothing",
        meta.len()
    );
}

/// The backup must be a fully valid, queryable SQLite database containing
/// the same rows as the source — not just a non-empty file. Opens the
/// backup as an independent `KernelDb` and checks it has the same
/// singleton kernel id and the workspace/document rows written before
/// the backup was taken.
#[test]
fn vacuum_into_backup_is_valid_and_contains_expected_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("source.db");
    let backup_path = dir.path().join("backup.db");

    let db = KernelDb::open(&db_path).unwrap();
    let ws_id = db
        .get_or_create_default_workspace(PrincipalId::system())
        .unwrap();
    let doc = ContextId::new();
    db.insert_document(&DocumentRow {
        document_id: doc,
        workspace_id: ws_id,
        doc_kind: DocKind::Conversation,
        language: None,
        path: None,
        created_at: now_millis(),
        created_by: PrincipalId::system(),
    })
    .unwrap();
    let source_kernel_id = db.kernel_id().unwrap();

    db.vacuum_into(&backup_path).unwrap();

    // Open the backup as an independent connection — proves it's a
    // structurally valid SQLite file with the full schema, not a
    // truncated or corrupt copy.
    let backup = KernelDb::open(&backup_path).unwrap();
    assert_eq!(
        backup.kernel_id().unwrap(),
        source_kernel_id,
        "backup must carry the same singleton kernel identity"
    );
    let restored_doc = backup
        .get_document(doc)
        .expect("query backup db")
        .expect("document present in backup");
    assert_eq!(restored_doc.doc_kind, DocKind::Conversation);
}

/// SQLite refuses to `VACUUM INTO` an existing file. The error must
/// surface cleanly (not panic, not silently succeed by overwriting) —
/// `kj db backup` relies on this to avoid clobbering a same-named backup.
#[test]
fn vacuum_into_target_exists_errors_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("source.db");
    let backup_path = dir.path().join("backup.db");
    std::fs::write(&backup_path, b"not a database, just occupying the name").unwrap();

    let db = KernelDb::open(&db_path).unwrap();
    let result = db.vacuum_into(&backup_path);

    assert!(
        result.is_err(),
        "vacuum_into must error when the target file already exists"
    );
    // The pre-existing (bogus) file content must be left untouched —
    // vacuum_into must not have deleted-then-recreated it.
    let contents = std::fs::read(&backup_path).unwrap();
    assert_eq!(contents, b"not a database, just occupying the name");
}

/// A path with a `'` — the SQL string-literal delimiter — must not be
/// able to break out of the `VACUUM INTO` statement. Guards the bound-
/// parameter choice (`?1`) over string interpolation.
#[test]
fn vacuum_into_path_with_quote_is_bound_not_interpolated() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("source.db");
    let backup_path = dir.path().join("back'up.db");

    let db = KernelDb::open(&db_path).unwrap();
    db.vacuum_into(&backup_path).unwrap();

    assert!(
        backup_path.exists(),
        "a bound parameter should handle a quote in the filename literally"
    );
}

// ── 2. Context lifecycle ────────────────────────────────────────────

// ── 3. Label validation: colon ──────────────────────────────────────

#[test]
fn label_validation_colon() {
    let db = KernelDb::temporary().unwrap();
    let row = make_context_row(Some("my:label"));

    let err = db.insert_context(&row).unwrap_err();
    assert!(matches!(err, KernelDbError::InvalidLabel(_)));
}

// ── 4. Label uniqueness ────────────────────────────────────────────

// ── 5. Fork lineage 3 deep ─────────────────────────────────────────

// ── 8. Drift edge allows dupes ─────────────────────────────────────

// ── 10. Preset lifecycle ───────────────────────────────────────────

// ── 11. Workspace lifecycle ────────────────────────────────────────

// ── 12. Workspace soft delete ──────────────────────────────────────

// ── 13. Drift push creates edge ────────────────────────────────────

// ── 14. Context DAG recursive ──────────────────────────────────────

// ── 15. Tag resolution (label + prefix) ────────────────────────────

// ── 16. Resolve context basic ──────────────────────────────────────

// ── 17. Null labels coexist ────────────────────────────────────────

// ── 18. Archive excludes from active + fork_children ─────────

// ── 19. Context edge cascade ───────────────────────────────────────

// ── 20. Context row to_context conversion ──────────────────────────

// ── 21. Roundtrip: create context, read back, verify all 15 fields ──

// ── 22. FK violation produces Validation, not LabelConflict ──────────

#[test]
fn fk_violation_is_validation_error() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);

    // Reference a workspace_id that doesn't exist on the context
    let ctx_id = ContextId::new();
    // Insert document with valid workspace first
    db.insert_document(&DocumentRow {
        document_id: ctx_id,
                    workspace_id: ws_id,
        doc_kind: DocKind::Conversation,
        language: None,
        path: None,
        created_at: now_millis(),
        created_by: PrincipalId::new(),
    })
    .unwrap();

    let row = ContextRow {
        context_id: ctx_id,
        label: Some("fk-test".into()),
        system_prompt: None,
        context_state: ContextState::Live,
        created_at: now_millis(),
        created_by: PrincipalId::new(),
        forked_from: None,
        fork_kind: None,
        archived_at: None,
        workspace_id: Some(WorkspaceId::new()), // doesn't exist
        preset_id: None,
        concluded_at: None,
        last_activity_at: None,
        promoted_at: None,
        demoted_at: None,
        paused_at: None,
        origin_host: None,
        performer: None, senpai_ctx: None, persona: None,
    };
    let err = db.insert_context(&row).unwrap_err();
    assert!(
        matches!(err, KernelDbError::Validation(_)),
        "expected Validation for FK violation, got: {err}"
    );
}

// ── 22b. Characters CRUD ──────────────────────────────────────────

// ── 22c. the `root` flag ────────────────────────────────────────────

// ── 22e. the senpai migration ───────────────────────────────────────

/// `open_read_only` reads an existing `kernel.db` without running
/// `SCHEMA` or migrations — the lockout-recovery path
/// (`kaijutsu-server list-characters`), which must work with the
/// service stopped and must never contend with a live server's write
/// connection.
#[test]
fn open_read_only_reads_an_existing_db_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    let character = PrincipalId::new();
    {
        let db = KernelDb::open(&path).unwrap();
        db.insert_character(&CharacterRow { rc_dir: Some("coder".into()), console_ctx: None, mcp_hub_ctx: None,
            principal_id: character,
            name: "hajime".to_string(),
            created_at: 1000,
            retired_at: None,
            handoff_ctx: None, root_ctx: None, root: false,
        })
        .unwrap();
    }

    let ro = KernelDb::open_read_only(&path).unwrap();
    let row = ro.get_character_by_name("hajime").unwrap().unwrap();
    assert_eq!(row.principal_id, character);

    // A write attempt through the read-only connection must fail loudly
    // rather than silently succeeding — the whole point of the mode.
    let err = ro
        .insert_character(&CharacterRow { rc_dir: Some("coder".into()), console_ctx: None, mcp_hub_ctx: None,
            principal_id: PrincipalId::new(),
            name: "intruder".to_string(),
            created_at: 2000,
            retired_at: None,
            handoff_ctx: None, root_ctx: None, root: false,
        })
        .unwrap_err();
    assert!(
        matches!(err, KernelDbError::Db(_)),
        "a write on a read-only connection must fail, got: {err}"
    );
}

/// `open_read_only` against a path with no database must fail loudly —
/// there is nothing sensible to fall back to (`docs/character.md`'s
/// "corruption, never a fallback" rule applies here too).
#[test]
fn open_read_only_fails_loudly_on_a_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("no-such-kernel.db");
    assert!(KernelDb::open_read_only(&missing).is_err());
}

// ── 23. Context shell CRUD ────────────────────────────────────────

// ── 23b. Context tool bindings CRUD (Phase 5, D-54) ──────────────
//
// Normalized schema: parent `context_bindings` + `_instances` (ordered)
// + `_names` (sticky map). The get path reconstructs `ContextToolBinding`
// by joining; the upsert path writes all three tables transactionally.

// ── Context egress (docs/egress.md) ─────────────────────────────

#[test]
fn validate_egress_host_accepts_dns_names_loopback_literals_and_star() {
    assert_eq!(validate_egress_host("crates.io").unwrap(), "crates.io");
    assert_eq!(validate_egress_host("LOCALHOST").unwrap(), "localhost");
    assert_eq!(validate_egress_host("127.0.0.1").unwrap(), "127.0.0.1");
    assert_eq!(validate_egress_host("::1").unwrap(), "::1");
    assert_eq!(validate_egress_host("0:0:0:0:0:0:0:1").unwrap(), "::1");
    assert_eq!(validate_egress_host("*").unwrap(), "*");
}

#[test]
fn validate_egress_host_rejects_scheme_path_port_wildcard_empty_and_space() {
    for bad in ["https://x", "x/path", "x:443", "*.x.io", "", "x y"] {
        let err = validate_egress_host(bad).unwrap_err();
        assert!(matches!(err, KernelDbError::InvalidEgressHost(_)), "{bad}: {err}");
    }
}

// ── Fork inheritance ──────────────────────────────────────────────

// ── 25. Workspace paths with read_only ────────────────────────────

// ── 27. Context workspace paths query ─────────────────────────────

// ── 28. Workspace path permission checking ────────────────────────

// ── 29. Time-well ring placement (promote/demote/pause) ────────────

// ── 31. Well-known context registry (docs/drifting-dead-letters.md, slice 1) ──

/// `temporary()` must open a real file on disk, not `:memory:` — and that
/// file (and the directory holding it) must be gone once nothing holds
/// the `KernelDb` anymore. Asserted against the filesystem directly, not
/// against any in-process bookkeeping.
#[test]
fn temporary_writes_a_real_file_and_removes_it_on_drop() {
    let db = KernelDb::temporary().unwrap();
    let dir = db._temp_dir.as_ref().unwrap().path().to_path_buf();
    let db_path = dir.join("kernel.db");
    assert!(db_path.is_file(), "temporary() must back its handle with a real file");

    drop(db);

    assert!(
        !dir.exists(),
        "the temp directory must be removed once the KernelDb handle drops"
    );
}

// ========================================================================
// insert_document_with_op — the row and the first op commit together or
// not at all. A row that commits alone is a document with no snapshot and
// no oplog: it loads empty and every read of it fails.
// ========================================================================

// ========================================================================
// `context_state` and `archived_at` are written together. Before this,
// archiving stamped only the timestamp, so every archived context read
// back as `live` and the enum lied to anyone who trusted it alone.
// ========================================================================
