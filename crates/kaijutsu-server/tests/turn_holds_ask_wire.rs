//! e2e: **a model's turn holds on its own ask.** A gated tool call leaves a
//! durable ask, and the turn that made the call waits for the answer instead
//! of ending at the gate. The approval worker still runs the approved
//! command; the model reads its real output, or the denial, as the tool
//! result in the same turn. `docs/gate-resume.md`, "The turn holds".
//!
//! Driven through real surfaces on a live server: a scripted mock model makes
//! the call inside `kj drive`, and the assigned reviewer answers with
//! `kj ledger allow|deny` from its own authenticated connection.

mod common;
use common::create_context;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use common::run_local;
use kaijutsu_client::{KernelHandle, KeySource, RpcClient, ServerEvent, SshClient, SshConfig, turn_events_channel};
use kaijutsu_kernel::llm::{ContentBlock, Message, MessageContent, MockClient, Provider, stream::StreamEvent};
use kaijutsu_server::{AuthDb, SharedKernel, SshServer, SshServerConfig};
use kaijutsu_types::{BlockKind, ContextId, PrincipalId, Status};
use russh::keys::{Algorithm, PrivateKey};
use tokio::sync::broadcast::Receiver;

/// Poll until `check` holds, or fail: every wait here is on work a
/// background driver does, so a timeout is the bug.
async fn wait_for(label: &str, mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline {
        if check() { return; }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {label}");
}

async fn turn_end(rx: &mut Receiver<ServerEvent>, context: ContextId) -> ServerEvent {
    loop {
        match tokio::time::timeout(Duration::from_secs(20), rx.recv()).await {
            Ok(Ok(ev @ ServerEvent::TurnCompleted { context_id, .. }))
            | Ok(Ok(ev @ ServerEvent::TurnFailed { context_id, .. }))
                if context_id == context => return ev,
            Ok(Ok(_)) => continue,
            Ok(Err(e)) => panic!("turn push channel error: {e}"),
            Err(_) => panic!("timed out waiting for a turn event on {context}"),
        }
    }
}

fn done(stop: &str) -> StreamEvent {
    StreamEvent::Done { stop_reason: Some(stop.into()), input_tokens: None, output_tokens: None, extra: None }
}

fn shell_write(id: &str, command: &str) -> StreamEvent {
    StreamEvent::ToolUse { id: id.into(), name: "shell_write".into(), input: serde_json::json!({ "command": command }) }
}

fn reply(text: &str) -> Vec<StreamEvent> {
    vec![StreamEvent::TextStart, StreamEvent::TextDelta(text.into()), StreamEvent::TextEnd, done("end_turn")]
}

/// The tool results a request carried, in order, as (id, content, is_error).
fn tool_results(request: &[Message]) -> Vec<(String, String, bool)> {
    request.iter().flat_map(|m| match &m.content {
        MessageContent::Blocks(blocks) => blocks.iter().filter_map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, content, is_error } => Some((tool_use_id.clone(), content.clone(), *is_error)),
            _ => None,
        }).collect(),
        _ => Vec::new(),
    }).collect()
}

/// A model performer and its reviewer on distinct connections, and a
/// scratch directory for the commands to leave markers in.
struct Held {
    _performer_client: RpcClient,
    _reviewer_client: RpcClient,
    performer_kj: KernelHandle,
    reviewer_kj: KernelHandle,
    server: SharedKernel,
    context: ContextId,
    reviewer_context: ContextId,
    scratch: tempfile::TempDir,
    _tmp: tempfile::TempDir,
}

