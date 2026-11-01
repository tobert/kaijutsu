use super::*;

/// A database fault while reading a binding is an error, not "unbound":
/// callers would read it as a context that grants nothing.
#[test]
fn get_context_binding_reports_a_database_fault() {
    let db = KernelDb::temporary().unwrap();
    db.conn.execute_batch("PRAGMA foreign_keys = OFF; DROP TABLE context_bindings;").unwrap();
    assert!(db.get_context_binding(ContextId::new()).is_err());
}

#[test]
fn context_binding_roundtrip_preserves_order_and_sticky_names() {
    let mut db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("binding-roundtrip"));
    insert_context_with_doc(&db, &ctx, ws_id);

    let original = binding_with(
        &["builtin.block", "builtin.file", "external.gpal"],
        &[
            ("read", "builtin.file", "file_read"),
            ("write", "builtin.file", "file_write"),
            ("consult", "external.gpal", "consult_gemini"),
        ],
    );
    db.upsert_context_binding(ctx.context_id, &original).unwrap();

    let loaded = db
        .get_context_binding(ctx.context_id)
        .unwrap()
        .expect("binding should exist after upsert");

    // Order of `allowed_instances` must survive the roundtrip (D-20:
    // order is the tiebreaker for Auto resolution).
    assert_eq!(loaded.allowed_instances.len(), 3);
    assert_eq!(loaded.allowed_instances[0].as_str(), "builtin.block");
    assert_eq!(loaded.allowed_instances[1].as_str(), "builtin.file");
    assert_eq!(loaded.allowed_instances[2].as_str(), "external.gpal");

    // Name map content must match exactly — sticky resolution is the
    // whole point of persisting name_map across restart.
    assert_eq!(loaded.name_map.len(), 3);
    assert_eq!(
        loaded.name_map.get("read").unwrap(),
        &(InstanceId::new("builtin.file"), "file_read".into())
    );
    assert_eq!(
        loaded.name_map.get("consult").unwrap(),
        &(InstanceId::new("external.gpal"), "consult_gemini".into())
    );
}

#[test]
fn context_binding_roundtrip_preserves_tool_and_facade_grants() {
    use crate::mcp::Capability;
    let mut db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("binding-caps"));
    insert_context_with_doc(&db, &ctx, ws_id);

    // A tool-granular role bundle: no instance-wide grants, just specific
    // tools plus a facade.
    let mut original = ContextToolBinding::new();
    original.grant(Capability::Tool {
        instance: InstanceId::new("builtin.file"),
        tool: "read".into(),
    });
    original.grant(Capability::Tool {
        instance: InstanceId::new("builtin.file"),
        tool: "grep".into(),
    });
    original.grant(Capability::Facade("shell".into()));
    db.upsert_context_binding(ctx.context_id, &original).unwrap();

    let loaded = db
        .get_context_binding(ctx.context_id)
        .unwrap()
        .expect("binding should exist after upsert");

    assert!(
        loaded.allowed_instances.is_empty(),
        "no instance-wide grants were written"
    );
    assert!(!loaded.is_empty(), "tool/facade grants make it non-empty");
    assert_eq!(loaded.allowed_tools.len(), 2, "both tool grants survived");
    assert!(loaded.allows_tool(&InstanceId::new("builtin.file"), "read"));
    assert!(loaded.allows_tool(&InstanceId::new("builtin.file"), "grep"));
    assert!(
        !loaded.allows_tool(&InstanceId::new("builtin.file"), "write"),
        "ungranted sibling tool stays denied across restart"
    );
    assert_eq!(loaded.allowed_facades, vec!["shell".to_string()]);
}

