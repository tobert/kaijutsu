use super::*;

#[test]
fn context_shell_upsert_and_get() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("shell-test"));
    insert_context_with_doc(&db, &ctx, ws_id);

    // Initially none
    assert!(db.get_context_shell(ctx.context_id).unwrap().is_none());

    // Insert
    let row = ContextShellRow {
        context_id: ctx.context_id,
        cwd: Some("/home/user/src/kaijutsu".into()),
        updated_at: now_millis(),
    };
    db.upsert_context_shell(&row).unwrap();

    let loaded = db.get_context_shell(ctx.context_id).unwrap().unwrap();
    assert_eq!(loaded.cwd, Some("/home/user/src/kaijutsu".into()));

    // Update (upsert changes cwd)
    let row2 = ContextShellRow {
        context_id: ctx.context_id,
        cwd: Some("/tmp/work".into()),
        updated_at: now_millis(),
    };
    db.upsert_context_shell(&row2).unwrap();

    let loaded = db.get_context_shell(ctx.context_id).unwrap().unwrap();
    assert_eq!(loaded.cwd, Some("/tmp/work".into()));
}

#[test]
fn context_shell_get_unknown() {
    let db = KernelDb::temporary().unwrap();
    assert!(db.get_context_shell(ContextId::new()).unwrap().is_none());
}

#[test]
fn context_shell_copy() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let src = make_context_row(Some("src"));
    let tgt = make_context_row(Some("tgt"));
    insert_context_with_doc(&db, &src, ws_id);
    insert_context_with_doc(&db, &tgt, ws_id);

    // Copy from context with shell config
    let row = ContextShellRow {
        context_id: src.context_id,
        cwd: Some("/home/user/project".into()),
        updated_at: now_millis(),
    };
    db.upsert_context_shell(&row).unwrap();

    assert!(
        db.copy_context_shell(src.context_id, tgt.context_id)
            .unwrap()
    );

    let copied = db.get_context_shell(tgt.context_id).unwrap().unwrap();
    assert_eq!(copied.cwd, Some("/home/user/project".into()));
}

#[test]
fn context_shell_copy_empty() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let src = make_context_row(Some("src"));
    let tgt = make_context_row(Some("tgt"));
    insert_context_with_doc(&db, &src, ws_id);
    insert_context_with_doc(&db, &tgt, ws_id);

    // Copy from context with no shell config → returns false
    assert!(
        !db.copy_context_shell(src.context_id, tgt.context_id)
            .unwrap()
    );
    assert!(db.get_context_shell(tgt.context_id).unwrap().is_none());
}

#[test]
fn context_shell_cascade_delete() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("cascade"));
    insert_context_with_doc(&db, &ctx, ws_id);

    db.upsert_context_shell(&ContextShellRow {
        context_id: ctx.context_id,
        cwd: Some("/tmp".into()),
        updated_at: now_millis(),
    })
    .unwrap();
    assert!(db.get_context_shell(ctx.context_id).unwrap().is_some());

    // Delete context → shell row should cascade
    db.conn.execute("DELETE FROM contexts WHERE context_id = ?1",
        params![blob_param(ctx.context_id.as_bytes())]).unwrap();
    assert!(db.get_context_shell(ctx.context_id).unwrap().is_none());
}

// ── 24. Context env CRUD ──────────────────────────────────────────

#[test]
fn context_env_set_and_get() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("env-test"));
    insert_context_with_doc(&db, &ctx, ws_id);

    // Initially empty
    let vars = db.get_context_env(ctx.context_id).unwrap();
    assert!(vars.is_empty());

    // Set vars
    db.set_context_env(ctx.context_id, "RUST_LOG", "debug")
        .unwrap();
    db.set_context_env(ctx.context_id, "EDITOR", "vim").unwrap();

    let vars = db.get_context_env(ctx.context_id).unwrap();
    assert_eq!(vars.len(), 2);
    // Ordered by key
    assert_eq!(vars[0].key, "EDITOR");
    assert_eq!(vars[0].value, "vim");
    assert_eq!(vars[1].key, "RUST_LOG");
    assert_eq!(vars[1].value, "debug");
}

