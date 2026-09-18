//! e2e: identity invariants around a **human's** input — the compose draft
//! and the interactive shell. Amy: *"the draft should never have a path for
//! the model to reach it"* (`docs/issues.md`, "The compose draft is the
//! player's alone") and *"are all the gate sites using the right
//! identifiers to check who it is?"* (`docs/issues.md`, "Identity
//! audit"). `docs/approval-identity.md`, "Three identities" is the contract:
//! `principal_id` is the authenticated requester, `actor_id` the performer,
//! `reviewer_id` the effective reviewer — and `user_initiated` "controls
//! presentation, never approval authority".
//!
//! Every test here drives a real SSH + Cap'n Proto round trip, no stubs.
//!
//!   * the draft is owned by the connection, never by anything the request
//!     carries (`rpc.rs`, `edit_input`/`get_input_state`/`submit_input`);
//!   * `submitInput` authors the user block as that same connection
//!     principal and starts an `Interactive` turn;
//!   * a human's own typed shell command is gated exactly like a model's
//!     tool call once a hook asks for one, and `user_initiated` only
//!     changes how the command is displayed (`Role::User`, excluded by
//!     default) — never whether it runs;
//!   * a human with nobody responsible above her confirms her own gated
//!     command, and the approval executes it;
//!   * a model-bound credential that is neither the ask's actor nor its
//!     reviewer cannot answer it either.

mod common;
use common::{create_context};

use std::sync::Arc;
use std::time::Duration;

use common::{connect_client, run_local, seed_mock_backend_with_model, start_server_with_kernel_handle};
use kaijutsu_client::{
    KernelHandle, RpcClient, RpcError, ServerEvent, SshClient, SshConfig, TurnOrigin,
    turn_events_channel,
};
use kaijutsu_client::{KeySource};
use kaijutsu_kernel::kernel_db::CharacterRow;
use kaijutsu_kernel::mcp::{AskSpec, GlobPattern, HookAction, HookEntry, HookId};
use kaijutsu_server::{AuthDb, SharedKernel, SshServer, SshServerConfig};
use kaijutsu_types::{BlockKind, BlockQuery, BlockSnapshot, ContextId, PrincipalId, Role, Status};
use russh::keys::{Algorithm, PrivateKey};

/// Poll until `check` returns true, or fail loudly. A timeout here IS the
/// bug — every wait is on work a background driver does.
async fn wait_for(label: &str, mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {label}");
}

/// Every block in the context, document order, read straight off the
/// server's own store — stable and fast, the way `gate_executes_wire.rs`
/// reads its worker's blocks.
fn context_blocks(server: &SharedKernel, ctx: ContextId) -> Vec<BlockSnapshot> {
    server.documents.block_snapshots(ctx).unwrap()
}

/// Every block in the context, read over the wire.
async fn blocks(kernel: &KernelHandle, ctx: ContextId) -> Vec<BlockSnapshot> {
    kernel.get_blocks(ctx, &BlockQuery::All).await.expect("get_blocks")
}

/// Connect over SSH with an in-memory key, retaining the SSH session so the
/// connection survives for the life of the returned client.
async fn connect_with_key(addr: std::net::SocketAddr, key: PrivateKey, username: &str) -> RpcClient {
    let config = SshConfig {
        host: addr.ip().to_string(),
        port: addr.port(),
        username: username.to_string(),
        key_source: KeySource::InMemory(Arc::new(key)),
        insecure: true,
    };
    let mut ssh = SshClient::new(config);
    let channel = ssh.connect().await.unwrap();
    let mut client = RpcClient::new(channel.into_stream()).await.unwrap();
    client.retain_ssh_session(ssh);
    client
}

fn character(principal_id: PrincipalId, name: &str) -> CharacterRow {
    CharacterRow {
        principal_id,
        name: name.to_string(),
        created_at: kaijutsu_types::now_millis() as i64,
        retired_at: None,
        handoff_ctx: None, root_ctx: None,
        root: false,
    }
}

