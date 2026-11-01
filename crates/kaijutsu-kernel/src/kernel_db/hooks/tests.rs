use super::*;

// ── 23b. Hook persistence (hook persistence follow-up) ────────────
//
// Hooks are global (not per-context), so these tests don't need a
// context row at all. Insert writes one row per entry with the DB
// computing `insertion_idx` internally; load returns rows ordered
// by `(phase, priority, insertion_idx)` so the broker can rebuild
// `HookTable.entries` in the same Vec order as before restart.

#[test]
fn hook_insert_roundtrip_preserves_all_action_variants() {
    // One insert per action_kind. Round-trip must preserve every
    // variant-specific column and every match field so the broker's
    // reconstruction loses no information.
    let db = KernelDb::temporary().unwrap();

    // Builtin-invoke with full match fields.
    let ctx_id = ContextId::new();
    let principal = PrincipalId::new();
    let builtin = HookRow {
        hook_id: "h-builtin".into(),
        phase: "pre_call".into(),
        priority: 10,
        match_instance: Some("builtin.*".into()),
        match_tool: Some("file_*".into()),
        match_context: Some(ctx_id),
        match_principal: Some(principal),
        action_kind: "builtin_invoke".into(),
        action_builtin_name: Some("tracing_audit".into()),
        action_kaish_body: None,
        action_kaish_script_id: None,
        action_kaish_path: None,
        action_result_text: None,
        action_is_error: None,
        action_deny_reason: None,
        action_log_target: None,
        action_log_level: None,
        action_ask_description: None,
    };
    db.insert_hook(&builtin).unwrap();

    // ShortCircuit with is_error=true and no matches.
    let sc = HookRow {
        hook_id: "h-sc".into(),
        phase: "post_call".into(),
        priority: 0,
        match_instance: None,
        match_tool: None,
        match_context: None,
        match_principal: None,
        action_kind: "shortcircuit".into(),
        action_builtin_name: None,
        action_kaish_body: None,
        action_kaish_script_id: None,
        action_kaish_path: None,
        action_result_text: Some("synthetic".into()),
        action_is_error: Some(true),
        action_deny_reason: None,
        action_log_target: None,
        action_log_level: None,
        action_ask_description: None,
    };
    db.insert_hook(&sc).unwrap();

    // Deny.
    let deny = HookRow {
        hook_id: "h-deny".into(),
        phase: "pre_call".into(),
        priority: -5,
        match_instance: Some("builtin.file".into()),
        match_tool: None,
        match_context: None,
        match_principal: None,
        action_kind: "deny".into(),
        action_builtin_name: None,
        action_kaish_body: None,
        action_kaish_script_id: None,
        action_kaish_path: None,
        action_result_text: None,
        action_is_error: None,
        action_deny_reason: Some("no writes".into()),
        action_log_target: None,
        action_log_level: None,
        action_ask_description: None,
    };
    db.insert_hook(&deny).unwrap();

    // Log.
    let log = minimal_hook_row("h-log", "on_notification", 0);
    db.insert_hook(&log).unwrap();

    // Kaish (reserved; rejected at admin time, but the row shape
    // must still round-trip so a manually-inserted row hydrates
    // predictably and the skip-with-warn path works).
    let kaish = HookRow {
        hook_id: "h-kaish".into(),
        phase: "list_tools".into(),
        priority: 0,
        match_instance: None,
        match_tool: None,
        match_context: None,
        match_principal: None,
        action_kind: "kaish_invoke".into(),
        action_builtin_name: None,
        action_kaish_body: Some("script-42".into()),
        action_kaish_script_id: None,
        action_kaish_path: None,
        action_result_text: None,
        action_is_error: None,
        action_deny_reason: None,
        action_log_target: None,
        action_log_level: None,
        action_ask_description: None,
    };
    db.insert_hook(&kaish).unwrap();

    // Ask (D-57): description round-trips; None means "auto-generate
    // at fire time" and is exercised by `minimal_hook_row`-style
    // callers elsewhere, not here.
    let ask = HookRow {
        hook_id: "h-ask".into(),
        phase: "pre_call".into(),
        priority: 0,
        match_instance: None,
        match_tool: None,
        match_context: None,
        match_principal: None,
        action_kind: "ask".into(),
        action_builtin_name: None,
        action_kaish_body: None,
        action_kaish_script_id: None,
        action_kaish_path: None,
        action_result_text: None,
        action_is_error: None,
        action_deny_reason: None,
        action_log_target: None,
        action_log_level: None,
        action_ask_description: Some("about to rm -rf a workspace path".into()),
    };
    db.insert_hook(&ask).unwrap();

    let loaded = db.load_all_hooks().unwrap();
    assert_eq!(loaded.len(), 6);

    let by_id: std::collections::HashMap<String, HookRow> = loaded
        .into_iter()
        .map(|r| (r.hook_id.clone(), r))
        .collect();

    let b = by_id.get("h-builtin").unwrap();
    assert_eq!(b.phase, "pre_call");
    assert_eq!(b.priority, 10);
    assert_eq!(b.match_instance.as_deref(), Some("builtin.*"));
    assert_eq!(b.match_tool.as_deref(), Some("file_*"));
    assert_eq!(b.match_context, Some(ctx_id));
    assert_eq!(b.match_principal, Some(principal));
    assert_eq!(b.action_kind, "builtin_invoke");
    assert_eq!(b.action_builtin_name.as_deref(), Some("tracing_audit"));

    let s = by_id.get("h-sc").unwrap();
    assert_eq!(s.action_kind, "shortcircuit");
    assert_eq!(s.action_result_text.as_deref(), Some("synthetic"));
    assert_eq!(s.action_is_error, Some(true));

    let d = by_id.get("h-deny").unwrap();
    assert_eq!(d.action_kind, "deny");
    assert_eq!(d.action_deny_reason.as_deref(), Some("no writes"));
    assert_eq!(d.priority, -5);

    let l = by_id.get("h-log").unwrap();
    assert_eq!(l.action_kind, "log");
    assert_eq!(l.action_log_level.as_deref(), Some("info"));

    let k = by_id.get("h-kaish").unwrap();
    assert_eq!(k.action_kind, "kaish_invoke");
    assert_eq!(k.action_kaish_body.as_deref(), Some("script-42"));

    let a = by_id.get("h-ask").unwrap();
    assert_eq!(a.action_kind, "ask");
    assert_eq!(
        a.action_ask_description.as_deref(),
        Some("about to rm -rf a workspace path"),
    );
}

