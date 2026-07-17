//! Remote-mode hook listener e2e.
//!
//! Exercises the pieces of the hook path that only make sense against a live
//! actor + kernel and can't be unit-tested against a bare in-memory
//! document: auto-register (`register_session_auto`), `session.start`'s
//! once-only label rename, and the remote-mode tool-call completion fix
//! (`insert_tool_blocks` setting the `ToolCall` block's final status).
//!
//! Mirrors the ephemeral-SSH-server harness in `tests/e2e_shell.rs` (kept
//! separate on purpose — that file, and `tests/adapter_mapping.rs`, are
//! owned by other work in flight right now).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::task::LocalSet;

use kaijutsu_client::{KeySource, SshConfig};
use kaijutsu_crdt::{BlockKind, Status};
use kaijutsu_mcp::hook_listener::{HookListener, send_hook_event};
use kaijutsu_mcp::{Backend, KaijutsuMcp};
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

/// Connect a `KaijutsuMcp` to the ephemeral server. `cc_session_id: None`
/// mirrors the real startup case this suite cares about — session id
/// unknown until the first hook event.
async fn connect_mcp(addr: SocketAddr) -> KaijutsuMcp {
    let config = SshConfig {
        host: addr.ip().to_string(),
        port: addr.port(),
        username: "test_user".to_string(),
        key_source: KeySource::ephemeral(),
        insecure: true,
    };
    KaijutsuMcp::connect_with_config(config, "hook-e2e-test", None)
        .await
        .expect("MCP connect failed")
}

/// Auto-register with retry — the freshly-spawned actor needs a moment to
/// finish connecting before RPCs succeed (mirrors `register_with_retry` in
/// `tests/e2e_shell.rs`).
async fn auto_register_with_retry(mcp: &KaijutsuMcp, label: &str) -> serde_json::Value {
    for _ in 0..100 {
        let raw = mcp
            .register_session_auto(Some(label.to_string()), None)
            .await;
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("register_session_auto never became ready");
}

fn unique_socket_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "kaijutsu-mcp-hook-e2e-{tag}-{}-{}.sock",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ))
}