#[test]
fn context_env_upsert() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("env-upsert"));
    insert_context_with_doc(&db, &ctx, ws_id);

    db.set_context_env(ctx.context_id, "RUST_LOG", "debug")
        .unwrap();
    db.set_context_env(ctx.context_id, "RUST_LOG", "trace")
        .unwrap();

    let vars = db.get_context_env(ctx.context_id).unwrap();
    assert_eq!(vars.len(), 1);
    assert_eq!(vars[0].value, "trace");
}

#[test]
fn context_env_get_empty() {
    let db = KernelDb::temporary().unwrap();
    let vars = db.get_context_env(ContextId::new()).unwrap();
    assert!(vars.is_empty());
}

#[test]
fn context_env_delete() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("env-del"));
    insert_context_with_doc(&db, &ctx, ws_id);

    db.set_context_env(ctx.context_id, "FOO", "bar").unwrap();
    assert!(db.delete_context_env(ctx.context_id, "FOO").unwrap());
    assert!(!db.delete_context_env(ctx.context_id, "FOO").unwrap()); // already gone
    assert!(!db.delete_context_env(ctx.context_id, "NEVER_SET").unwrap());
}

/// A key that can never become `export KEY=...` must be refused at the
/// durable write — not stored and only discovered the next time a shell
/// materializes the context. `1BAD` starts with a digit, which
/// `is_valid_env_key` (`runtime/embedded_kaish.rs`) also rejects since
/// it can't lead a shell identifier.
#[test]
fn context_env_rejects_a_key_that_is_not_a_shell_identifier() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("env-bad-key"));
    insert_context_with_doc(&db, &ctx, ws_id);

    let err = db.set_context_env(ctx.context_id, "1BAD", "x").unwrap_err();
    match err {
        KernelDbError::InvalidEnvKey(msg) => {
            assert!(msg.contains("1BAD"), "error must name the offending key: {msg}");
        }
        other => panic!("expected InvalidEnvKey, got {other:?}"),
    }
    assert!(
        db.get_context_env(ctx.context_id).unwrap().is_empty(),
        "a rejected key must never reach the table"
    );
}

/// The permissive edge: an underscore-led key with digits and mixed
/// case is a legal shell identifier and must be accepted — this is the
/// same rule `is_valid_env_key` applies, not a stricter one.
#[test]
fn context_env_accepts_underscore_led_alnum_key() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("env-good-key"));
    insert_context_with_doc(&db, &ctx, ws_id);

    db.set_context_env(ctx.context_id, "_Kaiju_2", "ok").unwrap();
    let vars = db.get_context_env(ctx.context_id).unwrap();
    assert_eq!(vars.len(), 1);
    assert_eq!(vars[0].key, "_Kaiju_2");
    assert_eq!(vars[0].value, "ok");
}

#[test]
fn context_env_clear() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("env-clear"));
    insert_context_with_doc(&db, &ctx, ws_id);

    db.set_context_env(ctx.context_id, "A", "1").unwrap();
    db.set_context_env(ctx.context_id, "B", "2").unwrap();
    db.set_context_env(ctx.context_id, "C", "3").unwrap();

    assert_eq!(db.clear_context_env(ctx.context_id).unwrap(), 3);
    assert!(db.get_context_env(ctx.context_id).unwrap().is_empty());
}

#[test]
fn context_env_copy() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let src = make_context_row(Some("env-src"));
    let tgt = make_context_row(Some("env-tgt"));
    insert_context_with_doc(&db, &src, ws_id);
    insert_context_with_doc(&db, &tgt, ws_id);

    db.set_context_env(src.context_id, "RUST_LOG", "debug")
        .unwrap();
    db.set_context_env(src.context_id, "EDITOR", "vim").unwrap();
    db.set_context_env(src.context_id, "SHELL", "/bin/bash")
        .unwrap();

    let count = db.copy_context_env(src.context_id, tgt.context_id).unwrap();
    assert_eq!(count, 3);

    let vars = db.get_context_env(tgt.context_id).unwrap();
    assert_eq!(vars.len(), 3);
}

#[test]
fn context_env_cascade_delete() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("env-cascade"));
    insert_context_with_doc(&db, &ctx, ws_id);

    db.set_context_env(ctx.context_id, "FOO", "bar").unwrap();
    db.set_context_env(ctx.context_id, "BAZ", "qux").unwrap();

    db.conn.execute("DELETE FROM contexts WHERE context_id = ?1",
        params![blob_param(ctx.context_id.as_bytes())]).unwrap();
    let vars = db.get_context_env(ctx.context_id).unwrap();
    assert!(vars.is_empty());
}


