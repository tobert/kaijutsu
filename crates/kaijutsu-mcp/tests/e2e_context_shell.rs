//! End-to-end tests for the MCP store-replica + shell-poll path.
//!
//! These stand up a real ephemeral SSH server (the full SSH + Cap'n Proto
//! stack) and drive `KaijutsuMcp` exactly as a connected agent would:
//! `connect_with_config` → `register_session` → `context_shell`. This is the
//! layer that had no coverage — the in-`src` unit tests only exercise the
//! local (no-server) input path, and the server-side integration tests use the
//! raw `kernel.subscribe_output()` channel rather than the MCP's
//! SyncManager-backed store replica + `execute_and_poll_shell`.
//!
//! The motivating bug: `context_shell` returned an empty `stdout` even though
//! the server produced output, because the completion poll read the store the
//! instant it saw `Done` — before the background sync listener had applied the
//! preceding text ops. A faithful e2e is the only thing that catches that
//! class of replication/ordering regression.

use std::net::SocketAddr;

use rmcp::handler::server::wrapper::Parameters;
use tokio::net::TcpListener;
use tokio::task::LocalSet;

use kaijutsu_client::{KeySource, SshConfig};
use kaijutsu_mcp::{ContextShellRequest, KaijutsuMcp, RegisterSessionRequest};
use kaijutsu_server::{SshServer, SshServerConfig};

/// capnp-rpc requires a current-thread runtime with a LocalSet.
fn run_local<F: std::future::Future<Output = ()>>(f: F) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = LocalSet::new();
    rt.block_on(local.run_until(f));
}

/// Start an ephemeral SSH server on a random port; return its address.
async fn start_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = SshServerConfig::ephemeral(addr.port());

    tokio::task::spawn_local(async move {
        let server = SshServer::new(config);
        if let Err(e) = server.run_on_listener(listener).await {
            log::error!("Server error: {}", e);
        }
    });

    tokio::task::yield_now().await;
    addr
}

/// Connect a `KaijutsuMcp` to the ephemeral server with the test key + insecure
/// host-key policy, matching how the server-side integration tests connect.
async fn connect_mcp(addr: SocketAddr) -> KaijutsuMcp {
    let config = SshConfig {
        host: addr.ip().to_string(),
        port: addr.port(),
        username: "test_user".to_string(),
        key_source: KeySource::ephemeral(),
        insecure: true,
    };
    KaijutsuMcp::connect_with_config(config, "e2e-test", Some("e2e-session"))
        .await
        .expect("MCP connect failed")
}

/// Register a session, retrying while the freshly-spawned actor is still
/// connecting (`not ready: idle`). Mirrors how a real agent calls
/// `register_session` only once the connection has settled. Returns the parsed
/// success envelope, or panics on timeout.
async fn register_with_retry(mcp: &KaijutsuMcp, label: &str) -> serde_json::Value {
    for _ in 0..100 {
        let raw = mcp
            .register_session(Parameters(RegisterSessionRequest {
                label: Some(label.to_string()),
                context_type: None,
            }))
            .await;
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
            return v;
        }
        // Non-JSON means an error string (e.g. "not ready: idle") — back off.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("register_session never became ready");
}

/// The core regression guard: a command's stdout must survive the trip through
/// the store replica and land in the `context_shell` envelope.
#[test]
fn context_shell_returns_stdout() {
    run_local(async {
        let addr = start_server().await;
        let mcp = connect_mcp(addr).await;

        let reg = register_with_retry(&mcp, "e2e").await;
        assert!(
            reg.get("success").and_then(|v| v.as_bool()).unwrap_or(false),
            "register_session did not succeed: {reg}"
        );

        let out = mcp
            .context_shell(Parameters(ContextShellRequest {
                command: "echo hello".to_string(),
                timeout_secs: Some(30),
            }))
            .await;
        let env: serde_json::Value = serde_json::from_str(&out).unwrap();

        assert_eq!(
            env["status"].as_str(),
            Some("done"),
            "expected Done status, got envelope: {env}"
        );
        assert_eq!(
            env["exit_code"].as_i64(),
            Some(0),
            "expected exit_code 0, got envelope: {env}"
        );
        assert_eq!(
            env["stdout"].as_str(),
            Some("hello\n"),
            "stdout did not replicate into the envelope: {env}"
        );
    });
}

/// Sequential commands must each return their own stdout — guards against the
/// store replica diverging after the first command (stale frontier, stranded
/// text ops).
#[test]
fn context_shell_sequential_commands() {
    run_local(async {
        let addr = start_server().await;
        let mcp = connect_mcp(addr).await;

        register_with_retry(&mcp, "e2e-seq").await;

        for n in 1..=3 {
            let out = mcp
                .context_shell(Parameters(ContextShellRequest {
                    command: format!("echo line{n}"),
                    timeout_secs: Some(30),
                }))
                .await;
            let env: serde_json::Value = serde_json::from_str(&out).unwrap();
            assert_eq!(
                env["stdout"].as_str(),
                Some(format!("line{n}\n").as_str()),
                "command {n} stdout wrong: {env}"
            );
        }
    });
}
