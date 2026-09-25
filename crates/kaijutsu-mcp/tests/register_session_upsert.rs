//! End-to-end tests for `register_session`'s upsert/attach behavior.
//!
//! The bug: `register_session_impl` used to check idempotency only for
//! the CURRENT process (`remote.joined`), then unconditionally call
//! `create_context_typed(&label, ...)`. The label defaults to the harness's
//! agent-session id, so a reconnect after a dropped MCP session reused the
//! exact same label and hit KernelDb's label-uniqueness constraint as a
//! fatal `insert_context failed ... label conflict`.
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

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Mutex, OnceLock};

use rmcp::handler::server::wrapper::Parameters;
use tokio::net::TcpListener;
use tokio::task::LocalSet;

use kaijutsu_client::{KeySource, SshConfig};
use kaijutsu_mcp::{AutoRegistration, Backend, KaijutsuMcp, RegisterSessionRequest, ShellRequest};
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

/// The root key each ephemeral server binds, keyed by its address —
/// `SshServerConfig::ephemeral` mints a fresh one per call, so `connect_mcp`
/// needs the exact key `start_server` registered here rather than an
/// unrecognized ephemeral one of its own.
static ROOT_KEYS: OnceLock<Mutex<HashMap<SocketAddr, KeySource>>> = OnceLock::new();

fn root_keys() -> &'static Mutex<HashMap<SocketAddr, KeySource>> {
    ROOT_KEYS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Start an ephemeral SSH server on a random port; return its address.
async fn start_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = SshServerConfig::ephemeral(addr.port());
    root_keys()
        .lock()
        .unwrap()
        .insert(addr, KeySource::InMemory(config.root_key()));

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
/// Authenticates with the root key `start_server` bound for `addr`.
async fn connect_mcp(addr: SocketAddr) -> KaijutsuMcp {
    let key_source = root_keys()
        .lock()
        .unwrap()
        .get(&addr)
        .cloned()
        .expect("connect_mcp requires a server started through this file's start_server");
    let config = SshConfig {
        host: addr.ip().to_string(),
        port: addr.port(),
        username: "test_user".to_string(),
        key_source,
        insecure: true,
    };
    KaijutsuMcp::connect_with_config(config, "e2e-test", Some("e2e-session"))
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
        .shell_impl(ShellRequest {
                foreground: true,
            command: command.to_string(),
            timeout_secs: Some(30),
        }, None)
        .await;
    out.structured_content
        .clone()
        .unwrap_or_else(|| panic!("shell reply carried no structuredContent: {out:?}"))
}

/// A new session's context is created under the kernel's only root context,
/// and the reply names the parent and why it was chosen.
#[test]
fn a_new_session_is_created_under_the_only_root_context() {
    run_local(async {
        let addr = start_server().await;
        let mcp = connect_mcp(addr).await;
        let reg = register_with_retry(&mcp, "parent-test").await;
        assert!(reg.get("success").and_then(|v| v.as_bool()).unwrap_or(false), "{reg}");
        assert_eq!(reg["parent"]["label"].as_str(), Some(SshServerConfig::EPHEMERAL_ROOT), "{reg}");
        assert_eq!(reg["parent"]["source"].as_str(), Some("only_root"), "{reg}");
        assert!(reg["parent"]["context_id"].as_str().is_some_and(|id| id != reg["context_id"].as_str().unwrap()), "{reg}");
    });
}

