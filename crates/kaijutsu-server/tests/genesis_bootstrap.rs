//! Kernel start and root characters (`docs/character.md`, "Bootstrap: the
//! person creates themself"). A kernel with no live root character refuses
//! to start and names `kaijutsu-server init`. A kernel with one creates that
//! character's root context at start, once.

mod support;

use kaijutsu_types::PrincipalId;

async fn start(dir: &std::path::Path) -> Result<kaijutsu_server::SharedKernel, capnp::Error> {
    kaijutsu_server::rpc::create_shared_kernel(
        None,
        &kaijutsu_server::config_mounts::ConfigMounts::new(dir.join("config")),
        Some(dir),
        &[],
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kernel_without_a_root_character_refuses_to_start() {
    let tmp = tempfile::tempdir().unwrap();
    support::disable_embeddings(tmp.path());

    let Err(error) = start(tmp.path()).await else {
        panic!("a kernel with no root character must refuse to start");
    };
    let message = error.to_string();
    assert!(
        message.contains("kaijutsu-server init --as <name> --key <pubkey-file>"),
        "the refusal must say how to create the root character: {message}"
    );
    let contexts = kaijutsu_kernel::KernelDb::open(tmp.path().join("kernel.db"))
        .unwrap()
        .list_all_contexts()
        .unwrap();
    assert!(contexts.is_empty(), "a refused start creates no context, got {}", contexts.len());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_creates_the_root_context_once() {
    let tmp = tempfile::tempdir().unwrap();
    support::disable_embeddings(tmp.path());
    let amy = support::init_root(tmp.path(), "amy");

    let shared = start(tmp.path()).await.expect("a kernel with a root character starts");
    let contexts = shared.kernel_db.lock().list_active_contexts().unwrap();
    assert_eq!(contexts.len(), 1, "start creates exactly the root context");
    let root = &contexts[0];
    assert_eq!(root.label.as_deref(), Some("amy"));
    assert_eq!(root.context_type, "root");
    assert_eq!(root.played_by, Some(amy));
    assert_eq!(root.forked_from, None);
    assert_eq!(root.created_by, PrincipalId::system(), "the kernel creates it at start");
    assert!(
        shared.documents.contains(root.context_id),
        "the root context must have its conversation document"
    );
    assert_eq!(
        shared.kernel_db.lock().get_character(amy).unwrap().unwrap().root_ctx,
        Some(root.context_id)
    );
    let binding = shared.kernel.broker().binding(&root.context_id).await.expect("the root rc bundle binds it");
    assert!(binding.is_admin());
    drop(shared);

    let restarted = start(tmp.path()).await.expect("restart");
    let contexts = restarted.kernel_db.lock().list_active_contexts().unwrap();
    assert_eq!(contexts.len(), 1, "a restart creates no second root context");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_refuses_incomplete_orphan_recovery_and_retries_without_partial_settlement() {
    for fault in ["journal", "approval"] { failed_orphan_boot(fault).await; }
}

async fn failed_orphan_boot(fault: &str) {
    let tmp = tempfile::tempdir().unwrap();
    support::disable_embeddings(tmp.path());
    let amy = support::init_root(tmp.path(), "amy");
    let shared = start(tmp.path()).await.unwrap();
    let context = shared.kernel_db.lock().get_character(amy).unwrap().unwrap().root_ctx.unwrap();
    let call = shared.documents.insert_tool_call(context, None, None, "model_tool", serde_json::json!({}), None).unwrap();
    let result = shared.documents.insert_tool_result(context, &call, Some(&call), "partial output", false, None, None).unwrap();
    shared.documents.set_status(context, &result, kaijutsu_types::Status::Waiting).unwrap();
    shared.documents.set_stderr(context, &result, Some("partial stderr".into())).unwrap();
    shared.kernel.shutdown_runtime_worker().await.unwrap();
    drop(shared);
    let conn = rusqlite::Connection::open(tmp.path().join("kernel.db")).unwrap();
    let ask = approval_ledger::ask::create_ask(&conn, &approval_ledger::types::NewAsk {
        context_id: context.as_bytes().to_vec(), principal_id: amy.as_bytes().to_vec(),
        actor_id: amy.as_bytes().to_vec(), reviewer_id: PrincipalId::new().as_bytes().to_vec(),
        origin: approval_ledger::types::Origin::ShellGate, instance: None, tool: None, hook_id: None,
        description: "interrupted caller".into(), statements: vec![], authorized_label: None,
        rc_run_id: None, expires_at: None, options: vec![], signals: vec![], cwd: None,
        exec_source: None, exec_stdin: None, continuation_epoch: None, env: vec![],
    }).unwrap();
    conn.execute_batch(match fault {
        "journal" => "CREATE TRIGGER reject_orphan_recovery BEFORE INSERT ON oplog BEGIN SELECT RAISE(ABORT, 'injected orphan journal fault'); END;",
        _ => "CREATE TRIGGER reject_orphan_recovery BEFORE UPDATE OF status ON approvals BEGIN SELECT RAISE(ABORT, 'injected orphan approval fault'); END;",
    }).unwrap();
    let error = match start(tmp.path()).await {
        Ok(_) => panic!("an incomplete orphan recovery must refuse startup"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains(if fault == "journal" { "Cannot recover interrupted blocks" } else { "Cannot retire interrupted approvals" }), "{error}");
    assert!(error.contains(&format!("injected orphan {fault} fault")), "{error}");
    conn.execute_batch("DROP TRIGGER reject_orphan_recovery").unwrap();
    let recovered = start(tmp.path()).await.unwrap();
    for id in [&call, &result] {
        assert_eq!(recovered.documents.get_block_snapshot(context, id).unwrap().unwrap().status, kaijutsu_types::Status::Error);
    }
    let output = recovered.documents.get_block_snapshot(context, &result).unwrap().unwrap();
    assert_eq!(output.content, "partial output");
    let reason = output.stderr.unwrap();
    assert!(reason.starts_with("partial stderr\n"), "{reason}");
    assert_eq!(reason.matches("the kernel restarted").count(), 1);
    assert_eq!(recovered.documents.block_snapshots(context).unwrap().iter().filter(|b| b.kind == kaijutsu_types::BlockKind::Error).count(), 1);
    let status: String = conn.query_row("SELECT status FROM approvals WHERE request_id=?1", [&ask], |row| row.get(0)).unwrap();
    assert_eq!(status, "abandoned");
    recovered.kernel.shutdown_runtime_worker().await.unwrap();
}
