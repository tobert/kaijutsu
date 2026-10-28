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

use kaijutsu_client::{ConnectionStatus, KeySource, SshConfig};
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
    KaijutsuMcp::connect_with_config(config, "e2e-test", Some("e2e-session")).with_character(Some(SshServerConfig::EPHEMERAL_PERFORMER.to_string()))
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
                run_in_background: false,
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

/// A root connection with no `--character` has nobody to play the context,
/// so registration refuses and names the option and its variable.
#[test]
fn a_root_session_without_a_character_refuses_and_names_the_option() {
    run_local(async {
        let addr = start_server().await;
        let mcp = connect_mcp(addr).await.with_character(None);
        let mut raw = String::new();
        for _ in 0..100 {
            raw = mcp
                .register_session(Parameters(RegisterSessionRequest {
                    label: Some("no-performer".to_string()),
                    context_type: None,
                }))
                .await;
            if !raw.contains("not ready") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(raw.contains("--character") && raw.contains("KAIJUTSU_CHARACTER"), "{raw}");
        assert!(raw.contains(SshServerConfig::EPHEMERAL_ROOT), "{raw}");
    });
}

/// With `--character`, the root connection's session context is played by
/// that character.
#[test]
fn a_root_session_with_a_character_creates_a_context_that_character_plays() {
    run_local(async {
        let addr = start_server().await;
        let mcp = connect_mcp(addr).await;
        let reg = register_with_retry(&mcp, "has-performer").await;
        assert!(reg.get("success").and_then(|v| v.as_bool()).unwrap_or(false), "{reg}");
        let id = reg["context_id"].as_str().expect("the reply names the context").to_string();
        let Backend::Remote(remote) = mcp.backend() else { unreachable!("connect_mcp is remote") };
        let id = kaijutsu_types::ContextId::parse(&id).expect("a context id");
        let info = remote
            .actor
            .execute_kj_quiet(id, vec!["context".into(), "info".into(), id.to_hex()])
            .await
            .expect("run kj context info");
        assert_eq!(info.exit_code, 0, "{}", info.stderr);
        let played_by = info.data.as_ref().and_then(|d| d.get("played_by_name")).and_then(|n| n.as_str());
        assert_eq!(played_by, Some(SshServerConfig::EPHEMERAL_PERFORMER));
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

/// Wait until `mcp`'s actor holds a live connection.
async fn wait_connected(mcp: &KaijutsuMcp) {
    let Backend::Remote(remote) = mcp.backend() else { unreachable!("connect_mcp is remote") };
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        remote.actor.watch_status().wait_for(|s| matches!(s, ConnectionStatus::Connected { .. })),
    )
    .await
    .expect("the actor connects to the ephemeral kernel")
    .expect("the actor is running");
}

/// A kernel that answers no ends startup registration: the window reports a
/// refusal rather than no answer, and the deferred wait returns instead of
/// retrying.
#[test]
fn a_verdict_ends_auto_registration() {
    use std::time::Duration;

    run_local(async {
        let addr = start_server().await;
        let mcp = connect_mcp(addr).await.with_parent(Some("no-such-parent".to_string()));
        wait_connected(&mcp).await;

        let window = mcp.auto_register("refused-seat", &[Duration::ZERO]).await;
        assert!(
            matches!(&window, AutoRegistration::Refused(reply) if reply.contains("no-such-parent")),
            "{window:?}"
        );

        let late = tokio::time::timeout(Duration::from_secs(10), mcp.register_when_connected("refused-seat", &[Duration::from_secs(1)]))
            .await
            .expect("a verdict must end the wait");
        assert!(
            matches!(&late, AutoRegistration::Refused(reply) if reply.contains("no-such-parent")),
            "{late:?}"
        );
    });
}

/// A model's `register_session` racing startup registration yields one
/// join, and startup reports it as another caller's so the hook listener
/// leaves that label alone.
#[test]
fn a_manual_register_racing_startup_registration_joins_once() {
    run_local(async {
        let addr = start_server().await;
        let mcp = connect_mcp(addr).await;
        wait_connected(&mcp).await;

        let manual = mcp.register_session(Parameters(RegisterSessionRequest {
            label: Some("chosen-by-the-model".to_string()),
            context_type: None,
        }));
        let startup = mcp.register_when_connected("cc-startup-label", &[]);
        // `join!` polls the manual call first, so it takes the lock first.
        let (manual, startup) = tokio::join!(manual, startup);
        let manual: serde_json::Value = serde_json::from_str(&manual).expect("register_session replies JSON");

        assert_eq!(manual["success"].as_bool(), Some(true), "{manual}");
        assert_eq!(startup, AutoRegistration::AlreadyJoined);
        let Backend::Remote(remote) = mcp.backend() else { unreachable!() };
        let labels: Vec<String> =
            remote.actor.list_contexts().await.unwrap().into_iter().map(|c| c.label).collect();
        assert!(!labels.iter().any(|l| l == "cc-startup-label"), "{labels:?}");
    });
}

/// A model's session created in Amy's root context waits for her approval.
/// The reply says the session is not joined and how to join, and once Amy
/// allows the create, registering again with the same label joins the
/// context the approved create made.
#[test]
fn a_held_create_says_how_to_join_and_joins_after_approval() {
    run_local(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let config = SshServerConfig::ephemeral_with_root(addr.port(), "amy");
        let gate = config.config_mounts.host_dir(kaijutsu_types::paths::CONFIG_ROOT).join("gate.toml");
        std::fs::create_dir_all(gate.parent().unwrap()).unwrap();
        std::fs::copy(concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/defaults/gate.toml"), &gate).unwrap();
        let amy_key = KeySource::InMemory(config.root_key());
        let claude_key = KeySource::ephemeral();
        let KeySource::InMemory(claude_private) = &claude_key else { unreachable!("an ephemeral key is in memory") };
        let claude = kaijutsu_types::PrincipalId::new();
        kaijutsu_server::AuthDb::open(config.auth_db_path.as_ref().unwrap())
            .unwrap()
            .add_key(claude, claude_private.public_key(), Some("claude"))
            .unwrap();
        let (kernel_tx, kernel_rx) = tokio::sync::oneshot::channel();
        tokio::task::spawn_local(async move {
            SshServer::new(config).run_on_listener_with_kernel_sink(listener, kernel_tx).await.unwrap();
        });
        let kernel = kernel_rx.await.unwrap();
        kernel.kernel_db.lock().insert_character(&kaijutsu_kernel::kernel_db::CharacterRow {
            principal_id: claude, name: "claude".into(), created_at: 0, retired_at: None,
            handoff_ctx: None, root_ctx: None, root: false,
        }).unwrap();
        let connect = |name: &str, key_source: KeySource| {
            KaijutsuMcp::connect_with_config(
                SshConfig { host: addr.ip().to_string(), port: addr.port(), username: name.into(), key_source, insecure: true },
                "e2e-test",
                Some("e2e-session"),
            )
        };

        let mcp = connect("claude", claude_key.clone());
        wait_connected(&mcp).await;
        let held = mcp
            .register_session(Parameters(RegisterSessionRequest { label: Some("held-seat".into()), context_type: None }))
            .await;
        let ask = kernel.kernel_db.lock().list_pending_asks().unwrap().pop().expect("the create waits for Amy");
        assert!(held.contains(&ask.request_id), "the reply names the ask: {held}");
        assert!(held.contains("not joined") && held.contains("register_session again"), "the reply says how to join: {held}");

        let amy = connect("amy", amy_key);
        wait_connected(&amy).await;
        let Backend::Remote(amy_remote) = amy.backend() else { unreachable!("connect is remote") };
        let root = kaijutsu_client::choose_parent(None, &amy_remote.actor.list_contexts().await.unwrap()).unwrap();
        let allowed = amy_remote
            .actor
            .execute_kj(root.context_id, vec!["ledger".into(), "allow".into(), ask.request_id.clone()])
            .await
            .unwrap();
        assert_eq!(allowed.exit_code, 0, "kj ledger allow: {} {}", allowed.stdout, allowed.stderr);

        let Backend::Remote(remote) = mcp.backend() else { unreachable!("connect is remote") };
        let created = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                if let Some(context) = remote.actor.resolve_context_label("held-seat").await.unwrap() {
                    break context;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the approved create lands");
        let joined = register_with_retry(&mcp, "held-seat").await;
        assert_eq!(joined["success"].as_bool(), Some(true), "{joined}");
        assert_eq!(joined["resumed"].as_bool(), Some(true), "{joined}");
        assert_eq!(joined["context_id"].as_str(), Some(created.id.to_hex().as_str()), "{joined}");
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

        // Conclude it — the explicit "this work is done" act — with `kj
        // context conclude` run in the context itself.
        let Backend::Remote(remote1) = mcp1.backend() else {
            panic!("expected Remote backend");
        };
        let ctx_id = {
            let guard = remote1.joined.read().await;
            guard.as_ref().expect("must be joined").context_id
        };
        let concluded = remote1
            .actor
            .execute_kj(ctx_id, vec!["context".into(), "conclude".into(), ctx_id.to_hex()])
            .await
            .expect("kj context conclude runs");
        assert_eq!(concluded.exit_code, 0, "conclude must succeed: {}", concluded.stderr);
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

        // Archive the bound context from its parent — the path this
        // process's own deferred session-end archive takes, standing in too
        // for another player's `kj context archive`. `mcp`'s `remote.joined` still
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
            .archive_context_from_parent(ctx_id)
            .await
            .expect("kj context archive must succeed");

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
