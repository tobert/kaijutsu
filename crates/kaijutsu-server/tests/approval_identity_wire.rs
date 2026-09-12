//! Approval identity over the product SSH/RPC wire.

mod common;

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
        principal_id: id, name: name.to_string(), created_at: 0, retired_at: None, handoff_ctx: None,
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
fn amy_default_and_director_delegation_route_approval_over_the_wire() {
    run_local(async {
        let temp = tempfile::tempdir().unwrap();
        let auth_path = temp.path().join("auth.db");
        let amy = PrincipalId::new();
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut config = SshServerConfig::ephemeral(addr.port());
        config.auth_db_path = Some(auth_path);
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::task::spawn_local(async move {
            SshServer::new(config).run_on_listener_with_kernel_sink(listener, tx).await.unwrap();
        });
        let kernel = rx.await.unwrap();
        add_character(&kernel, amy, "amy");
        add_character(&kernel, coder, "coder");
        add_character(&kernel, lead, "lead");
        add_character(&kernel, judge, "judge");
        let lead_client = connect(addr, lead_key, "lead").await;
        let (lead_kj, _) = lead_client.bind_kernel().await.unwrap();
        let work = lead_kj.create_context("coder-work").await.unwrap();
        let row = kernel.kernel_db.lock().get_context(work).unwrap().unwrap();
        assert_eq!(row.created_by, lead);
        assert_eq!(row.director_id, Some(lead));
        let coder_client = connect(addr, coder_key, "coder").await;
        let (coder_kj, _) = coder_client.bind_kernel().await.unwrap();
        kernel.kernel_db.lock().update_context_review_assignment(work, Some(coder), None, Some(lead)).unwrap();
        kernel.kernel.broker().hooks().write().await.pre_call.entries.push(HookEntry {
            id: HookId("identity-wire-gate".into()), match_instance: None,
            match_tool: Some(GlobPattern("shell_write".into())), match_context: Some(work),
            match_principal: None, action: HookAction::Ask(AskSpec { description: Some("identity wire".into()) }),
            priority: 0, kaish_script_id: None,
        });
        coder_kj.join_context(work, "coder-work").await.unwrap();
        assert!(coder_kj.call_mcp_tool("shell_write", &serde_json::json!({"command":"true"})).await.is_err());
        let ask = kernel.kernel_db.lock().list_pending_asks().unwrap().pop().unwrap();
        assert_eq!(ask.actor_id.as_deref(), Some(coder.as_bytes().as_slice()));
        assert_eq!(ask.reviewer_id.as_deref(), Some(amy.as_bytes().as_slice()));
        assert_eq!(
            kernel.kernel_db.lock().get_context(work).unwrap().unwrap().reviewer_id,
            None,
            "Amy-default review is resolved at the gate, not persisted as an override"
        );
        let amy_client = connect(addr, amy_key, "amy").await;
        let (amy_kj, _) = amy_client.bind_kernel().await.unwrap();
        let judge_client = connect(addr, judge_key, "judge").await;
        let (judge_kj, _) = judge_client.bind_kernel().await.unwrap();
        let lead_context = lead_kj.create_context("lead-review").await.unwrap();
        lead_kj.join_context(lead_context, "lead-review").await.unwrap();
        kj_fails(&lead_kj, lead_context, &["ledger", "allow", &ask.request_id]).await;
        assert_eq!(kernel.kernel_db.lock().get_approval(&ask.request_id).unwrap().unwrap().status.as_str(), "pending");
        amy_kj.join_context(work, "coder-work").await.unwrap();
        let delegation_help = amy_kj.execute_kj(work, &[
            "ledger".into(), "delegation".into(), "grant".into(), "--help".into(),
        ]).await.expect("delegation help reaches kernel");
        assert_eq!(delegation_help.exit_code, 0, "delegation help failed: {}", delegation_help.stderr);
        let delegation_help_text = format!("{}{}", delegation_help.stdout, delegation_help.stderr);
        assert!(delegation_help_text.contains("--to"), "delegation help: {delegation_help_text}");
        eprintln!("{delegation_help_text}");
        let context_help = amy_kj.execute_kj(work, &[
            "context".into(), "set".into(), "--help".into(),
        ]).await.expect("context help reaches kernel");
        assert_eq!(context_help.exit_code, 0, "context help failed: {}", context_help.stderr);
        let context_help_text = format!("{}{}", context_help.stdout, context_help.stderr);
        assert!(context_help_text.contains("--reviewer") && context_help_text.contains("--director"), "context help: {context_help_text}");
        assert!(context_help_text.contains("--clear-reviewer"));
        eprintln!("{context_help_text}");
        for argv in [vec!["context", "create", "--help"], vec!["ledger", "delegation", "revoke", "--help"]] {
            let argv = argv.into_iter().map(String::from).collect::<Vec<_>>();
            let help = amy_kj.execute_kj(work, &argv).await.expect("help reaches kernel");
            assert_eq!(help.exit_code, 0, "{}", help.stderr);
            eprintln!("{}{}", help.stdout, help.stderr);
        }
        kj(&amy_kj, work, &["ledger", "allow", &ask.request_id]).await;
        assert!(kernel.kernel_db.lock().get_approval(&ask.request_id).unwrap().unwrap().status.is_allowed());

        kj_fails(&lead_kj, lead_context, &["ledger", "delegation", "grant", "lead", "--to", "judge"]).await;
        kj(&amy_kj, work, &["ledger", "delegation", "grant", "lead", "--to", "judge"]).await;
        coder_kj.join_context(work, "coder-work").await.unwrap();
        assert!(coder_kj.call_mcp_tool("shell_write", &serde_json::json!({"command":"true"})).await.is_err());
        let delegated = kernel.kernel_db.lock().list_pending_asks().unwrap().pop().unwrap();
        assert_eq!(delegated.reviewer_id.as_deref(), Some(judge.as_bytes().as_slice()));
        kj_fails(&amy_kj, work, &["ledger", "allow", &delegated.request_id]).await;
        judge_kj.join_context(work, "coder-work").await.unwrap();
        kj(&judge_kj, work, &["ledger", "allow", &delegated.request_id]).await;
        assert!(kernel.kernel_db.lock().get_approval(&delegated.request_id).unwrap().unwrap().status.is_allowed());

        coder_kj.join_context(work, "coder-work").await.unwrap();
        assert!(coder_kj.call_mcp_tool("shell_write", &serde_json::json!({"command":"true"})).await.is_err());
        let reclaimed = kernel.kernel_db.lock().list_pending_asks().unwrap().pop().unwrap();
        assert_eq!(reclaimed.reviewer_id.as_deref(), Some(judge.as_bytes().as_slice()));
        amy_kj.join_context(work, "coder-work").await.unwrap();
        kj_fails(&amy_kj, work, &["ledger", "delegation", "revoke", "lead"]).await;
        kj(&amy_kj, work, &["ledger", "escalate", &reclaimed.request_id, "--to", "amy"]).await;
        let reclaimed = kernel.kernel_db.lock().get_approval(&reclaimed.request_id).unwrap().unwrap();
        assert_eq!(reclaimed.reviewer_id.as_deref(), Some(amy.as_bytes().as_slice()));
        kj(&amy_kj, work, &["ledger", "allow", &reclaimed.request_id]).await;
        assert!(kernel.kernel_db.lock().get_approval(&reclaimed.request_id).unwrap().unwrap().status.is_allowed());
        kj(&amy_kj, work, &["ledger", "delegation", "revoke", "lead"]).await;
        let other = coder_kj.create_context("coder-other").await.unwrap();
        coder_kj.join_context(work, "coder-work").await.unwrap();
        assert!(coder_kj.call_mcp_tool("shell_write", &serde_json::json!({"command":"false"})).await.is_err());
        let second = kernel.kernel_db.lock().list_pending_asks().unwrap().pop().unwrap();
        assert_eq!(second.reviewer_id.as_deref(), Some(amy.as_bytes().as_slice()), "a revoked delegation sends the next ask back to Amy");
        kj_fails(&coder_kj, other, &["ledger", "allow", &second.request_id]).await;
        assert_eq!(
            kernel.kernel_db.lock().get_approval(&second.request_id).unwrap().unwrap().status.as_str(),
            "pending",
        );
        kj(&amy_kj, work, &["ledger", "allow", &second.request_id]).await;

        kernel.kernel.broker().hooks().write().await.pre_call.entries.clear();
        kernel.kernel_db.lock().update_context_review_assignment(work, Some(coder), Some(judge), Some(lead)).unwrap();
        assert!(kernel.kernel_db.lock().retire_character(judge, 1).unwrap());
        let broken_info = amy_kj.execute_kj(work, &["context".into(), "info".into()])
            .await.expect("context info reaches a context with a retired reviewer");
        assert_eq!(broken_info.exit_code, 0, "broken context info failed: {}", broken_info.stderr);
        let broken_data = broken_info.data.expect("context info data");
        assert!(broken_data["reviewer_error"].as_str().is_some_and(|error| error.contains("retired")));
        assert!(broken_data["reviewer_id"].is_null());
        assert!(broken_data["reviewer_name"].is_null());
        assert_eq!(broken_data["reviewer_override_id"], judge.to_hex());
        kj(&amy_kj, work, &["context", "set", ".", "--reviewer", "amy"]).await;
        let repaired_info = amy_kj.execute_kj(work, &["context".into(), "info".into()])
            .await.expect("context info reaches repaired context");
        assert_eq!(repaired_info.exit_code, 0, "repaired context info failed: {}", repaired_info.stderr);
        let repaired_data = repaired_info.data.expect("repaired context info data");
        assert!(repaired_data["reviewer_error"].is_null());
        assert_eq!(repaired_data["reviewer_id"], amy.to_hex());
        assert_eq!(repaired_data["reviewer_name"], "amy");
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