#[test]
fn context_binding_flags_roundtrip() {
    use crate::mcp::Capability;
    let mut db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("binding-flags"));
    insert_context_with_doc(&db, &ctx, ws_id);

    let mut original = ContextToolBinding::new();
    original.grant(Capability::AllInstances);
    original.grant(Capability::AllFacades);
    original.grant(Capability::Admin);
    db.upsert_context_binding(ctx.context_id, &original).unwrap();

    let loaded = db
        .get_context_binding(ctx.context_id)
        .unwrap()
        .expect("binding should exist");
    assert!(loaded.all_instances, "all_instances survived restart");
    assert!(loaded.all_facades, "all_facades survived restart");
    assert!(loaded.binding_admin, "binding_admin survived restart");
}

#[test]
fn context_binding_get_absent_returns_none() {
    let db = KernelDb::temporary().unwrap();
    // No context, no upsert: get must return None. Deny-by-default — the
    // broker treats None as "grants nothing", no first-touch fallback.
    assert!(db.get_context_binding(ContextId::new()).unwrap().is_none());
}

#[test]
fn context_binding_upsert_replaces_wholesale() {
    // Phase 5 writes bindings as whole units; a second upsert must
    // wholly replace the children, not accumulate. Regression guard
    // against "leftover rows from a previous binding leak through."
    let mut db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("binding-replace"));
    insert_context_with_doc(&db, &ctx, ws_id);

    let first = binding_with(
        &["builtin.block", "builtin.file"],
        &[("read", "builtin.file", "file_read")],
    );
    db.upsert_context_binding(ctx.context_id, &first).unwrap();

    let second = binding_with(
        &["external.gpal"],
        &[("consult", "external.gpal", "consult_gemini")],
    );
    db.upsert_context_binding(ctx.context_id, &second).unwrap();

    let loaded = db.get_context_binding(ctx.context_id).unwrap().unwrap();
    assert_eq!(loaded.allowed_instances.len(), 1, "stale instances leaked");
    assert_eq!(loaded.allowed_instances[0].as_str(), "external.gpal");
    assert_eq!(loaded.name_map.len(), 1, "stale name_map entries leaked");
    assert!(loaded.name_map.contains_key("consult"));
    assert!(!loaded.name_map.contains_key("read"));
}

#[test]
fn context_binding_delete_returns_whether_existed() {
    let mut db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("binding-delete"));
    insert_context_with_doc(&db, &ctx, ws_id);

    // Delete of absent row returns false.
    assert!(!db.delete_context_binding(ctx.context_id).unwrap());

    db.upsert_context_binding(
        ctx.context_id,
        &binding_with(&["builtin.file"], &[]),
    )
    .unwrap();

    // Delete of present row returns true and clears children via CASCADE.
    assert!(db.delete_context_binding(ctx.context_id).unwrap());
    assert!(db.get_context_binding(ctx.context_id).unwrap().is_none());
}

#[test]
fn context_binding_cascades_on_context_delete() {
    // Parent context gone → all three binding tables cascade-clear.
    // Guards against orphaned rows in binding_instances / binding_names.
    let mut db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let ctx = make_context_row(Some("binding-cascade"));
    insert_context_with_doc(&db, &ctx, ws_id);

    db.upsert_context_binding(
        ctx.context_id,
        &binding_with(
            &["builtin.file", "builtin.block"],
            &[("read", "builtin.file", "file_read")],
        ),
    )
    .unwrap();
    assert!(db.get_context_binding(ctx.context_id).unwrap().is_some());

    db.conn.execute("DELETE FROM contexts WHERE context_id = ?1",
        params![blob_param(ctx.context_id.as_bytes())]).unwrap();
    assert!(db.get_context_binding(ctx.context_id).unwrap().is_none());

    // Children rows must also be gone (belt-and-suspenders on the
    // CASCADE — a future FK misstep would be caught here).
    let inst_rows: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM context_binding_instances WHERE context_id = ?1",
            params![blob_param(ctx.context_id.as_bytes())],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(inst_rows, 0);
    let name_rows: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM context_binding_names WHERE context_id = ?1",
            params![blob_param(ctx.context_id.as_bytes())],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(name_rows, 0);
}
