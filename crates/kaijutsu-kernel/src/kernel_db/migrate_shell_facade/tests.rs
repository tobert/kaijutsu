use super::*;
use crate::mcp::binding::Capability;

/// Plant a database as it looked before `shell-facade-swap-v1`:
/// - `toolie` holds only `facade:shell` (old read-only)
/// - `coder` holds only `facade:shell_write` (old writable)
/// - `banto` holds both `facade:shell` and `facade:shell_write`
/// - a `shell-escape-guard` hook matches `shell_write`
/// - `context_binding_names` holds sticky entries for both instances
fn old_database(path: &Path) -> (ContextId, ContextId, ContextId) {
    let mut db = KernelDb::open(path).unwrap();
    let ws = setup_test_db(&db);
    let toolie = make_context_row(Some("toolie"));
    let coder = make_context_row(Some("coder"));
    let banto = make_context_row(Some("banto"));
    for row in [&toolie, &coder, &banto] {
        insert_context_with_doc(&db, row, ws);
    }

    let mut b_toolie = ContextToolBinding::default();
    b_toolie.grant(Capability::Facade("shell".into()));
    b_toolie.name_map.insert("shell".into(), (InstanceId::new("builtin.shell"), "shell".into()));
    db.upsert_context_binding(toolie.context_id, &b_toolie).unwrap();

    let mut b_coder = ContextToolBinding::default();
    b_coder.grant(Capability::Facade("shell_write".into()));
    b_coder.name_map.insert("shell_write".into(), (InstanceId::new("builtin.shell_write"), "shell_write".into()));
    db.upsert_context_binding(coder.context_id, &b_coder).unwrap();

    let mut b_banto = ContextToolBinding::default();
    b_banto.grant(Capability::Facade("shell".into()));
    b_banto.grant(Capability::Facade("shell_write".into()));
    b_banto.name_map.insert("shell".into(), (InstanceId::new("builtin.shell"), "shell".into()));
    b_banto.name_map.insert("shell_write".into(), (InstanceId::new("builtin.shell_write"), "shell_write".into()));
    db.upsert_context_binding(banto.context_id, &b_banto).unwrap();

    db.conn
        .execute(
            "INSERT INTO hooks (hook_id, phase, priority, insertion_idx, match_instance, match_tool, action_kind, action_kaish_body)
             VALUES ('shell-escape-guard', 'pre_call', 50, 0, 'builtin.shell_write', 'shell_write', 'kaish_invoke', 'exit 0')",
            [],
        )
        .unwrap();

    db.conn
        .execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::SHELL_FACADE_SWAP_MIGRATION])
        .unwrap();

    (toolie.context_id, coder.context_id, banto.context_id)
}

#[test]
fn shell_facades_and_hooks_swap_cleanly_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    let (toolie, coder, banto) = old_database(&path);

    let db = KernelDb::open(&path).unwrap();

    let toolie_b = db.get_context_binding(toolie).unwrap().unwrap();
    assert_eq!(toolie_b.allowed_facades, vec!["shell_readonly".to_string()]);
    assert!(toolie_b.name_map.is_empty(), "stale sticky name cleared");

    let coder_b = db.get_context_binding(coder).unwrap().unwrap();
    assert_eq!(coder_b.allowed_facades, vec!["shell".to_string()]);
    assert!(coder_b.name_map.is_empty(), "stale sticky name cleared");

    let banto_b = db.get_context_binding(banto).unwrap().unwrap();
    assert_eq!(banto_b.allowed_facades, vec!["shell".to_string()]);
    assert!(banto_b.name_map.is_empty(), "stale sticky names cleared");

    let (inst, tool): (Option<String>, Option<String>) = db
        .conn
        .query_row(
            "SELECT match_instance, match_tool FROM hooks WHERE hook_id = 'shell-escape-guard'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(inst.as_deref(), Some("builtin.shell"));
    assert_eq!(tool.as_deref(), Some("shell"));
}

#[test]
fn shell_facade_swap_runs_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    let (toolie, coder, banto) = old_database(&path);

    drop(KernelDb::open(&path).unwrap());
    let db = KernelDb::open(&path).unwrap();

    assert_eq!(db.get_context_binding(toolie).unwrap().unwrap().allowed_facades, vec!["shell_readonly".to_string()]);
    assert_eq!(db.get_context_binding(coder).unwrap().unwrap().allowed_facades, vec!["shell".to_string()]);
    assert_eq!(db.get_context_binding(banto).unwrap().unwrap().allowed_facades, vec!["shell".to_string()]);
}
