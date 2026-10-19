//! Approval identity over the product SSH/RPC wire.

mod common;
use common::{create_context};

use std::sync::Arc;

use common::run_local;
use kaijutsu_client::{KernelHandle, KeySource, RpcClient, SshClient, SshConfig};
use kaijutsu_kernel::kernel_db::CharacterRow;
use kaijutsu_kernel::mcp::{AskSpec, GlobPattern, HookAction, HookEntry, HookId};
use kaijutsu_server::{AuthDb, SharedKernel, SshServer, SshServerConfig};
use kaijutsu_types::PrincipalId;
use russh::keys::{Algorithm, PrivateKey};

async fn connect(addr: std::net::SocketAddr, key: PrivateKey, name: &str) -> RpcClient {
    let mut ssh = SshClient::new(SshConfig {
        host: addr.ip().to_string(), port: addr.port(), username: name.to_string(),
        key_source: KeySource::InMemory(Arc::new(key)), insecure: true,
    });
    let channel = ssh.connect().await.expect("SSH connect");
    let mut client = RpcClient::new(channel.into_stream()).await.expect("RPC client");
    client.retain_ssh_session(ssh);
    client
}

fn add_character(kernel: &SharedKernel, id: PrincipalId, name: &str) {
    kernel.kernel_db.lock().insert_character(&CharacterRow {
        principal_id: id, name: name.to_string(), created_at: 0, retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
    }).expect("insert character");
}

async fn kj(kernel: &KernelHandle, context_id: kaijutsu_types::ContextId, argv: &[&str]) {
    let argv: Vec<String> = argv.iter().map(|arg| (*arg).to_owned()).collect();
    let result = kernel.execute_kj(context_id, &argv).await.expect("kj reaches kernel");
    assert_eq!(
        result.exit_code, 0,
        "kj {argv:?} failed: {} {}",
        result.stdout, result.stderr,
    );
}

async fn kj_fails(kernel: &KernelHandle, context_id: kaijutsu_types::ContextId, argv: &[&str]) {
    let argv: Vec<String> = argv.iter().map(|arg| (*arg).to_owned()).collect();
    let result = kernel.execute_kj(context_id, &argv).await.expect("kj reaches kernel");
    assert_ne!(
        result.exit_code, 0,
        "kj {argv:?} unexpectedly succeeded: {} {}",
        result.stdout, result.stderr,
    );
}

