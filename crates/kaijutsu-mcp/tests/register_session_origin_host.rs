//! End-to-end tests for `register_session`'s `origin_host` stamping
//! (docs/issues.md, "cc-* hook re-registration mints a new context per MCP
//! relaunch" — last paragraph: "Host field on registration ... would make
//! the fleet board self-evident").
//!
//! `register_session_impl` calls the new `setContextOriginHost` RPC with its
//! own `hostname::get()` right after a FRESH context is created — never on
//! an attach/resume, mirroring `created_by`/`created_at` (an origin fact,
//! not a "last seen from" one). These tests exercise that through the real
//! `KaijutsuMcp` entry point (not the bare RPC — `kaijutsu-server`'s
//! `context_origin_host.rs` already covers the wire/DB layer directly), so
//! a regression in the kaijutsu-mcp-side wiring (wrong branch, wrong call
//! site, swallowed differently) shows up here even if the RPC itself is
//! fine.
//!
//! Same harness shape as `register_session_upsert.rs` (a real ephemeral SSH
//! server, driven exactly as a connected agent would) — duplicated rather
//! than shared, matching that file's own precedent of not sharing a
//! `common` module with `e2e_shell.rs`.

use std::net::SocketAddr;

use rmcp::handler::server::wrapper::Parameters;
use tokio::net::TcpListener;
use tokio::task::LocalSet;

use kaijutsu_client::{KeySource, SshConfig};
use kaijutsu_mcp::{Backend, KaijutsuMcp, RegisterSessionRequest};
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

/// A fresh `register_session` must stamp `origin_host` with the real local
/// hostname (the same value `kaijutsu-app`'s `sink_host()` would report) —
/// proving the kaijutsu-mcp-side wiring actually calls through, not just
/// that the RPC exists.
#[test]
fn register_session_stamps_origin_host_on_a_fresh_context() {
    run_local(async {
        let addr = start_server().await;
        let label = "origin-host-fresh-test";

        let mcp = connect_mcp(addr).await;
        let reg = register_with_retry(&mcp, label).await;
        assert!(reg.get("success").and_then(|v| v.as_bool()).unwrap_or(false));

        let Backend::Remote(remote) = mcp.backend() else {
            panic!("expected Remote backend");
        };
        let ctx_id = {
            let guard = remote.joined.read().await;
            guard.as_ref().expect("must be joined").context_id
        };

        let expected_host = hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_default();
        assert!(!expected_host.is_empty(), "test environment must report a real hostname");

        let resolved = remote
            .actor
            .resolve_context_label(label)
            .await
            .unwrap()
            .expect("just-registered label must resolve");
        assert_eq!(resolved.id, ctx_id);
        assert_eq!(
            resolved.origin_host,
            Some(expected_host),
            "register_session must stamp origin_host with this process's own hostname"
        );
    });
}

/// The "origin fact, not last-seen" invariant: reconnecting with the SAME
/// label (the `resumed: true` attach path) must NOT re-stamp `origin_host`
/// — even though the reconnecting process is (in this test) the very same
/// machine. Simulated here by hand-setting a sentinel value between the two
/// registrations; if `register_session_impl` ever starts calling
/// `setContextOriginHost` on the attach branch too, this sentinel gets
/// clobbered back to the real hostname and the test catches it.
#[test]
fn register_session_does_not_restamp_origin_host_on_resume() {
    run_local(async {
        let addr = start_server().await;
        let label = "origin-host-resume-test";

        let first_context_id = {
            let mcp1 = connect_mcp(addr).await;
            let reg1 = register_with_retry(&mcp1, label).await;
            assert!(reg1.get("success").and_then(|v| v.as_bool()).unwrap_or(false));
            assert_eq!(reg1.get("resumed").and_then(|v| v.as_bool()), Some(false));

            let Backend::Remote(remote1) = mcp1.backend() else {
                panic!("expected Remote backend");
            };
            let ctx_id = {
                let guard = remote1.joined.read().await;
                guard.as_ref().expect("must be joined").context_id
            };
            // Overwrite the freshly-stamped real hostname with a sentinel —
            // a resume must leave this alone.
            remote1
                .actor
                .set_context_origin_host(ctx_id, "sentinel-other-machine")
                .await
                .expect("manual set_context_origin_host must succeed");
            ctx_id
            // mcp1 drops here — simulating the process going away.
        };

        let mcp2 = connect_mcp(addr).await;
        let reg2 = register_with_retry(&mcp2, label).await;
        assert!(reg2.get("success").and_then(|v| v.as_bool()).unwrap_or(false));
        assert_eq!(
            reg2.get("resumed").and_then(|v| v.as_bool()),
            Some(true),
            "same label, still-live context must attach, not create fresh: {reg2}"
        );
        assert_eq!(reg2["context_id"].as_str(), Some(first_context_id.to_hex().as_str()));

        let Backend::Remote(remote2) = mcp2.backend() else {
            panic!("expected Remote backend");
        };
        let resolved = remote2
            .actor
            .resolve_context_label(label)
            .await
            .unwrap()
            .expect("label still resolves");
        assert_eq!(
            resolved.origin_host,
            Some("sentinel-other-machine".to_string()),
            "a resumed (attached) registration must NOT overwrite an existing \
             origin_host — it is an origin fact set once at creation, never a \
             'last seen from' one"
        );
    });
}