/// Bind `listener` on a fresh temp socket and wait for it to exist. Returns
/// the socket path.
async fn spawn_listener(listener: Arc<HookListener>, tag: &str) -> PathBuf {
    let socket_path = unique_socket_path(tag);
    let bg_path = socket_path.clone();
    tokio::spawn(async move {
        let _ = listener.start(bg_path).await;
    });
    for _ in 0..100 {
        if socket_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(socket_path.exists(), "hook socket never bound");
    socket_path
}

/// The `renameContext` RPC (`kaijutsu.capnp` `kernel::renameContext @29`)
/// end to end: rename lands in the kernel (visible via `list_contexts`),
/// and a label another context already holds is refused with an error
/// rather than silently stolen — label theft is `kj context retag`'s
/// explicitly-confirmed job, not this RPC's.
#[test]
fn rename_context_rpc_renames_and_refuses_taken_labels() {
    run_local(async {
        let addr = start_server().await;
        let mcp = connect_mcp(addr).await;
        let reg = auto_register_with_retry(&mcp, "rename-e2e-original").await;
        assert!(
            reg.get("success").and_then(|v| v.as_bool()).unwrap_or(false),
            "register_session_auto failed: {reg}"
        );
        let context_id = reg["context_id"].as_str().unwrap();
        let context_id = kaijutsu_crdt::ContextId::parse(context_id).unwrap();

        let Backend::Remote(remote) = mcp.backend().clone() else {
            panic!("expected remote backend");
        };
        remote
            .actor
            .rename_context(context_id, "rename-e2e-renamed")
            .await
            .expect("rename_context should succeed");
        let contexts = remote.actor.list_contexts().await.unwrap();
        assert!(
            contexts.iter().any(|c| c.label == "rename-e2e-renamed"),
            "renamed label not visible in list_contexts: {contexts:?}"
        );
        assert!(
            !contexts.iter().any(|c| c.label == "rename-e2e-original"),
            "old label still present after rename: {contexts:?}"
        );

        // A second context may not take the same label — proves the first
        // rename really persisted (the uniqueness constraint sees it).
        let other = remote
            .actor
            .create_context("rename-e2e-other")
            .await
            .expect("create second context");
        let err = remote
            .actor
            .rename_context(other, "rename-e2e-renamed")
            .await
            .expect_err("renaming onto a taken label must fail, not steal it");
        let msg = err.to_string();
        assert!(
            !msg.contains("not implemented"),
            "server still lacks the rename_context handler: {msg}"
        );
    });
}

/// Item 4 + 5: `session.start` performs the label rename and model set
/// exactly once — the first event's session id wins the suffix, a second
/// `session.start` (different id) must not rename again — and the listener
/// stays live and answering throughout.
#[test]
fn session_start_renames_label_once_and_listener_stays_live() {
    run_local(async {
        let addr = start_server().await;
        let mcp = connect_mcp(addr).await;

        // The label an auto-register would generate when the session id
        // isn't known yet at startup (see `main.rs::auto_register_label`).
        let label = "cc-testdir-0101-0000";
        let reg = auto_register_with_retry(&mcp, label).await;
        assert!(
            reg.get("success").and_then(|v| v.as_bool()).unwrap_or(false),
            "register_session_auto failed: {reg}"
        );

        let Backend::Remote(remote) = mcp.backend().clone() else {
            panic!("expected remote backend");
        };
        let listener = Arc::new(HookListener::remote(
            remote.clone(),
            Arc::clone(&remote.shared_context_id),
            Arc::clone(mcp.session_id_arc()),
            Some(label.to_string()),
        ));
        let socket_path = spawn_listener(listener, "rename").await;

        for session_id in [
            "11112222-3333-4444-5555-666677778888",
            "99998888-7777-6666-5555-444433332222",
        ] {
            let event = serde_json::json!({
                "event": "session.start",
                "source": "claude-code",
                "session_id": session_id,
                "model": "claude-opus-4-8",
            })
            .to_string();
            let response = send_hook_event(&socket_path, &event)
                .await
                .unwrap()
                .expect("hook socket must still answer after a failed rename_context call");
            assert!(!response.trim().is_empty(), "hook response must not be empty");
        }

        // Exactly-once: the FIRST session.start's id owns the suffix; the
        // second event (different id) must not rename again.
        let expected = format!("{label}-11112222");
        let contexts = remote.actor.list_contexts().await.unwrap();
        assert!(
            contexts.iter().any(|c| c.label == expected),
            "context not renamed to first session's suffix ({expected}): {contexts:?}"
        );
        assert!(
            !contexts.iter().any(|c| c.label.ends_with("-99998888")),
            "second session.start renamed again — at-most-once violated: {contexts:?}"
        );

        // The listener must still be alive and answering after both events
        // (a wedged/panicked handler would fail the socket send above).
        let ping = send_hook_event(
            &socket_path,
            r#"{"event":"ping","source":"claude-code"}"#,
        )
        .await
        .unwrap()
        .expect("listener must still be responsive after fail-open rename attempts");
        assert!(ping.contains("\"status\":\"ok\""));
    });
}

/// Item 8, remote-mode mirror of the local-mode unit test in
/// `hook_listener.rs`: a hook-authored `tool.after` must complete the
/// `ToolCall` block (Status::Done), not leave it Running forever.
#[test]
fn tool_after_completes_the_call_block_remote_mode() {
    run_local(async {
        let addr = start_server().await;
        let mcp = connect_mcp(addr).await;
        let reg = auto_register_with_retry(&mcp, "hook-tool-e2e").await;
        assert!(
            reg.get("success").and_then(|v| v.as_bool()).unwrap_or(false),
            "register_session_auto failed: {reg}"
        );

        let Backend::Remote(remote) = mcp.backend().clone() else {
            panic!("expected remote backend");
        };
        let listener = Arc::new(HookListener::remote(
            remote.clone(),
            Arc::clone(&remote.shared_context_id),
            Arc::clone(mcp.session_id_arc()),
            None,
        ));
        let socket_path = spawn_listener(listener, "toolcall").await;

        let event = serde_json::json!({
            "event": "tool.after",
            "source": "claude-code",
            "tool": {
                "name": "Bash",
                "input": {"command": "ls"},
                "output": "total 0",
            },
        })
        .to_string();
        send_hook_event(&socket_path, &event).await.unwrap();

        // insert_tool_blocks writes into `remote.synced` synchronously
        // before the socket response is sent; a short grace period guards
        // against scheduling jitter, not a real race.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let blocks = remote.synced.lock().as_ref().unwrap().blocks();
        let call = blocks
            .iter()
            .find(|b| b.kind == BlockKind::ToolCall)
            .expect("tool call block inserted");
        assert_eq!(
            call.status,
            Status::Done,
            "hook-authored tool call must complete in remote mode too"
        );
    });
}