#[test]
fn amy_default_and_escalation_route_approval_over_the_wire() {
    run_local(async {
        let temp = tempfile::tempdir().unwrap();
        let auth_path = temp.path().join("auth.db");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut config = SshServerConfig::ephemeral_with_root(addr.port(), "amy");
        common::allow_in_shipped_gate(&config, &["kj context create"]); // model credentials create their own contexts in setup
        let amy = kaijutsu_kernel::KernelDb::open(config.data_dir.as_ref().unwrap().join("kernel.db"))
            .unwrap()
            .get_character_by_name("amy")
            .unwrap()
            .expect("amy is the kernel's root character")
            .principal_id;
        let coder = PrincipalId::new();
        let lead = PrincipalId::new();
        let judge = PrincipalId::new();
        let amy_key = PrivateKey::random(&mut rand_v10::rng(), Algorithm::Ed25519).unwrap();
        let coder_key = PrivateKey::random(&mut rand_v10::rng(), Algorithm::Ed25519).unwrap();
        let lead_key = PrivateKey::random(&mut rand_v10::rng(), Algorithm::Ed25519).unwrap();
        let judge_key = PrivateKey::random(&mut rand_v10::rng(), Algorithm::Ed25519).unwrap();
        let auth = AuthDb::open(&auth_path).unwrap();
        auth.add_key(amy, amy_key.public_key(), Some("amy")).unwrap();
        auth.add_key(coder, coder_key.public_key(), Some("coder")).unwrap();
        auth.add_key(lead, lead_key.public_key(), Some("lead")).unwrap();
        auth.add_key(judge, judge_key.public_key(), Some("judge")).unwrap();
        drop(auth);
        config.auth_db_path = Some(auth_path);
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::task::spawn_local(async move {
            SshServer::new(config).run_on_listener_with_kernel_sink(listener, tx).await.unwrap();
        });
        let kernel = rx.await.unwrap();
        add_character(&kernel, coder, "coder");
        add_character(&kernel, lead, "lead");
        add_character(&kernel, judge, "judge");
        let lead_client = connect(addr, lead_key, "lead").await;
        let (lead_kj, _) = lead_client.bind_kernel().await.unwrap();
        let work = create_context(&lead_kj, "coder-work").await.unwrap();
        let row = kernel.kernel_db.lock().get_context(work).unwrap().unwrap();
        assert_eq!(row.created_by, lead);
        assert!(row.senpai_ctx.is_some(), "the creating context is the new context's senpai");
        let coder_client = connect(addr, coder_key, "coder").await;
        let (coder_kj, _) = coder_client.bind_kernel().await.unwrap();
        kernel.kernel_db.lock().assign_test_review(work, Some(coder), None).unwrap();
        kernel.kernel.broker().hooks().write().await.pre_call.entries.push(HookEntry {
            id: HookId("identity-wire-gate".into()), match_instance: None,
            match_tool: Some(GlobPattern("shell_write".into())), match_context: Some(work),
            match_principal: None, action: HookAction::Ask(AskSpec { description: Some("identity wire".into()) }),
            priority: 0, kaish_script_id: None,
        });
        coder_kj.join_context(work, "coder-work").await.unwrap();
        let submission = coder_kj.shell_submit("true", work, true).await.unwrap();
        assert!(submission.refusal.is_some(), "the retained shell submission must carry the gate refusal");
        let ask = kernel.kernel_db.lock().list_pending_asks().unwrap().pop().unwrap();
        assert_eq!(ask.actor_id.as_deref(), Some(coder.as_bytes().as_slice()));
        assert_eq!(ask.reviewer_id.as_deref(), Some(amy.as_bytes().as_slice()));
        let amy_client = connect(addr, amy_key, "amy").await;
        let (amy_kj, _) = amy_client.bind_kernel().await.unwrap();
        let judge_client = connect(addr, judge_key, "judge").await;
        let (judge_kj, _) = judge_client.bind_kernel().await.unwrap();
        let lead_context = create_context(&lead_kj, "lead-review").await.unwrap();
        lead_kj.join_context(lead_context, "lead-review").await.unwrap();
        kj_fails(&lead_kj, lead_context, &["ledger", "allow", &ask.request_id]).await;
        assert_eq!(kernel.kernel_db.lock().get_approval(&ask.request_id).unwrap().unwrap().status.as_str(), "pending");
        amy_kj.join_context(work, "coder-work").await.unwrap();
        for argv in [vec!["context", "create", "--help"], vec!["ledger", "escalate", "--help"]] {
            let argv = argv.into_iter().map(String::from).collect::<Vec<_>>();
            let help = amy_kj.execute_kj(work, &argv).await.expect("help reaches kernel");
            assert_eq!(help.exit_code, 0, "{}", help.stderr);
            eprintln!("{}{}", help.stdout, help.stderr);
        }
        kj(&amy_kj, work, &["ledger", "allow", &ask.request_id]).await;
        assert!(kernel.kernel_db.lock().get_approval(&ask.request_id).unwrap().unwrap().status.is_allowed());

        // A later ask routes to the senpai's holder again; only Amy (its
        // reviewer or the lineage root) reassigns it, and then only the new
        // reviewer answers.
        coder_kj.join_context(work, "coder-work").await.unwrap();
        let submission = coder_kj.shell_submit("true", work, true).await.unwrap();
        assert!(submission.refusal.is_some(), "the retained shell submission must carry the gate refusal");
        let escalated = kernel.kernel_db.lock().list_pending_asks().unwrap().pop().unwrap();
        assert_eq!(escalated.reviewer_id.as_deref(), Some(amy.as_bytes().as_slice()));
        kj_fails(&lead_kj, lead_context, &["ledger", "escalate", &escalated.request_id, "--to", "judge"]).await;
        kj(&amy_kj, work, &["ledger", "escalate", &escalated.request_id, "--to", "judge"]).await;
        let escalated_row = kernel.kernel_db.lock().get_approval(&escalated.request_id).unwrap().unwrap();
        assert_eq!(escalated_row.reviewer_id.as_deref(), Some(judge.as_bytes().as_slice()));
        kj_fails(&amy_kj, work, &["ledger", "allow", &escalated.request_id]).await;
        judge_kj.join_context(work, "coder-work").await.unwrap();
        kj(&judge_kj, work, &["ledger", "allow", &escalated.request_id]).await;
        assert!(kernel.kernel_db.lock().get_approval(&escalated.request_id).unwrap().unwrap().status.is_allowed());

        let other = create_context(&coder_kj, "coder-other").await.unwrap();
        coder_kj.join_context(work, "coder-work").await.unwrap();
        let submission = coder_kj.shell_submit("false", work, true).await.unwrap();
        assert!(submission.refusal.is_some(), "the retained shell submission must carry the gate refusal");
        let second = kernel.kernel_db.lock().list_pending_asks().unwrap().pop().unwrap();
        assert_eq!(second.reviewer_id.as_deref(), Some(amy.as_bytes().as_slice()), "the next ask goes back to the senpai's holder");
        kj_fails(&coder_kj, other, &["ledger", "allow", &second.request_id]).await;
        assert_eq!(
            kernel.kernel_db.lock().get_approval(&second.request_id).unwrap().unwrap().status.as_str(),
            "pending",
        );
        kj(&amy_kj, work, &["ledger", "allow", &second.request_id]).await;
        kernel.kernel.broker().hooks().write().await.pre_call.entries.clear();
        drop(coder_kj);
        drop(amy_kj);
        drop(lead_kj);
        drop(judge_kj);
        drop(coder_client);
        drop(amy_client);
        drop(lead_client);
        drop(judge_client);
        tokio::task::yield_now().await;
    });
}