#[test]
fn hook_delete_returns_whether_existed() {
    let db = KernelDb::temporary().unwrap();
    // Delete of absent id is false (idempotent cleanup).
    assert!(!db.delete_hook("nope").unwrap());

    db.insert_hook(&minimal_hook_row("keep", "pre_call", 0))
        .unwrap();
    assert!(db.delete_hook("keep").unwrap());
    // Second delete of same id is false.
    assert!(!db.delete_hook("keep").unwrap());
}

#[test]
fn load_all_hooks_orders_by_phase_priority_then_insertion_idx() {
    // Tests the evaluation-law tiebreak (§4.3): priority ascending,
    // insertion-order tiebreak. Insert three hooks in pre_call with
    // identical priority; load must return them in insertion order.
    // Cross-phase, ordering within each phase must be preserved.
    let db = KernelDb::temporary().unwrap();
    db.insert_hook(&minimal_hook_row("a", "pre_call", 5))
        .unwrap();
    db.insert_hook(&minimal_hook_row("b", "pre_call", 5))
        .unwrap();
    db.insert_hook(&minimal_hook_row("c", "pre_call", 1))
        .unwrap();
    db.insert_hook(&minimal_hook_row("x", "post_call", 0))
        .unwrap();
    db.insert_hook(&minimal_hook_row("y", "post_call", 0))
        .unwrap();

    let loaded = db.load_all_hooks().unwrap();
    let pre: Vec<&str> = loaded
        .iter()
        .filter(|r| r.phase == "pre_call")
        .map(|r| r.hook_id.as_str())
        .collect();
    // Priority 1 first, then priority 5 with insertion order (a, b).
    assert_eq!(pre, vec!["c", "a", "b"]);

    let post: Vec<&str> = loaded
        .iter()
        .filter(|r| r.phase == "post_call")
        .map(|r| r.hook_id.as_str())
        .collect();
    assert_eq!(post, vec!["x", "y"]);
}

