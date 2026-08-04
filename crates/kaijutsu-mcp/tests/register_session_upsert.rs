//! End-to-end tests for `register_session`'s upsert/attach behavior.
//!
//! The bug (docs/issues.md, "register_session hard-fails on label
//! conflict"): `register_session_impl` used to check idempotency only for
//! the CURRENT process (`remote.joined`), then unconditionally call
//! `create_context_typed(&label, ...)`. The label defaults to the harness's
//! agent-session id, so a reconnect after a dropped MCP session reused the
//! exact same label and hit KernelDb's label-uniqueness constraint as a
//! fatal `insert_context failed ... label conflict` — hit live 2026-07-29.
//!
//! Fix: `register_session_impl` resolves the label against the durable
//! KernelDb first (`resolve_context_label`, DB-driven — not the in-memory
//! registry `list_contexts` reads). A live match attaches instead of
//! creating (`"resumed": true` in the reply); a concluded/archived match
//! creates a fresh context under a deterministic suffixed label instead of
//! silently resurrecting the old one (`"previous_context"` in the reply).
//!
//! Same harness shape as `e2e_shell.rs`: a real ephemeral SSH server, driven
//! exactly as a connected agent would.

use std::net::SocketAddr;

use rmcp::handler::server::wrapper::Parameters;
use tokio::net::TcpListener;
use tokio::task::LocalSet;

use kaijutsu_client::{KeySource, SshConfig};
use kaijutsu_mcp::{Backend, KaijutsuMcp, RegisterSessionRequest, ShellRequest};
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

/// Connect a fresh `KaijutsuMcp` — a distinct actor/session each call, exactly
/// like a new agent process reconnecting after the previous one died.
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
/// connecting (`not ready: idle`). Returns the parsed reply envelope.
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
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("register_session never became ready");
}

async fn run_shell(mcp: &KaijutsuMcp, command: &str) -> serde_json::Value {
    let out = mcp
        .shell(Parameters(ShellRequest {
            command: command.to_string(),
            timeout_secs: Some(30),
        }))
        .await;
    serde_json::from_str(&out).unwrap_or_else(|e| panic!("shell reply was not JSON: {e}: {out}"))
}

/// The core regression guard: a second MCP session (a fresh actor — the
/// reconnect-after-a-dropped-session shape) registering with the SAME label
/// as a still-live context must attach to it, not hard-fail on the
/// KernelDb label-uniqueness constraint. The reply must carry
/// `"resumed": true`, and the attached session's document/shell machinery
/// must actually work (not just a bare id match).
#[test]
fn reconnect_with_same_label_attaches_instead_of_conflicting() {
    run_local(async {
        let addr = start_server().await;
        let label = "upsert-reconnect-test";

        // First "process": registers, does something, then goes away —
        // simulating a dropped MCP session (crash, client restart, …). The
        // server-side context is never explicitly torn down (nothing
        // unregisters a context on client disconnect), which is exactly
        // what makes the old code's blind `create_context_typed` retry
        // fatal: the label is still claimed.
        let first_context_id = {
            let mcp1 = connect_mcp(addr).await;
            let reg1 = register_with_retry(&mcp1, label).await;
            assert!(
                reg1.get("success").and_then(|v| v.as_bool()).unwrap_or(false),
                "first register_session did not succeed: {reg1}"
            );
            assert_eq!(
                reg1.get("resumed").and_then(|v| v.as_bool()),
                Some(false),
                "a brand-new label must not report resumed: {reg1}"
            );
            let out = run_shell(&mcp1, "echo first-session").await;
            assert_eq!(out["stdout"].as_str(), Some("first-session\n"));
            reg1["context_id"].as_str().unwrap().to_string()
            // mcp1 (and its actor, doc task, event bridge) drops here.
        };

        // Second "process": a completely fresh actor/session, same label.
        let mcp2 = connect_mcp(addr).await;
        let reg2 = register_with_retry(&mcp2, label).await;
        assert!(
            reg2.get("success").and_then(|v| v.as_bool()).unwrap_or(false),
            "reconnect register_session did not succeed — got the old fatal \
             label-conflict behavior back? envelope: {reg2}"
        );
        assert_eq!(
            reg2.get("resumed").and_then(|v| v.as_bool()),
            Some(true),
            "reconnecting with a label that names a live context must report \
             resumed: true: {reg2}"
        );
        assert_eq!(
            reg2["context_id"].as_str(),
            Some(first_context_id.as_str()),
            "attach must return the SAME context id as the original registration"
        );
        assert_eq!(
            reg2.get("previous_context"),
            Some(&serde_json::Value::Null),
            "attach is not the concluded/archived case — previous_context must be null: {reg2}"
        );

        // The attached session's shell/document machinery must actually
        // work — not just an id match.
        let out = run_shell(&mcp2, "echo second-session").await;
        assert_eq!(
            out["stdout"].as_str(),
            Some("second-session\n"),
            "shell exec must work through the attached (resumed) context: {out}"
        );
    });
}