impl Held {
    async fn new(label: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let auth_db_path = tmp.path().join("auth.db");
        let performer = PrincipalId::new();
        let reviewer = PrincipalId::new();
        let performer_key = PrivateKey::random(&mut rand_v10::rng(), Algorithm::Ed25519).unwrap();
        let reviewer_key = PrivateKey::random(&mut rand_v10::rng(), Algorithm::Ed25519).unwrap();
        let auth_db = AuthDb::open(&auth_db_path).unwrap();
        auth_db.add_key(performer, performer_key.public_key(), Some("held-performer")).unwrap();
        auth_db.add_key(reviewer, reviewer_key.public_key(), Some("held-reviewer")).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut config = SshServerConfig::ephemeral(addr.port());
        // Setup runs as non-root principals, whose uncovered statements ask;
        // only the model's own calls should reach a reviewer here.
        common::allow_in_shipped_gate(&config, &["kj context create", "kj drive"]);
        config.auth_db_path = Some(auth_db_path);
        common::seed_mock_backend_with_model(config.data_dir.as_ref().unwrap(), "mock-model");
        let (kernel_tx, kernel_rx) = tokio::sync::oneshot::channel();
        tokio::task::spawn_local(async move {
            SshServer::new(config).run_on_listener_with_kernel_sink(listener, kernel_tx).await.unwrap();
        });
        let server = kernel_rx.await.unwrap();
        {
            let db = server.kernel_db.lock();
            for (principal_id, name) in [(performer, "held-performer"), (reviewer, "held-reviewer")] {
                db.insert_character(&kaijutsu_kernel::kernel_db::CharacterRow {
                    principal_id, name: name.into(), created_at: kaijutsu_types::now_millis() as i64,
                    retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
                }).unwrap();
            }
        }
        let connect = |key: PrivateKey| async move {
            let config = SshConfig { host: addr.ip().to_string(), port: addr.port(), username: "held".into(),
                key_source: KeySource::InMemory(Arc::new(key)), insecure: true };
            let mut ssh = SshClient::new(config);
            let channel = ssh.connect().await.unwrap();
            let mut client = RpcClient::new(channel.into_stream()).await.unwrap();
            client.retain_ssh_session(ssh);
            client
        };
        let performer_client = connect(performer_key).await;
        let reviewer_client = connect(reviewer_key).await;
        let (performer_kj, _) = performer_client.bind_kernel().await.unwrap();
        let (reviewer_kj, _) = reviewer_client.bind_kernel().await.unwrap();
        let context = create_context(&performer_kj, label).await.unwrap();
        let reviewer_context = create_context(&reviewer_kj, &format!("{label}-review")).await.unwrap();
        server.kernel_db.lock().update_context_review(context, Some(performer), Some(reviewer)).unwrap();
        performer_kj.join_context(context, label).await.unwrap();
        Self {
            _performer_client: performer_client, _reviewer_client: reviewer_client,
            performer_kj, reviewer_kj, server, context, reviewer_context,
            scratch: tempfile::tempdir().unwrap(), _tmp: tmp,
        }
    }

    fn marker(&self, name: &str) -> PathBuf { self.scratch.path().join(name) }

    /// Script the model, subscribe to turn events, and start one drive.
    async fn drive(&self, script: Vec<Vec<StreamEvent>>) -> (Arc<parking_lot::Mutex<Vec<Vec<Message>>>>, Receiver<ServerEvent>) {
        let (mock, sent) = MockClient::new("").with_scripted_stream(script).recording_sent_messages();
        self.server.kernel.llm().write().await.register("mock", Arc::new(Provider::Mock(mock)));
        let (callback, events) = turn_events_channel(32);
        self.performer_kj.subscribe_turn_events(callback).await.unwrap();
        let admitted = self.performer_kj.execute_kj_quiet(self.context,
            &["drive".into(), "--prompt".into(), "run it".into()]).await.unwrap();
        assert_eq!(admitted.exit_code, 0, "{}", admitted.stderr);
        (sent, events)
    }

    /// The pending asks this context's turn raised, oldest first.
    fn pending(&self) -> Vec<approval_ledger::types::ApprovalRow> {
        self.server.kernel_db.lock().list_pending_asks().unwrap().into_iter()
            .filter(|row| row.context_id == self.context.as_bytes().to_vec()).collect()
    }

    async fn wait_for_asks(&self, count: usize) -> Vec<String> {
        wait_for(&format!("{count} pending ask(s)"), || self.pending().len() >= count).await;
        self.pending().into_iter().map(|row| row.request_id).collect()
    }

    /// Answer from the reviewer's own seat, the way a human does.
    async fn answer(&self, request_id: &str, allow: bool) {
        let verb = if allow { "allow" } else { "deny" };
        self.reviewer_kj.join_context(self.reviewer_context, "held-review").await.unwrap();
        self.reviewer_kj.shell_execute(&format!("kj ledger {verb} {request_id}"), self.reviewer_context, true)
            .await.expect("shell_execute for the answer");
        wait_for("the answer to land in the ledger", || matches!(
            self.server.kernel_db.lock().get_approval(request_id).unwrap(),
            Some(row) if matches!(row.status, kaijutsu_kernel::ApprovalStatus::Allowed | kaijutsu_kernel::ApprovalStatus::Denied)
        )).await;
    }

    fn approval(&self, request_id: &str) -> approval_ledger::types::ApprovalRow {
        self.server.kernel_db.lock().get_approval(request_id).unwrap().expect("the ask row")
    }

    fn blocks(&self) -> Vec<kaijutsu_types::BlockSnapshot> {
        self.server.kernel.blocks().block_snapshots(self.context).unwrap()
    }