/// Install a `PreCall` hook that turns every `shell_write` in `ctx` into a
/// pending ask, the way the lfm2d scorer does for a seat whose shell is
/// watched — `gate_executes_wire.rs`'s `install_ask_hook_on_worker`, scoped
/// by a caller-chosen id so two installs in one test don't collide.
async fn install_ask_hook(server: &SharedKernel, ctx: ContextId, id: &str) {
    server
        .kernel
        .broker()
        .hooks()
        .write()
        .await
        .pre_call
        .entries
        .push(HookEntry {
            id: HookId(id.to_string()),
            match_instance: None,
            match_tool: Some(GlobPattern("shell_write".into())),
            match_context: Some(ctx),
            match_principal: None,
            action: HookAction::Ask(AskSpec {
                description: Some("wire test: a human's own shell is gated too".into()),
            }),
            priority: 0,
            kaish_script_id: None,
        });
}

// ===========================================================================
// 1 + 2: the draft, and what submitting it does
// ===========================================================================

/// Two independently authenticated connections and their kernel handles,
/// sharing one server. Bare credentials — no character sheets — because
/// `edit_input`/`get_input_state`/`submit_input` never resolve one; only
/// the authenticated principal on the connection matters
/// (`docs/approval-identity.md`, "Three identities").
struct TwoConnections {
    _a_client: RpcClient,
    _b_client: RpcClient,
    a: KernelHandle,
    b: KernelHandle,
    a_principal: PrincipalId,
    b_principal: PrincipalId,
    server: SharedKernel,
}

/// `mock_llm` seeds a mock backend so a test that submits a chat prompt can
/// drive a turn; the draft-only tests don't need one.
async fn two_connections(mock_llm: bool) -> TwoConnections {
    let tmp = tempfile::tempdir().unwrap();
    let auth_db_path = tmp.path().join("auth.db");
    let a_principal = PrincipalId::new();
    let b_principal = PrincipalId::new();
    let a_key = PrivateKey::random(&mut rand_v10::rng(), Algorithm::Ed25519).unwrap();
    let b_key = PrivateKey::random(&mut rand_v10::rng(), Algorithm::Ed25519).unwrap();
    let auth_db = AuthDb::open(&auth_db_path).unwrap();
    auth_db.add_key(a_principal, a_key.public_key(), Some("conn-a")).unwrap();
    auth_db.add_key(b_principal, b_key.public_key(), Some("conn-b")).unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = SshServerConfig::ephemeral(addr.port());
    config.auth_db_path = Some(auth_db_path);
    if mock_llm && let Some(ref data_dir) = config.data_dir {
        seed_mock_backend_with_model(data_dir, "mock-model");
    }
    let (kernel_tx, kernel_rx) = tokio::sync::oneshot::channel();
    tokio::task::spawn_local(async move {
        SshServer::new(config)
            .run_on_listener_with_kernel_sink(listener, kernel_tx)
            .await
            .unwrap();
    });
    let server = kernel_rx.await.unwrap();

    let a_client = connect_with_key(addr, a_key, "conn-a").await;
    let b_client = connect_with_key(addr, b_key, "conn-b").await;
    let (a, _) = a_client.bind_kernel().await.unwrap();
    let (b, _) = b_client.bind_kernel().await.unwrap();

    TwoConnections {
        _a_client: a_client,
        _b_client: b_client,
        a,
        b,
        a_principal,
        b_principal,
        server,
    }
}

/// **1. The draft belongs to the connection.** Two authenticated
/// connections join the same context. A's `editInput` can only ever reach
/// A's own draft, so B's `getInputState` shows B's own (empty) draft, not
/// A's text — and B's `submitInput` submits B's draft, not A's, leaving
/// A's untouched.
///
/// Falsified by a mutation that takes the principal for these three RPCs
/// from anything but the authenticated connection.
#[test]
fn the_draft_belongs_to_the_connection_not_the_request() {
    run_local(async {
        let conns = two_connections(true).await;
        let ctx = create_context(&conns.a, "shared-draft").await.unwrap();
        // B's submit below is a chat prompt, which starts a turn and so
        // needs a resolvable performer/reviewer — irrelevant to what this
        // test pins, but required for the submit to succeed at all.
        seed_turn_identity(&conns.server, ctx);
        conns.a.join_context(ctx, "a").await.unwrap();
        conns.b.join_context(ctx, "b").await.unwrap();

        conns
            .a
            .edit_input(ctx, 0, "A's private text", 0)
            .await
            .unwrap();

        // B's own read must be B's own (empty) draft, never A's — B never
        // typed anything yet.
        let b_state = conns.b.get_input_state(ctx).await.unwrap();
        assert_eq!(
            b_state.content, "",
            "B's getInputState must show B's own draft, not A's text"
        );

        conns.b.edit_input(ctx, 0, "B's text", 0).await.unwrap();
        let b_result = conns.b.submit_input(ctx, false).await.unwrap();

        let all = blocks(&conns.a, ctx).await;
        let b_submitted = all
            .iter()
            .find(|b| b.id == b_result.block_id)
            .expect("B's submitted block exists");
        assert_eq!(b_submitted.content, "B's text");
        assert_eq!(
            b_submitted.id.principal_id, conns.b_principal,
            "the submitted block is authored by B's connection, not A's"
        );

        let a_draft = all
            .iter()
            .find(|b| b.status == Status::Draft)
            .expect("A's draft still exists after B's submit");
        assert_eq!(
            a_draft.content, "A's private text",
            "B's submit must never touch A's draft"
        );
        assert_eq!(a_draft.id.principal_id, conns.a_principal);
    });
}