/// With several root contexts and no named parent, registration refuses and
/// lists them instead of choosing one.
#[test]
fn several_roots_without_a_named_parent_refuse() {
    run_local(async {
        let addr = start_server().await;
        let mcp = connect_mcp(addr).await;
        let first = register_with_retry(&mcp, "seat").await;
        assert!(first.get("success").and_then(|v| v.as_bool()).unwrap_or(false), "{first}");
        let out = run_shell(&mcp, "kj character create second-root --root").await;
        assert_eq!(out["exit_code"].as_i64(), Some(0), "{out}");

        let other = connect_mcp(addr).await;
        let mut raw = String::new();
        for _ in 0..100 {
            raw = other
                .register_session(Parameters(RegisterSessionRequest {
                    label: Some("needs-a-parent".to_string()),
                    context_type: None,
                }))
                .await;
            if !raw.contains("not ready") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(raw.contains("second-root") && raw.contains("--parent"), "{raw}");
    });
}

/// A kernel that answers and refuses ends startup registration: the window
/// reports a refusal rather than an unreachable kernel, and the wait for a
/// connection returns instead of retrying on every reconnect.
#[test]
fn a_refusal_on_a_live_connection_ends_auto_registration() {
    use std::time::Duration;

    run_local(async {
        let addr = start_server().await;
        let mcp = connect_mcp(addr).await.with_parent(Some("no-such-parent".to_string()));
        let delays = [Duration::ZERO, Duration::from_millis(250), Duration::from_secs(1), Duration::from_secs(2)];

        let window = mcp.auto_register("refused-seat", &delays).await;
        assert!(
            matches!(&window, AutoRegistration::Refused(reply) if reply.contains("no-such-parent")),
            "{window:?}"
        );

        let late = tokio::time::timeout(Duration::from_secs(10), mcp.register_when_connected("refused-seat"))
            .await
            .expect("a refusal on a live connection must end the wait");
        assert!(
            matches!(&late, AutoRegistration::Refused(reply) if reply.contains("no-such-parent")),
            "{late:?}"
        );
    });
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

/// The already-joined fast path must not trap a session on a context that
/// became archived out from under it (e.g. another player's `kj context
/// archive`, or this process's own deferred session-end archive at
/// shutdown — see `hook_listener.rs`'s `archive_if_session_ended`).
/// `register_session_impl` used to answer `already_registered` with
/// whatever `remote.joined` held, unconditionally — once that context was
/// archived, every later `register_session` call (and every `shell` call)
/// stayed stuck on a dead context with no way back short of a fresh MCP
/// process. It must instead rebind to a fresh context, exactly as the
/// documented behavior for an archived label promises (`previous_context`
/// in the reply).
#[test]
fn a_bound_context_archived_out_from_under_the_session_gets_rebound() {
    run_local(async {
        let addr = start_server().await;
        let label = "upsert-archived-while-bound-test";

        let mcp = connect_mcp(addr).await;
        let reg1 = register_with_retry(&mcp, label).await;
        assert!(reg1.get("success").and_then(|v| v.as_bool()).unwrap_or(false), "{reg1}");
        let original_context_id = reg1["context_id"].as_str().unwrap().to_string();

        // Archive the bound context directly through the kernel — standing
        // in for another player's `kj context archive`, or this process's
        // own deferred session-end archive. `mcp`'s `remote.joined` still
        // names this context; nothing on the MCP side has heard about the
        // archive yet.
        let Backend::Remote(remote) = mcp.backend() else {
            panic!("expected Remote backend");
        };
        let ctx_id = {
            let guard = remote.joined.read().await;
            guard.as_ref().expect("must be joined").context_id
        };
        remote
            .actor
            .archive_context(ctx_id)
            .await
            .expect("archive_context must succeed");

        // Same MCP session, same label: the naive `already_registered` fast
        // path would answer with the now-archived `original_context_id`.
        let reg2 = register_with_retry(&mcp, label).await;
        assert!(
            reg2.get("success").and_then(|v| v.as_bool()).unwrap_or(false),
            "register_session must rebind, not report already_registered on a \
             dead context: {reg2}"
        );
        assert_ne!(
            reg2.get("already_registered"),
            Some(&serde_json::Value::Bool(true)),
            "must not answer already_registered with an archived context: {reg2}"
        );

        let new_context_id = reg2["context_id"].as_str().unwrap().to_string();
        assert_ne!(
            new_context_id, original_context_id,
            "the archived context must not be reused — a fresh context is required"
        );

        let previous = reg2
            .get("previous_context")
            .filter(|v| !v.is_null())
            .expect("previous_context must be populated for the archived-while-bound case");
        assert_eq!(previous["context_id"].as_str(), Some(original_context_id.as_str()));
        assert_eq!(previous["archived"].as_bool(), Some(true), "{previous}");

        // The fresh context must actually work through the SAME session.
        let out = run_shell(&mcp, "echo rebound").await;
        assert_eq!(out["stdout"].as_str(), Some("rebound\n"), "{out}");
    });
}

/// `register_session` attaches the MCP session as a peer (docs/
/// instrument-design.md, "Many hands, one trust boundary" — every connected
/// client registers so the room can render who's at the table). Nick follows
/// the `mcp/<label>` convention; presence must be visible via `listPeers`
/// through the same actor the session used to register.
#[test]
fn register_session_attaches_as_mcp_peer() {
    run_local(async {
        let addr = start_server().await;
        let label = "peer-attach-test";

        let mcp = connect_mcp(addr).await;
        let reg = register_with_retry(&mcp, label).await;
        assert!(
            reg.get("success").and_then(|v| v.as_bool()).unwrap_or(false),
            "register_session did not succeed: {reg}"
        );

        let Backend::Remote(remote) = mcp.backend() else {
            panic!("expected Remote backend");
        };

        let expected_nick = format!("mcp/{label}");
        let peers = remote
            .actor
            .list_peers()
            .await
            .expect("list_peers must succeed");
        assert!(
            peers.iter().any(|p| p.nick == expected_nick),
            "expected peer '{expected_nick}' to be attached after register_session, got {peers:?}"
        );
    });
}