    fn result_block(&self, tool_use_id: &str) -> kaijutsu_types::BlockSnapshot {
        self.blocks().into_iter()
            .find(|b| b.kind == BlockKind::ToolResult && b.tool_use_id.as_deref() == Some(tool_use_id))
            .unwrap_or_else(|| panic!("no result block for {tool_use_id}"))
    }

    /// Nothing but the turn itself reports the outcome: no seed, no
    /// follow-up prompt.
    fn assert_no_seed(&self) {
        let seeds: Vec<_> = self.blocks().into_iter()
            .filter(|b| b.kind == BlockKind::Text && b.role != kaijutsu_types::Role::Model
                && (b.content.contains("the action you were waiting on") || b.content.contains("approved and ran")))
            .map(|b| b.content).collect();
        assert!(seeds.is_empty(), "a held turn reads its own result; no seed may be written: {seeds:?}");
    }

    /// Rehydrating the log sends the bytes the turn sent.
    fn assert_rehydrates_as_sent(&self, sent: &[Message], final_text: &str) {
        let mut mailbox = kaijutsu_kernel::ConversationMailbox::new();
        mailbox.catch_up(&self.blocks());
        let mut expected = sent.to_vec();
        expected.push(Message::assistant(final_text));
        let replayed: Vec<serde_json::Value> = mailbox.snapshot().iter().map(|m| serde_json::to_value(m).unwrap()).collect();
        let expected: Vec<serde_json::Value> = expected.iter().map(|m| serde_json::to_value(m).unwrap()).collect();
        if let Some(i) = (0..replayed.len().max(expected.len())).find(|&i| replayed.get(i) != expected.get(i)) {
            panic!("message {i} differs\nreplayed: {:#}\nsent: {:#}",
                replayed.get(i).unwrap_or(&serde_json::Value::Null), expected.get(i).unwrap_or(&serde_json::Value::Null));
        }
    }

    async fn close(self) {
        self.server.kernel.shutdown_runtime_worker().await.unwrap();
    }
}

/// The model asks, the reviewer allows, and the same turn continues with the
/// command's real output as its tool result.
///
/// Falsified by ending the turn at the gate: the first `kj drive` completes
/// with one request sent, and the output arrives later as a seed.
#[test]
fn an_allowed_ask_continues_the_same_turn_with_the_real_output() {
    run_local(async {
        let h = Held::new("held-allow").await;
        let marker = h.marker("allowed");
        let command = format!("echo held-ran | tee -a {}", marker.display());
        let (sent, mut events) = h.drive(vec![
            vec![shell_write("held-1", &command), done("tool_use")],
            reply("saw it"),
        ]).await;

        let asks = h.wait_for_asks(1).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(sent.lock().len(), 1, "the model is not asked again while its call waits");
        assert!(h.server.kernel.turn_in_flight(h.context), "the turn holds while its ask is pending");
        assert!(!marker.exists(), "nothing runs before the answer");

        h.answer(&asks[0], true).await;
        assert!(matches!(turn_end(&mut events, h.context).await, ServerEvent::TurnCompleted { .. }));

        let sent = sent.lock().clone();
        assert_eq!(sent.len(), 2, "one turn, two requests");
        let results = tool_results(&sent[1]);
        assert_eq!(results.len(), 1);
        assert!(results[0].1.contains("held-ran"), "the model reads the real output: {results:?}");
        assert!(!results[0].2);
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "held-ran\n", "the command ran exactly once");
        assert_eq!(h.result_block("held-1").status, Status::Done);
        assert!(h.approval(&asks[0]).status == kaijutsu_kernel::ApprovalStatus::Allowed);
        assert!(!h.server.kernel_db.lock().undelivered_answers().unwrap().iter().any(|a| a.request_id == asks[0]),
            "delivery spent the answer");
        h.assert_no_seed();
        h.assert_rehydrates_as_sent(&sent[1], "saw it");
        h.close().await;
    });
}

/// A denial is the tool result; nothing runs and the model continues.
#[test]
fn a_denied_ask_returns_the_denial_in_the_same_turn() {
    run_local(async {
        let h = Held::new("held-deny").await;
        let marker = h.marker("denied");
        let command = format!("echo must-not-run | tee {}", marker.display());
        let (sent, mut events) = h.drive(vec![
            vec![shell_write("held-2", &command), done("tool_use")],
            reply("understood"),
        ]).await;

        let asks = h.wait_for_asks(1).await;
        h.answer(&asks[0], false).await;
        assert!(matches!(turn_end(&mut events, h.context).await, ServerEvent::TurnCompleted { .. }));

        let sent = sent.lock().clone();
        assert_eq!(sent.len(), 2);
        let results = tool_results(&sent[1]);
        assert!(results[0].2, "a denial is an error result: {results:?}");
        assert!(results[0].1.contains("denied"), "the model reads the denial: {results:?}");
        assert!(!marker.exists());
        assert_eq!(h.result_block("held-2").status, Status::Error);
        h.assert_no_seed();
        h.assert_rehydrates_as_sent(&sent[1], "understood");
        h.close().await;
    });
}