/// Drain the turn push channel until a terminal event for `ctx` arrives —
/// `turn_events_wire.rs`'s pattern. A timeout is the bug, not a skip.
async fn recv_turn_event(
    rx: &mut tokio::sync::broadcast::Receiver<ServerEvent>,
    ctx: ContextId,
) -> ServerEvent {
    loop {
        match tokio::time::timeout(Duration::from_secs(15), rx.recv()).await {
            Ok(Ok(ev @ ServerEvent::TurnCompleted { context_id, .. }))
            | Ok(Ok(ev @ ServerEvent::TurnFailed { context_id, .. }))
                if context_id == ctx =>
            {
                return ev;
            }
            Ok(Ok(_)) => continue,
            Ok(Err(e)) => panic!("turn push channel error: {e}"),
            Err(_) => panic!("timed out waiting for a turn event on {ctx}"),
        }
    }
}

/// A performer + reviewer assignment good enough for a model turn to start
/// — wire context creation leaves the performer unset, so a submit test
/// that wants a turn arranges this first (`common::seed_turn_identity`'s
/// pattern, duplicated here as a private helper per this lane's territory).
fn seed_turn_identity(server: &SharedKernel, ctx: ContextId) {
    let performer = PrincipalId::new();
    let reviewer = PrincipalId::new();
    let db = server.kernel_db.lock();
    db.insert_character(&character(performer, "submit-performer")).unwrap();
    db.insert_character(&character(reviewer, "submit-reviewer")).unwrap();
    db.update_context_review(ctx, Some(performer), Some(reviewer)).unwrap();
}

/// **2. `submitInput` authors the user block as the connection principal,
/// and the turn it starts is `Interactive`.** The id on the promoted block
/// is A's connection principal (never B's, never a system identity), and
/// the pushed `TurnCompleted`/`TurnFailed` names `TurnOrigin::Interactive`
/// — the origin that keeps a submit out of the musician's autonomous act
/// while still reaching an ACP frontend waiting on it.
#[test]
fn submit_input_authors_the_connection_principal_and_starts_an_interactive_turn() {
    run_local(async {
        let conns = two_connections(true).await;
        let ctx = create_context(&conns.a, "submit-identity").await.unwrap();
        seed_turn_identity(&conns.server, ctx);
        conns.a.join_context(ctx, "a").await.unwrap();

        let (callback, mut rx) = turn_events_channel(64);
        conns.a.subscribe_turn_events(callback).await.unwrap();

        conns.a.edit_input(ctx, 0, "hello from A", 0).await.unwrap();
        let result = conns.a.submit_input(ctx, false).await.unwrap();

        let all = blocks(&conns.a, ctx).await;
        let user_block = all
            .iter()
            .find(|b| b.id == result.block_id)
            .expect("the submitted block exists");
        assert_eq!(user_block.role, Role::User);
        assert_eq!(
            user_block.id.principal_id, conns.a_principal,
            "the user block's author is the connection that submitted it"
        );

        let origin = match recv_turn_event(&mut rx, ctx).await {
            ServerEvent::TurnCompleted { origin, .. } => origin,
            ServerEvent::TurnFailed { origin, .. } => origin,
            other => panic!("expected a terminal turn event, got {other:?}"),
        };
        assert_eq!(
            origin,
            TurnOrigin::Interactive,
            "a submitInput turn is interactive, and it still announces"
        );
    });
}