#[test]
fn context_egress_add_list_remove_round_trip() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("egress-crud"));
    insert_context_with_doc(&db, &ctx, ws_id);

    assert!(db.list_context_egress(ctx.context_id).unwrap().is_empty());

    db.add_context_egress(ctx.context_id, "crates.io").unwrap();
    db.add_context_egress(ctx.context_id, "127.0.0.1").unwrap();
    // Adding the same host twice is idempotent, not a conflict.
    db.add_context_egress(ctx.context_id, "crates.io").unwrap();

    let hosts = db.list_context_egress(ctx.context_id).unwrap();
    assert_eq!(hosts, vec!["127.0.0.1".to_string(), "crates.io".to_string()]);

    assert!(db.remove_context_egress(ctx.context_id, "crates.io").unwrap());
    // Already gone — a second removal reports nothing to remove.
    assert!(!db.remove_context_egress(ctx.context_id, "crates.io").unwrap());
    assert!(!db.remove_context_egress(ctx.context_id, "never-added.example").unwrap());

    let hosts = db.list_context_egress(ctx.context_id).unwrap();
    assert_eq!(hosts, vec!["127.0.0.1".to_string()]);
}

#[test]
fn context_egress_copy() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let src = make_context_row(Some("egress-src"));
    let tgt = make_context_row(Some("egress-tgt"));
    insert_context_with_doc(&db, &src, ws_id);
    insert_context_with_doc(&db, &tgt, ws_id);

    db.add_context_egress(src.context_id, "crates.io").unwrap();
    db.add_context_egress(src.context_id, "127.0.0.1").unwrap();

    let count = db.copy_context_egress(src.context_id, tgt.context_id).unwrap();
    assert_eq!(count, 2);

    let hosts = db.list_context_egress(tgt.context_id).unwrap();
    assert_eq!(hosts, vec!["127.0.0.1".to_string(), "crates.io".to_string()]);
}

#[test]
fn context_egress_cascade_delete() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("egress-cascade"));
    insert_context_with_doc(&db, &ctx, ws_id);

    db.add_context_egress(ctx.context_id, "crates.io").unwrap();
    db.conn.execute("DELETE FROM contexts WHERE context_id = ?1",
        params![blob_param(ctx.context_id.as_bytes())]).unwrap();
    assert!(db.list_context_egress(ctx.context_id).unwrap().is_empty());
}

/// `add_context_egress`/`remove_context_egress` reject a bad host the
/// same way `set_context_env` rejects a bad key — refused at the durable
/// write, not just at the kj surface.
#[test]
fn context_egress_add_rejects_an_invalid_host() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("egress-invalid"));
    insert_context_with_doc(&db, &ctx, ws_id);

    let err = db.add_context_egress(ctx.context_id, "https://x").unwrap_err();
    assert!(matches!(err, KernelDbError::InvalidEgressHost(_)), "{err}");
    assert!(
        db.list_context_egress(ctx.context_id).unwrap().is_empty(),
        "a rejected host must not land a partial row"
    );
}

/// A `kernel.db` created before `context_egress` existed still gets the
/// table on open — `SCHEMA`'s `CREATE TABLE IF NOT EXISTS` runs on every
/// open, so a pre-existing file gains it without a dedicated migration.
/// Mirrors `open_migrates_the_ledger_schema_and_reopen_is_idempotent`:
/// build a real DB, then simulate "before this table existed" by
/// dropping it, closing the connection, and reopening the same file.
#[test]
fn context_egress_table_appears_on_a_db_opened_without_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    {
        let db = KernelDb::open(&path).unwrap();
        db.conn.execute_batch("DROP TABLE context_egress;").unwrap();
    }
    // Drop closed the connection; re-open the same file.
    let db = KernelDb::open(&path).unwrap();
    // A DB with no context_egress rows opens cleanly and the table is
    // queryable — this would fail with "no such table" before open()
    // ran the current SCHEMA against it.
    assert!(db.list_context_egress(ContextId::new()).unwrap().is_empty());
}