/// The concluded-context half: a label that names a CONCLUDED context must
/// not be silently resurrected. `register_session` must create a fresh
/// context under a deterministic suffixed label and report the old one in
/// `previous_context`.
#[test]
fn concluded_context_gets_fresh_suffixed_label_not_resurrected() {
    run_local(async {
        let addr = start_server().await;
        let label = "upsert-concluded-test";

        let mcp1 = connect_mcp(addr).await;
        let reg1 = register_with_retry(&mcp1, label).await;
        assert!(reg1.get("success").and_then(|v| v.as_bool()).unwrap_or(false));
        let original_context_id = reg1["context_id"].as_str().unwrap().to_string();

        // Conclude it — the explicit "this work is done" act — via the
        // actor directly (no MCP tool wraps `conclude` today).
        let Backend::Remote(remote1) = mcp1.backend() else {
            panic!("expected Remote backend");
        };
        let ctx_id = {
            let guard = remote1.joined.read().await;
            guard.as_ref().expect("must be joined").context_id
        };
        remote1
            .actor
            .conclude(ctx_id)
            .await
            .expect("conclude must succeed");
        drop(mcp1);

        // Reconnect with the SAME label.
        let mcp2 = connect_mcp(addr).await;
        let reg2 = register_with_retry(&mcp2, label).await;
        assert!(
            reg2.get("success").and_then(|v| v.as_bool()).unwrap_or(false),
            "register_session must not hard-fail when the label names a \
             concluded context: {reg2}"
        );
        assert_eq!(
            reg2.get("resumed").and_then(|v| v.as_bool()),
            Some(false),
            "a concluded context must never be silently resumed: {reg2}"
        );

        let new_context_id = reg2["context_id"].as_str().unwrap().to_string();
        assert_ne!(
            new_context_id, original_context_id,
            "a concluded context must not be reused — a fresh context is required"
        );

        let new_label = reg2["label"].as_str().unwrap();
        assert_ne!(
            new_label, label,
            "the fresh context must NOT reuse the concluded context's exact label \
             (KernelDb's label-uniqueness index still holds it even after conclude)"
        );
        assert!(
            new_label.starts_with(&format!("{label}-")),
            "the fresh label must be a deterministic suffix of the requested one, \
             got '{new_label}'"
        );

        let previous = reg2
            .get("previous_context")
            .filter(|v| !v.is_null())
            .expect("previous_context must be populated for the concluded-context case");
        assert_eq!(previous["context_id"].as_str(), Some(original_context_id.as_str()));
        assert!(
            previous.get("concluded_at").map(|v| !v.is_null()).unwrap_or(false),
            "previous_context must carry concluded_at: {previous}"
        );

        // The fresh context must actually work.
        let out = run_shell(&mcp2, "echo fresh-context").await;
        assert_eq!(out["stdout"].as_str(), Some("fresh-context\n"));
    });
}