// ===========================================================================
// 3: a human's own shell command is gated like a model's
// ===========================================================================

/// **3. `user_initiated` is presentation only.** With an `Ask` hook
/// installed, a human's own typed `shell_execute` — `user_initiated: true`
/// — is refused pending exactly like a model's `shell_write` tool call: the
/// command block is `Role::User` and excluded from hydration by default
/// (docs/gate-and-shell-split.md's split, and the shell submit design), but
/// none of that grants it authority to run. The ask stays `Pending` until
/// someone answers it.
#[test]
fn a_humans_own_shell_command_is_gated_like_a_models_and_user_initiated_grants_no_authority() {
    run_local(async {
        let (addr, server) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let principal = client.whoami().await.unwrap().principal_id;
        let (kernel, _) = client.bind_kernel().await.unwrap();
        let ctx = create_context(&kernel, "human-shell-gate").await.unwrap();
        kernel.join_context(ctx, "human").await.unwrap();
        install_ask_hook(&server, ctx, "wire-user-initiated-ask").await;

        let code = "echo should-not-run-yet";
        let refusal = match kernel.shell_execute(code, ctx, true).await {
            Err(RpcError::Refused(r)) => r,
            other => panic!("a gated human shell command must refuse with a Refusal, got {other:?}"),
        };
        assert!(
            refusal.is_pending(),
            "user_initiated must not bypass the gate: {refusal:?}"
        );
        let ask_id = refusal.ask_id().expect("a pending refusal names its ask").to_string();

        let all = context_blocks(&server, ctx);
        let command = all
            .iter()
            .find(|b| b.kind == BlockKind::ToolCall && b.status == Status::Waiting)
            .expect("the gated command block, left Waiting on the ask");
        assert_eq!(
            command.role,
            Role::User,
            "a user_initiated command is authored with Role::User, the same as any human message"
        );
        assert_eq!(command.id.principal_id, principal);
        assert!(
            command.excluded,
            "a user-initiated command is excluded from hydration by default"
        );

        let pending = server
            .kernel_db
            .lock()
            .get_approval(&ask_id)
            .unwrap()
            .expect("the ask row");
        assert_eq!(
            pending.status,
            kaijutsu_kernel::ApprovalStatus::Pending,
            "user_initiated must not have settled its own ask"
        );
    });
}

// ===========================================================================
// 4 + 5: who may answer a gated ask
// ===========================================================================

/// One connected credential, sheeted as a character of the given name, plus
/// the live server. No context is created yet.
struct OneCredential {
    _client: RpcClient,
    kj: KernelHandle,
    kj_principal: PrincipalId,
    server: SharedKernel,
}

/// A kernel whose only root character is `name`, connected as that root.
async fn one_credential(name: &str) -> OneCredential {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = SshServerConfig::ephemeral_with_root(addr.port(), name);
    let key = (*config.root_key()).clone();
    let principal = kaijutsu_kernel::KernelDb::open(config.data_dir.as_ref().unwrap().join("kernel.db"))
        .unwrap()
        .get_character_by_name(name)
        .unwrap()
        .expect("the ephemeral root character")
        .principal_id;
    let (kernel_tx, kernel_rx) = tokio::sync::oneshot::channel();
    tokio::task::spawn_local(async move {
        SshServer::new(config)
            .run_on_listener_with_kernel_sink(listener, kernel_tx)
            .await
            .unwrap();
    });
    let server = kernel_rx.await.unwrap();

    let client = connect_with_key(addr, key, name).await;
    let (kj, _) = client.bind_kernel().await.unwrap();
    OneCredential { _client: client, kj, kj_principal: principal, server }
}

/// Find the block right after `command_block_id` — its `ToolResult` pair,
/// the way `execute_shell_command` always authors them adjacently.
fn output_after(server: &SharedKernel, ctx: ContextId, command_block_id: &kaijutsu_types::BlockId) -> BlockSnapshot {
    let all = context_blocks(server, ctx);
    let at = all
        .iter()
        .position(|b| &b.id == command_block_id)
        .expect("the command block exists");
    all.get(at + 1)
        .cloned()
        .expect("a ToolResult block immediately follows its ToolCall")
}

