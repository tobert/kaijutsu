use super::*;

#[test]
fn fork_copies_binding() {
    // Permissions follow the fork: copy_context_binding clones the parent's
    // loadout so a fork is not locked out under deny-by-default.
    use crate::mcp::Capability;
    let mut db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let parent = make_context_row(Some("fork-parent"));
    let child = make_context_row(Some("fork-child"));
    insert_context_with_doc(&db, &parent, ws_id);
    insert_context_with_doc(&db, &child, ws_id);

    let mut b = ContextToolBinding::new();
    b.grant(Capability::AllInstances);
    b.grant(Capability::Facade("shell".into()));
    db.upsert_context_binding(parent.context_id, &b).unwrap();

    assert!(db.copy_context_binding(parent.context_id, child.context_id).unwrap());
    let loaded = db
        .get_context_binding(child.context_id)
        .unwrap()
        .expect("child inherited a binding");
    assert!(loaded.all_instances);
    assert!(loaded.allows(&Capability::Facade("shell".into())));
}

// ── 26. Fork context config (composite) ──────────────────────────

#[test]
fn fork_context_config_full() {
    let mut db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let src = make_context_row(Some("fork-src"));
    let tgt = make_context_row(Some("fork-tgt"));
    insert_context_with_doc(&db, &src, ws_id);
    insert_context_with_doc(&db, &tgt, ws_id);

    // Set up source with shell config + env vars
    db.upsert_context_shell(&ContextShellRow {
        context_id: src.context_id,
        cwd: Some("/home/user/src/kaijutsu".into()),
        updated_at: now_millis(),
    })
    .unwrap();
    db.set_context_env(src.context_id, "RUST_LOG", "debug")
        .unwrap();
    db.set_context_env(src.context_id, "EDITOR", "vim").unwrap();
    db.set_context_env(src.context_id, "TERM", "xterm-256color")
        .unwrap();

    db.fork_context_config(src.context_id, tgt.context_id)
        .unwrap();

    // Shell config copied
    let shell = db.get_context_shell(tgt.context_id).unwrap().unwrap();
    assert_eq!(shell.cwd, Some("/home/user/src/kaijutsu".into()));

    // Env vars copied
    let vars = db.get_context_env(tgt.context_id).unwrap();
    assert_eq!(vars.len(), 3);
}

#[test]
fn fork_context_config_empty_source() {
    let mut db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let src = make_context_row(Some("empty-src"));
    let tgt = make_context_row(Some("empty-tgt"));
    insert_context_with_doc(&db, &src, ws_id);
    insert_context_with_doc(&db, &tgt, ws_id);

    // Fork from context with no config → no error, no data on target
    db.fork_context_config(src.context_id, tgt.context_id)
        .unwrap();

    assert!(db.get_context_shell(tgt.context_id).unwrap().is_none());
    assert!(db.get_context_env(tgt.context_id).unwrap().is_empty());
}

/// A fork that fails partway must leave NO partial config behind — a
/// half-copied loadout strands a misconfigured (locked-out) context. The
/// three copies share one transaction, so a failure on a later write must
/// roll back the earlier ones. Force the env write (step 2) to abort after
/// the shell write (step 1) has already executed, then prove the shell row
/// never lands. Against the old per-write autocommit, the shell row would
/// survive and this fails.
#[test]
fn fork_context_config_rolls_back_on_partial_failure() {
    let mut db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let src = make_context_row(Some("atomic-src"));
    let tgt = make_context_row(Some("atomic-tgt"));
    insert_context_with_doc(&db, &src, ws_id);
    insert_context_with_doc(&db, &tgt, ws_id);

    // Source has shell config (copied first) AND an env var (copied second).
    db.upsert_context_shell(&ContextShellRow {
        context_id: src.context_id,
        cwd: Some("/work".into()),
        updated_at: now_millis(),
    })
    .unwrap();
    db.set_context_env(src.context_id, "RUST_LOG", "debug")
        .unwrap();

    // Booby-trap the env INSERT: a BEFORE INSERT trigger that aborts the
    // statement. Source reads are SELECTs and unaffected — only the
    // target's env write trips it, after the shell write has run in the tx.
    db.conn
        .execute_batch(
            "CREATE TRIGGER boom BEFORE INSERT ON context_env
             BEGIN SELECT RAISE(ABORT, 'forced'); END;",
        )
        .unwrap();

    let result = db.fork_context_config(src.context_id, tgt.context_id);
    assert!(result.is_err(), "the aborted env write must surface as an error");

    // Remove the trap; SELECTs below don't fire it, but keep the db clean.
    db.conn.execute_batch("DROP TRIGGER boom;").unwrap();

    // The shell write succeeded *before* the abort, yet must be gone: the
    // transaction rolled back on drop. No partial fork survived.
    assert!(
        db.get_context_shell(tgt.context_id).unwrap().is_none(),
        "shell copy must roll back when a later fork write fails",
    );
    assert!(
        db.get_context_env(tgt.context_id).unwrap().is_empty(),
        "no env rows on a rolled-back fork",
    );
}

