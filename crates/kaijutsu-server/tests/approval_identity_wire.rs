//! Approval identity over the product SSH/RPC wire.

mod common;

use std::sync::Arc;

use common::run_local;
use kaijutsu_client::{KeySource, RpcClient, SshClient, SshConfig};
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
    RpcClient::new(channel.into_stream()).await.expect("RPC client")
}

fn add_character(kernel: &SharedKernel, id: PrincipalId, name: &str) {
    kernel.kernel_db.lock().insert_character(&CharacterRow {
        principal_id: id, name: name.to_string(), created_at: 0, retired_at: None, handoff_ctx: None,
    }).expect("insert character");
}

#[test]
fn amy_approves_coder_in_same_context_but_coder_cannot_approve_from_another_context() {
    run_local(async {
        let temp = tempfile::tempdir().unwrap();
        let auth_path = temp.path().join("auth.db");
        let amy = PrincipalId::new();
        let coder = PrincipalId::new();
        let amy_key = PrivateKey::random(&mut rand_v10::rng(), Algorithm::Ed25519).unwrap();
        let coder_key = PrivateKey::random(&mut rand_v10::rng(), Algorithm::Ed25519).unwrap();
        let auth = AuthDb::open(&auth_path).unwrap();
        auth.add_key(amy, amy_key.public_key(), Some("amy")).unwrap();
        auth.add_key(coder, coder_key.public_key(), Some("coder")).unwrap();
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
        let coder_client = connect(addr, coder_key, "coder").await;
        let (coder_kj, _) = coder_client.bind_kernel().await.unwrap();
        let work = coder_kj.create_context("coder-work").await.unwrap();
        kernel.kernel_db.lock().update_context_review(work, Some(coder), Some(amy)).unwrap();
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
        let amy_client = connect(addr, amy_key, "amy").await;
        let (amy_kj, _) = amy_client.bind_kernel().await.unwrap();
        amy_kj.join_context(work, "coder-work").await.unwrap();
        amy_kj.shell_execute(&format!("kj ledger allow {}", ask.request_id), work, true).await.unwrap();
        assert!(kernel.kernel_db.lock().get_approval(&ask.request_id).unwrap().unwrap().status.is_allowed());
        let other = coder_kj.create_context("coder-other").await.unwrap();
        coder_kj.join_context(work, "coder-work").await.unwrap();
        assert!(coder_kj.call_mcp_tool("shell_write", &serde_json::json!({"command":"false"})).await.is_err());
        let second = kernel.kernel_db.lock().list_pending_asks().unwrap().pop().unwrap();
        coder_kj.shell_execute(&format!("kj ledger allow {}", second.request_id), other, true).await.unwrap();
        assert_eq!(
            kernel.kernel_db.lock().get_approval(&second.request_id).unwrap().unwrap().status.as_str(),
            "pending",
        );
    });
}