/// **4. A root's own gated command is a self-confirmation.** amy is the
/// kernel's root character; her context sits under her own root context,
/// which she plays, so nobody else is above her when she runs a gated
/// shell command. Every resolution layer is exhausted and amy is a root,
/// so the ask is raised with amy as its own reviewer, amy alone
/// may answer it, and her answer EXECUTES the command
/// (`docs/approval-identity.md`).
///
/// She answers from a second, hook-free context: the blanket `Ask` hook
/// this test installs would otherwise re-gate the `kj ledger allow` typed
/// into the same context (`docs/issues.md`, "Identity audit").
///
/// Before the walk, this same scenario left an `actor_id == reviewer_id`
/// row pending forever, and then briefly refused in-band instead of
/// raising one at all. Both are gone: an ask nobody can answer cannot
/// exist.
#[test]
fn a_root_humans_own_gated_command_is_a_self_confirmation_she_can_answer() {
    run_local(async {
        let amy = one_credential("amy").await;
        let amy_principal = amy.kj_principal;
        let work = create_context(&amy.kj, "amy-self-review-work").await.unwrap();
        let answering = create_context(&amy.kj, "amy-answering").await.unwrap();
        amy.kj.join_context(work, "amy").await.unwrap();
        install_ask_hook(&amy.server, work, "wire-amy-self-review").await;

        let refusal = match amy.kj.shell_execute("echo self-confirmed-for-real", work, true).await {
            Err(RpcError::Refused(r)) => r,
            other => panic!("expected a pending refusal, got {other:?}"),
        };
        assert!(refusal.is_pending(), "a self-confirmation is an open question, not a refusal: {refusal:?}");
        let ask_id = refusal.ask_id().expect("a pending refusal names its ask").to_string();

        let row = amy.server.kernel_db.lock().get_approval(&ask_id).unwrap().expect("the ask row");
        assert_eq!(row.actor_id.as_deref(), Some(amy_principal.as_bytes().as_slice()));
        assert_eq!(
            row.reviewer_id.as_deref(),
            Some(amy_principal.as_bytes().as_slice()),
            "with every layer exhausted the ask names its own actor as reviewer",
        );
        let output_block_id = row
            .output_block_id
            .as_deref()
            .and_then(kaijutsu_types::BlockId::from_key)
            .expect("the gated command left a linked output block");

        amy.kj.join_context(answering, "amy").await.unwrap();
        amy.kj
            .shell_execute(&format!("kj ledger allow {ask_id}"), answering, true)
            .await
            .expect("shell_execute for the answer");
        wait_for("amy's own answer to land in the ledger", || {
            matches!(
                amy.server.kernel_db.lock().get_approval(&ask_id).unwrap(),
                Some(row) if row.status == kaijutsu_kernel::ApprovalStatus::Allowed
            )
        })
        .await;

        // The approval EXECUTES: the ask's own linked output block fills
        // in with the command's real stdout.
        wait_for("the self-confirmed statement to actually execute", || {
            amy.server
                .documents
                .get_block_snapshot(work, &output_block_id)
                .ok()
                .flatten()
                .map(|b| b.status == Status::Done && b.content.contains("self-confirmed-for-real"))
                .unwrap_or(false)
        })
        .await;
    });
}