/// The composite fork — document row + context row + shell/env/binding + attachment
/// copy — lands in one shot. Insert a source with full config, fork it into a
/// brand-new target via `insert_forked_context`, and prove every piece arrived.
#[test]
fn insert_forked_context_creates_rows_and_copies_config() {
    let mut db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let src = make_context_row(Some("ifc-src"));
    insert_context_with_doc(&db, &src, ws_id);

    db.upsert_context_shell(&ContextShellRow {
        context_id: src.context_id,
        cwd: Some("/work/kaijutsu".into()),
        updated_at: now_millis(),
    })
    .unwrap();
    db.set_context_env(src.context_id, "RUST_LOG", "debug").unwrap();
    db.upsert_context_binding(
        src.context_id,
        &binding_with(&["builtin.file"], &[("read", "builtin.file", "file_read")]),
    )
    .unwrap();
    // Attachment carries the track binding into the child — the child joins
    // the same clock domain the parent is on (docs/tracks.md §3).
    db.upsert_track(&make_track("bass", 500)).unwrap();
    db.upsert_attachment(&PersistedAttachment {
        wakeup_every: 8,
        rotate_every_phrases: Some(4),
        ooda_armed: true,
        ..make_attachment("bass", src.context_id)
    })
    .unwrap();

    // The target is a fresh context that does not yet exist in any table.
    let tgt = make_context_row(Some("ifc-tgt"));
    db.insert_forked_context(&tgt, ws_id, src.context_id, Some("/work/kaijutsu")).unwrap();

    // Both the document row and the context row were created.
    assert!(db.get_document(tgt.context_id).unwrap().is_some());
    assert!(db.get_context(tgt.context_id).unwrap().is_some());

    // The given cwd landed; env + binding followed the fork.
    assert_eq!(
        db.get_context_shell(tgt.context_id).unwrap().unwrap().cwd,
        Some("/work/kaijutsu".into()),
    );
    assert_eq!(db.get_context_env(tgt.context_id).unwrap().len(), 1);
    assert!(db.get_context_binding(tgt.context_id).unwrap().is_some());

    // Attachment followed the fork — child joins the same track (bass), keeping
    // the wakeup divisor and rotate cadence from the parent.
    let child_att = db.get_attachment("bass", tgt.context_id).unwrap().unwrap();
    assert_eq!(child_att.track_id, "bass", "child attached to parent's track");
    assert_eq!(child_att.wakeup_every, 8, "wakeup divisor carried");
    assert_eq!(child_att.rotate_every_phrases, Some(4), "rotate cadence carried");
    assert!(child_att.ooda_armed, "ooda_armed carried");
}

/// The whole fork is all-or-nothing: if a *later* write (the config copy)
/// fails, the context + document rows inserted *earlier* in the same
/// transaction must roll back too. Otherwise a fork could strand a
/// committed-but-misconfigured context — the exact gap this method closes.
/// Booby-trap the target's env INSERT, then prove neither row survived.
/// Against the old `insert_context_with_document` + `fork_context_config`
/// pair (two separate autocommits), the context row would persist.
#[test]
fn insert_forked_context_rolls_back_rows_on_config_failure() {
    let mut db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let src = make_context_row(Some("ifc-atomic-src"));
    insert_context_with_doc(&db, &src, ws_id);
    // Source has an env var, so the fork will attempt a target env write.
    db.set_context_env(src.context_id, "RUST_LOG", "debug").unwrap();

    // Abort any INSERT into context_env. The source's var is already in
    // place (inserted above); only the fork's target write trips this,
    // after the context + document rows have run in the transaction.
    db.conn
        .execute_batch(
            "CREATE TRIGGER boom BEFORE INSERT ON context_env
             BEGIN SELECT RAISE(ABORT, 'forced'); END;",
        )
        .unwrap();

    let tgt = make_context_row(Some("ifc-atomic-tgt"));
    let result = db.insert_forked_context(&tgt, ws_id, src.context_id, None);
    assert!(result.is_err(), "the aborted env write must surface as an error");

    db.conn.execute_batch("DROP TRIGGER boom;").unwrap();

    // The context + document inserts succeeded earlier in the tx, yet must
    // be gone: the transaction rolled back on drop. No half-built fork.
    assert!(
        db.get_context(tgt.context_id).unwrap().is_none(),
        "context row must roll back when the config copy fails",
    );
    assert!(
        db.get_document(tgt.context_id).unwrap().is_none(),
        "document row must roll back when the config copy fails",
    );
}

/// A fork keeps its parent's loadout but not the grants for MCP servers
/// a client declared for the parent alone.
#[test]
fn insert_forked_context_drops_context_scoped_mcp_grants() {
    use crate::mcp::{Capability, ContextToolBinding, InstanceId};
    let mut db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let src = make_context_row(Some("ifc-scoped-src"));
    insert_context_with_doc(&db, &src, ws_id);
    let scoped = InstanceId::new(format!("context.{}.fixture", src.context_id.to_hex()));
    let mut binding = ContextToolBinding::new();
    binding.grant(Capability::AllInstances);
    binding.grant(Capability::Instance(scoped.clone()));
    db.upsert_context_binding(src.context_id, &binding).unwrap();

    let tgt = make_context_row(Some("ifc-scoped-tgt"));
    db.insert_forked_context(&tgt, ws_id, src.context_id, None).unwrap();

    let child = db.get_context_binding(tgt.context_id).unwrap().unwrap();
    assert!(child.all_instances, "the loadout follows the fork");
    assert!(
        !child.allows(&Capability::Instance(scoped)),
        "a parent's context-scoped MCP grant must not follow the fork"
    );
}