#[test]
fn hook_insert_same_id_errors() {
    // Primary-key collision is a load-bearing signal: the broker
    // should call `delete_hook` first for replace semantics rather
    // than relying on an implicit upsert. Lock that contract in.
    let db = KernelDb::temporary().unwrap();
    db.insert_hook(&minimal_hook_row("dup", "pre_call", 0))
        .unwrap();
    let err = db
        .insert_hook(&minimal_hook_row("dup", "pre_call", 0))
        .unwrap_err();
    match err {
        KernelDbError::Db(_) => {}
        other => panic!("expected DB error on PK collision, got {other:?}"),
    }
}

#[test]
fn upsert_hook_inserts_a_fresh_row() {
    let db = KernelDb::temporary().unwrap();
    let replaced = db.upsert_hook(&minimal_hook_row("new", "pre_call", 0)).unwrap();
    assert!(!replaced, "a fresh id must report replaced=false");
    let loaded = db.load_all_hooks().unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].hook_id, "new");
}

#[test]
fn upsert_hook_replaces_an_existing_row_in_place() {
    // The bug this closes: a bare INSERT errors on the PRIMARY KEY
    // collision (locked in by `hook_insert_same_id_errors`); `upsert_hook`
    // must instead replace the row's content atomically, in one
    // statement, with no separate delete step.
    let db = KernelDb::temporary().unwrap();
    db.upsert_hook(&minimal_hook_row("dup", "pre_call", 0)).unwrap();

    let mut second = minimal_hook_row("dup", "post_call", 7);
    second.action_log_level = Some("error".into());
    let replaced = db.upsert_hook(&second).unwrap();
    assert!(replaced, "re-adding an existing id must report replaced=true");

    let loaded = db.load_all_hooks().unwrap();
    assert_eq!(loaded.len(), 1, "upsert must not leave a duplicate row");
    assert_eq!(loaded[0].phase, "post_call");
    assert_eq!(loaded[0].priority, 7);
    assert_eq!(loaded[0].action_log_level.as_deref(), Some("error"));
}

#[test]
fn upsert_hook_failed_write_does_not_lose_the_existing_row() {
    // Falsifies the delete-then-insert replace this upsert replaces:
    // under that shape, a write that fails after the delete step but
    // before the insert step loses the hook outright. `upsert_hook` is
    // one statement, so a failed write must leave the prior row exactly
    // as it was.
    let db = KernelDb::temporary().unwrap();
    db.upsert_hook(&minimal_hook_row("keep", "pre_call", 3)).unwrap();

    db.set_query_only_for_test(true).unwrap();
    let mut attempt = minimal_hook_row("keep", "post_call", 99);
    attempt.action_log_level = Some("error".into());
    let err = db.upsert_hook(&attempt).unwrap_err();
    match err {
        KernelDbError::Db(_) => {}
        other => panic!("expected a DB error from the read-only write, got {other:?}"),
    }
    db.set_query_only_for_test(false).unwrap();

    let loaded = db.load_all_hooks().unwrap();
    assert_eq!(loaded.len(), 1, "the failed write must not have removed the row");
    assert_eq!(loaded[0].hook_id, "keep");
    assert_eq!(loaded[0].phase, "pre_call", "the failed write must not have changed the row");
    assert_eq!(loaded[0].priority, 3);
}

