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
use kaijutsu_types::{BlockKind, Status};
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
        let context_id = kaijutsu_types::ContextId::parse(context_id).unwrap();

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

/// **Slice 0 of the storage-position migration** (docs/crdt-position-2026-08.md,
/// "Build notes"): the acceptance test that has to exist *before* the MCP is
/// moved off client-side replication.
///
/// **Written before the migration, and it did its job.** At the time, its
/// sibling `tool_after_completes_the_call_block_remote_mode` (deleted —
/// see below) asserted against `remote.synced`, the process-local mirror
/// `insert_tool_blocks` wrote to synchronously, so it would have passed
/// even if `push_ops` silently failed and nothing ever reached the kernel.
/// It proved authoring, not delivery.
///
/// This one reads back through `get_all_blocks`, an authoritative **server**
/// query, so it can only pass if the hook-authored blocks genuinely crossed
/// the wire. It passed against `push_ops` and it passes unchanged against
/// RPC authoring — the whole point, since it is written against the
/// *contract* (hook fires ⇒ server holds a correct ToolCall/ToolResult pair)
/// rather than the mechanism.
///
/// (The sibling is gone now, not just strengthened: once slice 4 of
/// docs/crdt-position-2026-08.md deleted `RemoteState.synced` entirely,
/// asserting against it stopped being possible, and its round-trip claim —
/// "the kernel accepted the pair and the event feed carried it back" — is a
/// strict subset of what this test already proves via the server-authoritative
/// read. Keeping both would have meant keeping a mirror alive solely so a
/// duplicate test had something to assert against.)
///
/// It pins the fields the planned `block_create` route would silently drop:
/// `tool_name`, `tool_input`, the pair's parent link, and terminal status.
/// `block_create` parses a `metadata` argument and never reads it, so a
/// migration routed through it would render tool blocks with no name and no
/// input **and nothing would error** — this test is the tripwire for exactly
/// that.
#[test]
fn hook_authored_tool_blocks_reach_the_server_not_just_the_mirror() {
    run_local(async {
        let addr = start_server().await;
        let mcp = connect_mcp(addr).await;
        let reg = auto_register_with_retry(&mcp, "hook-tool-server-e2e").await;
        assert!(
            reg.get("success").and_then(|v| v.as_bool()).unwrap_or(false),
            "register_session_auto failed: {reg}"
        );

        let Backend::Remote(remote) = mcp.backend().clone() else {
            panic!("expected remote backend");
        };
        let context_id = remote
            .shared_context_id
            .lock()
            .expect("context id mutex")
            .expect("registered context");

        let listener = Arc::new(HookListener::remote(
            remote.clone(),
            Arc::clone(&remote.shared_context_id),
            Arc::clone(mcp.session_id_arc()),
            None,
        ));
        let socket_path = spawn_listener(listener, "toolcall-server").await;

        let event = serde_json::json!({
            "event": "tool.after",
            "source": "claude-code",
            "tool": {
                "name": "Bash",
                "input": {"command": "ls -la /tmp"},
                "output": "total 0",
            },
        })
        .to_string();
        send_hook_event(&socket_path, &event).await.unwrap();

        // `authorBlock`/`completeBlock` ack once the kernel accepts the RPC,
        // which says nothing about when (or whether) this listener's own
        // read of the result becomes visible. Poll the server rather than
        // sleeping a fixed grace period — a sleep would make this flaky
        // under load and, worse, would make a genuine delivery failure look
        // like a slow one.
        let mut server_blocks = Vec::new();
        for _ in 0..50 {
            server_blocks = remote
                .actor
                .get_all_blocks(context_id)
                .await
                .expect("get_all_blocks");
            if server_blocks.iter().any(|b| b.kind == BlockKind::ToolCall) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let call = server_blocks
            .iter()
            .find(|b| b.kind == BlockKind::ToolCall)
            .expect(
                "hook-authored ToolCall must be readable from the SERVER, \
                 not merely present in the local mirror",
            );

        assert_eq!(
            call.tool_name.as_deref(),
            Some("Bash"),
            "tool_name must survive the trip to the server"
        );
        assert!(
            call.tool_input
                .as_deref()
                .is_some_and(|i| i.contains("ls -la /tmp")),
            "tool_input must survive the trip to the server, got: {:?}",
            call.tool_input
        );
        assert_eq!(
            call.status,
            Status::Done,
            "the ToolCall must reach a terminal status server-side — an \
             orphan left at Running is the failure mode splitting the \
             mutex-held triple into separate RPCs would introduce"
        );

        let result = server_blocks
            .iter()
            .find(|b| b.kind == BlockKind::ToolResult)
            .expect("hook-authored ToolResult must be readable from the server");
        assert_eq!(
            result.parent_id.as_ref(),
            Some(&call.id),
            "the ToolResult must be parented to its ToolCall server-side — \
             the link `block_create` cannot currently express"
        );
    });
}

/// Hook-authored blocks must belong to the **agent session**, not to a
/// per-process identity and not to `system()`.
///
/// The bug this pins: remote mode carried a `PrincipalId::new()` minted once
/// per MCP *process*, so one Claude Code session authored under a different
/// principal after every `/mcp reconnect`, and a context accumulated blocks
/// from N anonymous principals that were in fact the same agent. Local mode
/// stamped `PrincipalId::system()`, claiming the kernel wrote them.
///
/// Asserting equality with `for_agent_session(sid)` — rather than merely
/// "not random" — is what makes this imply *stability across relaunches*
/// without needing a two-process test: the derivation is proven
/// deterministic by the unit tests in `kaijutsu-types::ids`, so same session
/// id ⇒ same principal, on any process. The two tests compose.
///
/// Read from the SERVER, for the same reason as the test above: the local
/// mirror would happily show a principal that never crossed the wire.
#[test]
fn hook_authored_blocks_belong_to_the_agent_session() {
    run_local(async {
        let addr = start_server().await;
        let mcp = connect_mcp(addr).await;
        let reg = auto_register_with_retry(&mcp, "hook-principal-e2e").await;
        assert!(
            reg.get("success").and_then(|v| v.as_bool()).unwrap_or(false),
            "register_session_auto failed: {reg}"
        );

        let Backend::Remote(remote) = mcp.backend().clone() else {
            panic!("expected remote backend");
        };
        let context_id = remote
            .shared_context_id
            .lock()
            .expect("context id mutex")
            .expect("registered context");

        let listener = Arc::new(HookListener::remote(
            remote.clone(),
            Arc::clone(&remote.shared_context_id),
            Arc::clone(mcp.session_id_arc()),
            None,
        ));
        let socket_path = spawn_listener(listener, "principal").await;

        let session_id = "sess-principal-test-0001";
        let event = serde_json::json!({
            "event": "tool.after",
            "source": "claude-code",
            "session_id": session_id,
            "tool": {
                "name": "Bash",
                "input": {"command": "echo hi"},
                "output": "hi",
            },
        })
        .to_string();
        send_hook_event(&socket_path, &event).await.unwrap();

        let expected = kaijutsu_types::PrincipalId::for_agent_session(session_id);

        let mut server_blocks = Vec::new();
        for _ in 0..50 {
            server_blocks = remote
                .actor
                .get_all_blocks(context_id)
                .await
                .expect("get_all_blocks");
            if server_blocks.iter().any(|b| b.kind == BlockKind::ToolCall) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let call = server_blocks
            .iter()
            .find(|b| b.kind == BlockKind::ToolCall)
            .expect("hook-authored ToolCall must reach the server");

        assert_eq!(
            call.id.principal_id, expected,
            "hook-authored block must belong to the agent session, so the same \
             Claude Code session keeps one identity across MCP relaunches"
        );
        assert_ne!(
            call.id.principal_id,
            kaijutsu_types::PrincipalId::system(),
            "must not claim the kernel authored an agent's tool call"
        );
    });
}

/// RPC authoring, end to end over the real wire — `authorBlock @106` and
/// `completeBlock @107`, migration step 3 of `docs/crdt-position-2026-08.md`.
///
/// This is the surface that replaces client-side replication, so it is
/// tested against the *server's* view: everything asserted below is read
/// back with `get_all_blocks`, never from a local mirror.
///
/// What it pins is exactly what the tempting shortcut drops. Routing tool
/// blocks through the existing `block_create` tool would lose `tool_name`
/// and `tool_input` — that verb parses a `metadata` argument and never reads
/// it — and would hardcode status and ordering, all without erroring. Every
/// assertion here is one of those silent losses made loud.
///
/// It also exercises reserve-then-flow as its own shape: the call is
/// authored `Running` and *stays* pending across a separate round trip
/// before `completeBlock` moves it, which is the model Amy asked for — a
/// quick reservation, then a result that flows when it is ready.
#[test]
fn rpc_authoring_carries_tool_fields_and_completes_independently() {
    run_local(async {
        let addr = start_server().await;
        let mcp = connect_mcp(addr).await;
        let reg = auto_register_with_retry(&mcp, "rpc-authoring-e2e").await;
        assert!(
            reg.get("success").and_then(|v| v.as_bool()).unwrap_or(false),
            "register_session_auto failed: {reg}"
        );

        let Backend::Remote(remote) = mcp.backend().clone() else {
            panic!("expected remote backend");
        };
        let context_id = remote
            .shared_context_id
            .lock()
            .expect("context id mutex")
            .expect("registered context");

        let principal = kaijutsu_types::PrincipalId::for_agent_session("sess-rpc-authoring");

        // Reserve: short call, returns an id, holds nothing.
        let call_id = remote
            .actor
            .author_block(kaijutsu_client::AuthorBlock::tool_call(
                context_id,
                principal,
                "Bash",
                serde_json::json!({"command": "ls -la /tmp"}),
                Some(kaijutsu_types::ToolKind::Mcp),
            ))
            .await
            .expect("authorBlock(tool_call)");

        // Pending is a legitimate state, observable from the server between
        // the reservation and the result. Asserting it here is what makes
        // this a reserve-then-flow test rather than two writes in a trench
        // coat.
        let mid = remote
            .actor
            .get_all_blocks(context_id)
            .await
            .expect("get_all_blocks");
        let pending = mid
            .iter()
            .find(|b| b.id == call_id)
            .expect("the reserved ToolCall must be visible before its result");
        assert_eq!(
            pending.status,
            Status::Running,
            "a reserved call is pending until its result flows — not an orphan"
        );
        assert_eq!(pending.tool_name.as_deref(), Some("Bash"));
        assert!(
            pending
                .tool_input
                .as_deref()
                .is_some_and(|i| i.contains("ls -la /tmp")),
            "tool_input must survive the wire, got: {:?}",
            pending.tool_input
        );

        // Flow: the result arrives later, as its own call, linked to the
        // reservation.
        let result_id = remote
            .actor
            .author_block(kaijutsu_client::AuthorBlock::tool_result(
                context_id,
                principal,
                call_id,
                "total 0",
                false,
                Some(kaijutsu_types::ToolKind::Mcp),
            ))
            .await
            .expect("authorBlock(tool_result)");

        remote
            .actor
            .complete_block(context_id, call_id, Status::Done, false, Some(0))
            .await
            .expect("completeBlock");

        let after = remote
            .actor
            .get_all_blocks(context_id)
            .await
            .expect("get_all_blocks");

        let call = after.iter().find(|b| b.id == call_id).expect("call present");
        assert_eq!(
            call.status,
            Status::Done,
            "completeBlock must move the reservation to its terminal state"
        );
        assert_eq!(
            call.id.principal_id, principal,
            "RPC-authored blocks carry the caller's principal, not the kernel's"
        );

        let result = after
            .iter()
            .find(|b| b.id == result_id)
            .expect("result present");
        assert_eq!(
            result.parent_id.as_ref(),
            Some(&call_id),
            "the result must be linked to its call server-side"
        );
        assert_eq!(result.content, "total 0");

        // `isError` must agree with `status`, and disagreement is REFUSED
        // rather than quietly resolved in favor of one side.
        //
        // This is the assertion that keeps the parameter honest. It is
        // redundant with `status` by construction, which is exactly the shape
        // that rots into a parsed-and-never-read field — the specific defect
        // that made `block_create` unusable for tool blocks and caused this
        // verb to exist. Cross-model review found the server ignoring it;
        // without a test, it would drift straight back to ignored.
        let contradiction = remote
            .actor
            .complete_block(context_id, call_id, Status::Done, true, None)
            .await;
        assert!(
            contradiction.is_err(),
            "completeBlock must refuse isError=true with status=Done, got: {contradiction:?}"
        );
    });
}

/// docs/issues.md "cc-* hook re-registration mints a new context per MCP
/// relaunch": an MCP process relaunch (process death + respawn, `/mcp
/// reconnect`) within the SAME Claude Code session must reattach to the
/// SAME context an earlier process already stabilized — not mint a fresh
/// one under a new launch-timestamp label.
///
/// Simulates two MCP process incarnations for the same repo + Claude Code
/// session: each auto-registers under its own timestamp-suffixed
/// placeholder label (mirrors `main.rs::auto_register_label`), but shares
/// the same stable base (`auto_register_base`) and, crucially, the SAME
/// session id once a hook event reveals it. "Process A" stabilizes first
/// (no prior context under the stable label — a plain rename). "Process B"
/// stabilizes second and must SWITCH onto A's now-stably-labeled context
/// instead of renaming its own placeholder onto a fresh row — proven by:
/// same final context id, the server holding A's pre-relaunch content under
/// that id when read back through B's connection, and B's original
/// placeholder ending up abandoned (not the context anyone is left pointing
/// at).
#[test]
fn relaunch_reattaches_to_the_same_stable_context() {
    run_local(async {
        let addr = start_server().await;
        let base = "cc-relaunch-e2e";

        // -- Process A: first-ever launch for this session --
        let mcp_a = connect_mcp(addr).await;
        let placeholder_a = format!("{base}-0810-1339");
        let reg_a = auto_register_with_retry(&mcp_a, &placeholder_a).await;
        assert!(reg_a["success"].as_bool().unwrap_or(false), "A register failed: {reg_a}");
        let placeholder_a_id =
            kaijutsu_types::ContextId::parse(reg_a["context_id"].as_str().unwrap()).unwrap();

        let Backend::Remote(remote_a) = mcp_a.backend().clone() else {
            panic!("expected remote backend");
        };
        let listener_a = Arc::new(HookListener::remote(
            remote_a.clone(),
            Arc::clone(&remote_a.shared_context_id),
            Arc::clone(mcp_a.session_id_arc()),
            Some(base.to_string()),
        ));
        let socket_a = spawn_listener(listener_a, "relaunch-a").await;

        let session_id = "11112222-3333-4444-5555-666677778888";
        let session_start = serde_json::json!({
            "event": "session.start",
            "source": "claude-code",
            "session_id": session_id,
            "model": "claude-opus-4-8",
        })
        .to_string();
        send_hook_event(&socket_a, &session_start).await.unwrap().unwrap();

        // A's placeholder gets renamed IN PLACE (no prior context under the
        // stable label yet) — same context id, new label.
        let stable_label = format!("{base}-11112222");
        let contexts = remote_a.actor.list_contexts().await.unwrap();
        let stabilized = contexts
            .iter()
            .find(|c| c.label == stable_label)
            .unwrap_or_else(|| panic!("A not stabilized onto {stable_label}: {contexts:?}"));
        assert_eq!(
            stabilized.id, placeholder_a_id,
            "first stabilization must rename A's own context, not create another"
        );

        // Give A some content — this is what "reattach" must actually
        // preserve, not just an id match.
        let prompt_event = serde_json::json!({
            "event": "prompt.submit",
            "source": "claude-code",
            "session_id": session_id,
            "prompt": "hello from process A",
        })
        .to_string();
        send_hook_event(&socket_a, &prompt_event).await.unwrap().unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // -- Process B: a relaunch (new MCP process, same session) --
        let mcp_b = connect_mcp(addr).await;
        let placeholder_b = format!("{base}-0810-1556"); // different launch timestamp
        let reg_b = auto_register_with_retry(&mcp_b, &placeholder_b).await;
        assert!(reg_b["success"].as_bool().unwrap_or(false), "B register failed: {reg_b}");
        let placeholder_b_id =
            kaijutsu_types::ContextId::parse(reg_b["context_id"].as_str().unwrap()).unwrap();
        assert_ne!(
            placeholder_b_id, placeholder_a_id,
            "sanity: B's placeholder must be a distinct fresh context"
        );

        let Backend::Remote(remote_b) = mcp_b.backend().clone() else {
            panic!("expected remote backend");
        };
        let listener_b = Arc::new(HookListener::remote(
            remote_b.clone(),
            Arc::clone(&remote_b.shared_context_id),
            Arc::clone(mcp_b.session_id_arc()),
            Some(base.to_string()),
        ));
        let socket_b = spawn_listener(listener_b, "relaunch-b").await;

        // The relaunch never sees another session.start (Claude Code only
        // fires it at genuine session start) — the next ordinary event
        // (e.g. a tool call) is what must trigger stabilization instead.
        let tool_event = serde_json::json!({
            "event": "tool.after",
            "source": "claude-code",
            "session_id": session_id,
            "tool": {"name": "Bash", "input": {"command": "ls"}, "output": "total 0"},
        })
        .to_string();
        send_hook_event(&socket_b, &tool_event).await.unwrap().unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // B must now be joined to A's stabilized context, not its own
        // placeholder and not a third context.
        let b_context_id = remote_b.shared_context_id.lock().unwrap().unwrap();
        assert_eq!(
            b_context_id, placeholder_a_id,
            "relaunch (process B) must reattach to process A's stabilized context, \
             not stay on its own placeholder or create a new one"
        );

        // The stable label still resolves to exactly one live context (A's) —
        // B's stabilization did not fork a second context under it.
        let resolved = remote_b.actor.resolve_context_label(&stable_label).await.unwrap();
        assert_eq!(resolved.unwrap().id, placeholder_a_id);

        // The context B is now joined to must genuinely hold A's pre-relaunch
        // content — real reattachment, not a same-id coincidence. Read back
        // through B's own connection, straight from the server (there is no
        // local mirror to read instead).
        let blocks = remote_b.actor.get_all_blocks(b_context_id).await.unwrap();
        assert!(
            blocks.iter().any(|b| b.content.contains("hello from process A")),
            "the context B reattached to is missing A's history: {blocks:?}"
        );

        // B's own placeholder context is abandoned, not migrated — it still
        // exists under its original label, distinct from the stable one.
        let contexts = remote_b.actor.list_contexts().await.unwrap();
        assert!(
            contexts.iter().any(|c| c.id == placeholder_b_id && c.label == placeholder_b),
            "B's placeholder should still exist (abandoned, not deleted): {contexts:?}"
        );
    });
}

/// The other half of the same fix: a genuinely NEW Claude Code session in
/// the same repo (different session id) must NOT be folded into an
/// existing session's stabilized context just because the repo-derived
/// label base matches. Same base, different session id ⇒ different stable
/// label ⇒ different context.
#[test]
fn new_session_same_repo_gets_a_distinct_context() {
    run_local(async {
        let addr = start_server().await;
        let base = "cc-newsession-e2e";

        let mcp_a = connect_mcp(addr).await;
        let reg_a = auto_register_with_retry(&mcp_a, &format!("{base}-0810-1000")).await;
        assert!(reg_a["success"].as_bool().unwrap_or(false));
        let Backend::Remote(remote_a) = mcp_a.backend().clone() else { panic!("remote") };
        let listener_a = Arc::new(HookListener::remote(
            remote_a.clone(),
            Arc::clone(&remote_a.shared_context_id),
            Arc::clone(mcp_a.session_id_arc()),
            Some(base.to_string()),
        ));
        let socket_a = spawn_listener(listener_a, "newsess-a").await;
        let event_a = serde_json::json!({
            "event": "session.start",
            "source": "claude-code",
            "session_id": "aaaaaaaa-0000-0000-0000-000000000000",
            "model": "claude-opus-4-8",
        })
        .to_string();
        send_hook_event(&socket_a, &event_a).await.unwrap().unwrap();
        let a_context_id = remote_a.shared_context_id.lock().unwrap().unwrap();

        let mcp_b = connect_mcp(addr).await;
        let reg_b = auto_register_with_retry(&mcp_b, &format!("{base}-0810-1100")).await;
        assert!(reg_b["success"].as_bool().unwrap_or(false));
        let Backend::Remote(remote_b) = mcp_b.backend().clone() else { panic!("remote") };
        let listener_b = Arc::new(HookListener::remote(
            remote_b.clone(),
            Arc::clone(&remote_b.shared_context_id),
            Arc::clone(mcp_b.session_id_arc()),
            Some(base.to_string()),
        ));
        let socket_b = spawn_listener(listener_b, "newsess-b").await;
        let event_b = serde_json::json!({
            "event": "session.start",
            "source": "claude-code",
            "session_id": "bbbbbbbb-0000-0000-0000-000000000000",
            "model": "claude-opus-4-8",
        })
        .to_string();
        send_hook_event(&socket_b, &event_b).await.unwrap().unwrap();
        let b_context_id = remote_b.shared_context_id.lock().unwrap().unwrap();

        assert_ne!(
            a_context_id, b_context_id,
            "distinct Claude Code sessions (different session ids) sharing a repo \
             must not be folded into the same context"
        );

        let contexts = remote_a.actor.list_contexts().await.unwrap();
        assert!(contexts.iter().any(|c| c.label == format!("{base}-aaaaaaaa")));
        assert!(contexts.iter().any(|c| c.label == format!("{base}-bbbbbbbb")));
    });
}