/// Two calls in one response: the turn waits for both answers, whatever
/// their order, and sends both results in call order.
#[test]
fn parallel_calls_wait_for_every_answer() {
    run_local(async {
        let h = Held::new("held-parallel").await;
        let first = h.marker("first");
        let second = h.marker("second");
        let (sent, mut events) = h.drive(vec![
            vec![shell_write("held-a", &format!("echo a | tee {}", first.display())),
                 shell_write("held-b", &format!("echo b | tee {}", second.display())), done("tool_use")],
            reply("both"),
        ]).await;

        let asks = h.wait_for_asks(2).await;
        let ask_for = |needle: &std::path::Path| asks.iter().find(|id| h.approval(id).exec_source.as_deref()
            .is_some_and(|s| s.contains(&needle.display().to_string()))).unwrap().clone();
        let (a, b) = (ask_for(&first), ask_for(&second));
        h.answer(&b, true).await;
        wait_for("the second command to run", || second.exists()).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(sent.lock().len(), 1, "one answer is not enough to continue");
        h.answer(&a, true).await;
        assert!(matches!(turn_end(&mut events, h.context).await, ServerEvent::TurnCompleted { .. }));

        let sent = sent.lock().clone();
        assert_eq!(sent.len(), 2);
        let results = tool_results(&sent[1]);
        let ids: Vec<&str> = results.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(ids, ["held-a", "held-b"], "results keep call order");
        assert!(results[0].1.contains('a') && results[1].1.contains('b'), "{results:?}");
        h.assert_no_seed();
        h.close().await;
    });
}

/// An interrupt ends the hold. The pending ask is abandoned, the pair
/// settles as an error, and a late answer runs nothing.
fn interrupt_ends_the_hold(immediate: bool) {
    run_local(async move {
        let h = Held::new(if immediate { "held-hard" } else { "held-soft" }).await;
        let marker = h.marker("interrupted");
        let (sent, mut events) = h.drive(vec![
            vec![shell_write("held-3", &format!("echo late | tee {}", marker.display())), done("tool_use")],
            reply("unused"),
        ]).await;

        let asks = h.wait_for_asks(1).await;
        let by = PrincipalId::new();
        let outcome = h.server.kernel.interrupt_context(h.context, immediate, by).unwrap();
        assert!(outcome.turn_interrupted);
        turn_end(&mut events, h.context).await;
        assert!(!h.server.kernel.turn_in_flight(h.context));

        assert_eq!(h.approval(&asks[0]).status, kaijutsu_kernel::ApprovalStatus::Abandoned,
            "an interrupted hold abandons its ask");
        assert_eq!(h.result_block("held-3").status, Status::Error);
        let late = h.reviewer_kj.shell_execute(&format!("kj ledger allow {}", asks[0]), h.reviewer_context, true).await;
        drop(late);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!marker.exists(), "an answer after the interrupt runs nothing");
        assert_eq!(sent.lock().len(), 1, "an interrupted hold asks the model nothing more");
        h.assert_no_seed();
        h.close().await;
    });
}

#[test]
fn a_hard_interrupt_ends_the_hold() { interrupt_ends_the_hold(true) }

#[test]
fn a_soft_interrupt_ends_the_hold() { interrupt_ends_the_hold(false) }

/// The wait is outside every cap: a hold longer than the broker's call
/// timeout still continues with the real output.
#[test]
fn a_hold_outlasts_the_tool_call_timeout() {
    run_local(async {
        let h = Held::new("held-long").await;
        h.server.kernel.broker().update_policy(&kaijutsu_kernel::mcp::InstanceId::new("builtin.shell_write"),
            Some(Duration::from_millis(200)), None).await.unwrap();
        let marker = h.marker("long");
        let (sent, mut events) = h.drive(vec![
            vec![shell_write("held-4", &format!("echo patient | tee {}", marker.display())), done("tool_use")],
            reply("done"),
        ]).await;
        let asks = h.wait_for_asks(1).await;
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert!(h.server.kernel.turn_in_flight(h.context), "the hold outlasts the call timeout");
        h.answer(&asks[0], true).await;
        assert!(matches!(turn_end(&mut events, h.context).await, ServerEvent::TurnCompleted { .. }));
        let results = tool_results(&sent.lock()[1]);
        assert!(results[0].1.contains("patient"), "{results:?}");
        h.close().await;
    });
}