/// **5. A model credential cannot answer as the human.** `worker` performs
/// a gated command in its own context, explicitly reviewed by `amy`
/// (a character sheet only — no live connection needed for this half).
/// `coder` — a distinct connected credential, the performer of its own,
/// unrelated context — is neither the ask's actor nor its reviewer.
/// `coder`'s `kj ledger allow` on `worker`'s ask is refused
/// `unauthorized_reviewer`.
///
/// `gate_executes_wire.rs` drives the same worker/reviewer split but only
/// ever exercises the happy path (the assigned reviewer answering); it has
/// no coverage of a wrong-identity answer attempt, self-approval or
/// otherwise — this and the previous test are net-new coverage, not an
/// overlap with it.
#[test]
fn a_model_credential_that_is_neither_actor_nor_reviewer_cannot_answer() {
    run_local(async {
        let tmp = tempfile::tempdir().unwrap();
        let auth_db_path = tmp.path().join("auth.db");
        let worker_principal = PrincipalId::new();
        let coder_principal = PrincipalId::new();
        let amy_principal = PrincipalId::new();
        let worker_key = PrivateKey::random(&mut rand_v10::rng(), Algorithm::Ed25519).unwrap();
        let coder_key = PrivateKey::random(&mut rand_v10::rng(), Algorithm::Ed25519).unwrap();
        let auth_db = AuthDb::open(&auth_db_path).unwrap();
        auth_db.add_key(worker_principal, worker_key.public_key(), Some("worker")).unwrap();
        auth_db.add_key(coder_principal, coder_key.public_key(), Some("coder")).unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut config = SshServerConfig::ephemeral(addr.port());
        config.auth_db_path = Some(auth_db_path);
        let (kernel_tx, kernel_rx) = tokio::sync::oneshot::channel();
        tokio::task::spawn_local(async move {
            SshServer::new(config)
                .run_on_listener_with_kernel_sink(listener, kernel_tx)
                .await
                .unwrap();
        });
        let server = kernel_rx.await.unwrap();
        {
            let db = server.kernel_db.lock();
            db.insert_character(&character(worker_principal, "worker")).unwrap();
            db.insert_character(&character(coder_principal, "coder")).unwrap();
            db.insert_character(&character(amy_principal, "amy")).unwrap();
        }

        let worker_client = connect_with_key(addr, worker_key, "worker").await;
        let coder_client = connect_with_key(addr, coder_key, "coder").await;
        let (worker_kj, _) = worker_client.bind_kernel().await.unwrap();
        let (coder_kj, _) = coder_client.bind_kernel().await.unwrap();

        let worker_ctx = create_context(&worker_kj, "coder-cannot-answer-worker").await.unwrap();
        let coder_ctx = create_context(&coder_kj, "coder-cannot-answer-coder").await.unwrap();
        server
            .kernel_db
            .lock()
            .update_context_review(worker_ctx, Some(worker_principal), Some(amy_principal))
            .unwrap();
        worker_kj.join_context(worker_ctx, "worker").await.unwrap();
        coder_kj.join_context(coder_ctx, "coder").await.unwrap();

        // The retained shell invokes the native shell_write tool, whose gate
        // records the authenticated identity on its pending ask.
        let submission = worker_kj
            .shell_submit("shell_write --command 'echo should-not-run' --foreground", worker_ctx, true)
            .await
            .unwrap();
        assert!(!submission.operation_id.is_empty(), "the retained shell submission needs an operation id");
        wait_for("the worker's pending shell ask", || {
            server
                .kernel_db
                .lock()
                .list_pending_asks()
                .unwrap()
                .iter()
                .any(|r| r.exec_source.as_deref() == Some("echo should-not-run"))
        })
        .await;
        let ask_id = server
            .kernel_db
            .lock()
            .list_pending_asks()
            .unwrap()
            .into_iter()
            .find(|r| r.exec_source.as_deref() == Some("echo should-not-run"))
            .expect("the worker's pending ask")
            .request_id;

        let row = server.kernel_db.lock().get_approval(&ask_id).unwrap().expect("the ask row");
        assert_eq!(row.actor_id.as_deref(), Some(worker_principal.as_bytes().as_slice()));
        assert_eq!(row.reviewer_id.as_deref(), Some(amy_principal.as_bytes().as_slice()));

        let command_block_id = coder_kj
            .shell_execute(&format!("kj ledger allow {ask_id}"), coder_ctx, true)
            .await
            .expect("the shell_execute call itself succeeds; the kj command is what fails");

        wait_for("coder's unauthorized answer attempt to settle", || {
            output_after(&server, coder_ctx, &command_block_id).status != Status::Running
        })
        .await;

        let output = output_after(&server, coder_ctx, &command_block_id);
        assert_eq!(output.status, Status::Error, "coder must not be able to answer worker's ask");
        let stderr = output.stderr.clone().unwrap_or_default();
        assert!(
            stderr.contains("may only be answered by its assigned reviewer"),
            "expected the unauthorized_reviewer refusal text, got: {stderr:?}"
        );

        let row_after = server.kernel_db.lock().get_approval(&ask_id).unwrap().unwrap();
        assert_eq!(
            row_after.status,
            kaijutsu_kernel::ApprovalStatus::Pending,
            "the ask is left pending — coder could not decide it"
        );
    });
}
