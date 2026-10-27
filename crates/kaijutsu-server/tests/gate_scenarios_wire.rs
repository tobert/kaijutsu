//! e2e: **the gate's approval properties, through a model's own calls.** A
//! scripted mock model makes shell calls inside `kj drive`; the gate reads
//! `gate.toml` at each submission, pre_call hooks run, and the assigned
//! reviewer answers asks with `kj ledger` from its own connection.
//! `docs/gate-policy-tuning.md` and `docs/gate-resume.md`, "The turn holds".

mod common;
use common::create_context;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::run_local;
use kaijutsu_client::{KernelHandle, KeySource, RpcClient, ServerEvent, SshClient, SshConfig, turn_events_channel};
use kaijutsu_kernel::ApprovalStatus;
use kaijutsu_kernel::llm::{ContentBlock, Message, MessageContent, MockClient, Provider, stream::StreamEvent};
use kaijutsu_kernel::mcp::{GlobPattern, HookAction, HookBody, HookEntry, HookId};
use kaijutsu_server::{AuthDb, SharedKernel, SshServer, SshServerConfig};
use kaijutsu_types::{ContextId, PrincipalId, Status};
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

fn call(id: &str, tool: &str, input: serde_json::Value) -> StreamEvent {
    StreamEvent::ToolUse { id: id.into(), name: tool.into(), input }
}

fn shell_write(id: &str, command: &str) -> StreamEvent {
    call(id, "shell_write", serde_json::json!({ "command": command }))
}

/// One model response that makes `calls` in parallel.
fn calls(calls: Vec<StreamEvent>) -> Vec<StreamEvent> {
    calls.into_iter().chain([done("tool_use")]).collect()
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

/// The result of call `id` in the last request the turn sent.
fn result_of(sent: &[Vec<Message>], id: &str) -> (String, bool) {
    let results = tool_results(sent.last().expect("a request"));
    let (_, content, is_error) = results.iter().find(|r| r.0 == id)
        .unwrap_or_else(|| panic!("no result for {id}: {results:?}"));
    (content.clone(), *is_error)
}

/// A `gate.toml` whose global allow tier holds the setup verbs and `allow`,
/// followed by `rest` in the same section.
fn gate(allow: &[&str], rest: &str) -> String {
    let keys: Vec<String> = ["kj context create", "kj drive", "cd"].iter().chain(allow).map(|k| format!("{k:?}")).collect();
    format!("[global]\nallow = [{}]\n{rest}\n", keys.join(", "))
}

/// A model performer and its reviewer on distinct connections, and a
/// workspace directory that is the context's cwd.
struct Seat {
    _performer_client: RpcClient,
    _reviewer_client: RpcClient,
    reviewer_kj: KernelHandle,
    performer_kj: KernelHandle,
    server: SharedKernel,
    context: ContextId,
    reviewer_context: ContextId,
    gate_path: PathBuf,
    workspace: tempfile::TempDir,
    _tmp: tempfile::TempDir,
}

impl Seat {
    async fn new(label: &str, gate_toml: &str, host_exec: bool) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let auth_db_path = tmp.path().join("auth.db");
        let performer = PrincipalId::new();
        let reviewer = PrincipalId::new();
        let performer_key = PrivateKey::random(&mut rand_v10::rng(), Algorithm::Ed25519).unwrap();
        let reviewer_key = PrivateKey::random(&mut rand_v10::rng(), Algorithm::Ed25519).unwrap();
        let auth_db = AuthDb::open(&auth_db_path).unwrap();
        auth_db.add_key(performer, performer_key.public_key(), Some("seat-performer")).unwrap();
        auth_db.add_key(reviewer, reviewer_key.public_key(), Some("seat-reviewer")).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut config = SshServerConfig::ephemeral(addr.port());
        if host_exec { config = config.with_host_exec(); }
        let gate_path = config.config_mounts.host_dir(kaijutsu_types::paths::CONFIG_ROOT).join("gate.toml");
        std::fs::create_dir_all(gate_path.parent().unwrap()).unwrap();
        std::fs::write(&gate_path, gate_toml).unwrap();
        config.auth_db_path = Some(auth_db_path);
        common::bootstrap_mock(&config, "mock-model").await;
        let (kernel_tx, kernel_rx) = tokio::sync::oneshot::channel();
        tokio::task::spawn_local(async move {
            SshServer::new(config).run_on_listener_with_kernel_sink(listener, kernel_tx).await.unwrap();
        });
        let server = kernel_rx.await.unwrap();
        {
            let db = server.kernel_db.lock();
            for (principal_id, name) in [(performer, "seat-performer"), (reviewer, "seat-reviewer")] {
                db.insert_character(&kaijutsu_kernel::kernel_db::CharacterRow {
                    principal_id, name: name.into(), created_at: kaijutsu_types::now_millis() as i64,
                    retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
                }).unwrap();
            }
        }
        let connect = |key: PrivateKey| async move {
            let config = SshConfig { host: addr.ip().to_string(), port: addr.port(), username: "seat".into(),
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
        let workspace = tempfile::tempdir().unwrap();
        let cd = common::shell_exec_wait(&performer_kj, &format!("cd {}", workspace.path().display()), context).await;
        assert_eq!(cd.2, Status::Done, "cd: {}", cd.1);
        Self {
            _performer_client: performer_client, _reviewer_client: reviewer_client,
            performer_kj, reviewer_kj, server, context, reviewer_context, gate_path, workspace, _tmp: tmp,
        }
    }

    fn path(&self, name: &str) -> PathBuf { self.workspace.path().join(name) }

    fn set_gate(&self, gate_toml: &str) { std::fs::write(&self.gate_path, gate_toml).unwrap(); }

    /// A pre_call hook on every `shell_write` call in this context that runs
    /// `body` as kaish.
    async fn hook_shell_write(&self, id: &str, body: &str) {
        self.server.kernel.broker().hooks().write().await.pre_call.entries.push(HookEntry {
            id: HookId(id.into()),
            match_instance: None,
            match_tool: Some(GlobPattern("shell_write".into())),
            match_context: Some(self.context),
            match_principal: None,
            action: HookAction::Invoke(HookBody::Kaish(body.into())),
            priority: 0,
            kaish_script_id: None,
        });
    }

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

    /// Drive `script` to a completed turn and return the requests it sent.
    /// An ask fails the test: these turns are expected to run without one.
    async fn drive_to_end(&self, script: Vec<Vec<StreamEvent>>) -> Vec<Vec<Message>> {
        let (sent, mut events) = self.drive(script).await;
        let first_ask = async {
            loop {
                if let Some(row) = self.asks().into_iter().next() { return row; }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let end = tokio::select! {
            end = turn_end(&mut events, self.context) => end,
            ask = first_ask => panic!("the turn held on an unexpected ask: {}", ask.description),
        };
        assert!(matches!(end, ServerEvent::TurnCompleted { .. }), "{end:?}");
        sent.lock().clone()
    }

    /// The asks this context's turns raised, in any status, oldest first.
    fn asks(&self) -> Vec<approval_ledger::types::ApprovalRow> {
        let db = self.server.kernel_db.lock();
        let mut rows: Vec<_> = db.list_pending_asks().unwrap().into_iter()
            .filter(|row| row.context_id == self.context.as_bytes().to_vec()).collect();
        rows.sort_by_key(|row| row.created_at);
        rows
    }

    async fn wait_for_asks(&self, count: usize) -> Vec<String> {
        wait_for(&format!("{count} pending ask(s)"), || self.asks().len() >= count).await;
        self.asks().into_iter().map(|row| row.request_id).collect()
    }

    /// Answer with `verb` and any flags, e.g. `deny --remember always`, from
    /// the reviewer's own seat, and wait for the ledger to take it up.
    async fn answer_with(&self, request_id: &str, verb: &str) {
        self.reviewer_kj.join_context(self.reviewer_context, "seat-review").await.unwrap();
        let (verb, flags) = verb.split_once(' ').unwrap_or((verb, ""));
        self.reviewer_kj.shell_execute(&format!("kj ledger {verb} {request_id} {flags}"), self.reviewer_context, true)
            .await.expect("shell_execute for the answer");
        wait_for("the answer to land in the ledger", || matches!(
            self.server.kernel_db.lock().get_approval(request_id).unwrap(),
            Some(row) if matches!(row.status, ApprovalStatus::Allowed | ApprovalStatus::Denied)
        )).await;
    }

    fn approval(&self, request_id: &str) -> approval_ledger::types::ApprovalRow {
        self.server.kernel_db.lock().get_approval(request_id).unwrap().expect("the ask row")
    }

    async fn close(self) {
        self.server.kernel.shutdown_runtime_worker().await.unwrap();
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// A deny rule remembered while a second identical ask is pending leaves
/// that ask alone: the policy in force when the command was evaluated covers
/// its approval, so the later allow runs it. The next identical statement
/// meets the rule and is refused with no ask.
#[test]
fn a_rule_remembered_after_an_ask_leaves_that_ask_alone() {
    run_local(async {
        let h = Seat::new("rule-after-ask", &gate(&[], ""), false).await;
        let (sent, mut events) = h.drive(vec![
            calls(vec![shell_write("late-1", "mkdir late"), shell_write("late-2", "mkdir late")]),
            reply("one refused, one ran"),
        ]).await;
        let asks = h.wait_for_asks(2).await;
        h.answer_with(&asks[0], "deny --remember always").await;
        assert_eq!(h.approval(&asks[1]).status, ApprovalStatus::Pending, "the new rule does not decide the pending ask");
        h.answer_with(&asks[1], "allow").await;
        assert!(matches!(turn_end(&mut events, h.context).await, ServerEvent::TurnCompleted { .. }));
        assert_eq!(h.approval(&asks[1]).status, ApprovalStatus::Allowed);
        let sent = sent.lock().clone();
        let errors: Vec<bool> = ["late-1", "late-2"].iter().map(|id| result_of(&sent, id).1).collect();
        assert_eq!(errors.iter().filter(|e| **e).count(), 1, "one refused, one ran: {:?}", tool_results(&sent[1]));
        assert!(h.path("late").is_dir(), "the approved command ran");

        let sent = h.drive_to_end(vec![calls(vec![shell_write("late-3", "mkdir late")]), reply("the rule refused it")]).await;
        let (content, is_error) = result_of(&sent, "late-3");
        assert!(is_error && content.contains("user rule denies the exact statement"), "{content}");
        assert!(h.asks().is_empty(), "the remembered rule refuses with no ask");
        h.close().await;
    });
}

/// A learned exact allow outranks a config deny added to `gate.toml` later.
/// The edit takes effect at the next submission, with no reload: a different
/// statement it covers is refused in the same turn.
#[test]
fn a_learned_allow_beats_a_config_deny_added_later() {
    run_local(async {
        let h = Seat::new("learned-beats-deny", &gate(&[], ""), false).await;
        let (_, mut events) = h.drive(vec![
            calls(vec![shell_write("run-1", "echo ran >> runs.txt")]),
            reply("noted"),
        ]).await;
        let asks = h.wait_for_asks(1).await;
        h.answer_with(&asks[0], "allow --remember always").await;
        assert!(matches!(turn_end(&mut events, h.context).await, ServerEvent::TurnCompleted { .. }));
        assert_eq!(read(&h.path("runs.txt")), "ran\n");

        h.set_gate(&gate(&[], "deny = [\"echo\"]"));
        let sent = h.drive_to_end(vec![
            calls(vec![shell_write("run-2", "echo ran >> runs.txt"), shell_write("other", "echo other >> other.txt")]),
            reply("ran it"),
        ]).await;
        let (content, is_error) = result_of(&sent, "run-2");
        assert!(!is_error, "the learned allow wins over the config deny: {content}");
        let (content, is_error) = result_of(&sent, "other");
        assert!(is_error && content.contains("global config denies echo"), "the edited gate.toml is in force: {content}");
        assert!(h.asks().is_empty(), "neither call asks");
        assert_eq!(read(&h.path("runs.txt")), "ran\nran\n");
        assert!(!h.path("other.txt").exists());
        h.close().await;
    });
}

/// An approved `shell_write` ask runs in the cwd and with the env it showed,
/// though a sibling call in the same reply changed both before the answer.
/// Later calls in the turn see the changed state.
#[test]
fn an_approved_shell_write_runs_with_the_cwd_and_env_it_showed() {
    run_local(async {
        let h = Seat::new("runs-what-was-shown", &gate(&["mkdir", "export", "sleep"], ""), false).await;
        let (_, mut events) = h.drive(vec![
            calls(vec![shell_write("d-1", "mkdir sub")]),
            calls(vec![shell_write("d-2", "cd sub")]),
            calls(vec![shell_write("d-3", "export SHOWN=shown-value")]),
            // The sibling waits until the ask exists, then leaves sub,
            // changes the variable, and proves both with `d-drifted`.
            calls(vec![
                shell_write("d-asks", "echo $SHOWN > out.txt"),
                shell_write("d-drift", "while ! [[ -f ../asked ]]; do sleep 0.05; done; cd ..; export SHOWN=drifted; mkdir \"d-$SHOWN\""),
            ]),
            calls(vec![shell_write("d-after", "mkdir \"after-$SHOWN\"")]),
            reply("wrote it where it was shown"),
        ]).await;
        let asks = h.wait_for_asks(1).await;
        let ask = h.approval(&asks[0]);
        assert!(ask.description.contains("NOTE: at ask time SHOWN=\"shown-value\""), "{}", ask.description);
        assert_eq!(ask.cwd.as_deref(), Some(h.path("sub").to_str().unwrap()));
        std::fs::write(h.path("asked"), "").unwrap();
        wait_for("the sibling call to change cwd and env", || h.path("d-drifted").is_dir()).await;
        h.answer_with(&asks[0], "allow").await;
        assert!(matches!(turn_end(&mut events, h.context).await, ServerEvent::TurnCompleted { .. }));
        assert_eq!(read(&h.path("sub/out.txt")), "shown-value\n", "the command ran where and with what it showed");
        assert!(!h.path("out.txt").exists());
        assert!(h.path("after-drifted").is_dir(), "the context's live state is the sibling's");
        h.close().await;
    });
}

/// A pre_call hook that exits neither 0 nor 3 fails the call closed: no ask,
/// nothing runs, and the error names the hook.
#[test]
fn a_failing_pre_call_hook_denies_with_no_ask() {
    run_local(async {
        let h = Seat::new("hook-fails", &gate(&[], ""), false).await;
        h.hook_shell_write("seat-hook", "no-such-command-in-the-seat").await;
        let sent = h.drive_to_end(vec![calls(vec![shell_write("crash", "mkdir crashed")]), reply("refused")]).await;
        let (content, is_error) = result_of(&sent, "crash");
        assert!(is_error && content.contains("seat-hook"), "{content}");
        assert!(h.asks().is_empty(), "a failed hook denies; it does not ask");
        assert!(!h.path("crashed").exists());
        h.close().await;
    });
}

/// A pre_call hook that exits 0 does not lower the gate's uncovered ask; an
/// allow then runs the command.
#[test]
fn a_proceeding_pre_call_hook_leaves_the_gate_ask() {
    run_local(async {
        let h = Seat::new("hook-proceeds", &gate(&[], ""), false).await;
        let saw = h.path("hook-saw.txt");
        h.hook_shell_write("seat-hook",
            &format!("echo $KJ_TOOL_ARGS | jq -r '.command' >> {}\nexit 0", saw.display())).await;
        let (sent, mut events) = h.drive(vec![calls(vec![shell_write("benign", "mkdir benign")]), reply("made it")]).await;
        let asks = h.wait_for_asks(1).await;
        let ask = h.approval(&asks[0]);
        assert_eq!(ask.hook_id, None, "the ask is the gate's, not the hook's");
        assert!(ask.description.contains("mkdir benign"), "{}", ask.description);
        assert!(!h.path("benign").exists(), "nothing runs before the answer");
        h.answer_with(&asks[0], "allow").await;
        assert!(matches!(turn_end(&mut events, h.context).await, ServerEvent::TurnCompleted { .. }));
        assert!(!result_of(&sent.lock(), "benign").1);
        assert!(h.path("benign").is_dir());
        assert!(read(&saw).contains("mkdir benign"), "the hook ran");
        h.close().await;
    });
}

/// A config deny refuses a read-only `shell` call with no ask.
#[test]
fn a_config_deny_refuses_a_read_only_shell_call() {
    run_local(async {
        let h = Seat::new("deny-read-only-shell", &gate(&[], "deny = [\"cat\"]"), false).await;
        std::fs::write(h.path("seed.txt"), "seed\n").unwrap();
        let sent = h.drive_to_end(vec![
            calls(vec![call("cat", "shell", serde_json::json!({ "command": "cat seed.txt" }))]),
            reply("refused"),
        ]).await;
        let (content, is_error) = result_of(&sent, "cat");
        assert!(is_error && content.contains("global config denies cat"), "{content}");
        assert!(!content.contains("seed\n"), "{content}");
        assert!(h.asks().is_empty(), "a config deny does not ask");
        h.close().await;
    });
}

/// An ask the reviewer denies is in the reviewer's ledger history. This is
/// the control for the config-deny case below: the same listing, read the
/// same way, does show a denied row.
#[test]
fn a_denied_ask_is_listed_in_the_ledger_history() {
    run_local(async {
        let h = Seat::new("denied-history", &gate(&[], ""), false).await;
        let (_sent, mut events) = h.drive(vec![
            calls(vec![shell_write("made", "mkdir refused")]),
            reply("refused"),
        ]).await;
        let asks = h.wait_for_asks(1).await;
        h.answer_with(&asks[0], "deny").await;
        assert!(matches!(turn_end(&mut events, h.context).await, ServerEvent::TurnCompleted { .. }));

        let (_, listing, status) =
            common::shell_exec_wait(&h.reviewer_kj, "kj ledger list --status denied", h.reviewer_context).await;
        assert_eq!(status, Status::Done, "{listing}");
        assert!(listing.contains("mkdir refused"), "the denied ask is in the history:\n{listing}");
        h.close().await;
    });
}

/// A config deny is a decision, so the ledger's history shows it: the
/// reviewer's `kj ledger list --status denied` names the refused statement.
#[test]
#[ignore = "F8 in docs/issues.md: a PreCall deny writes no ledger row"]
fn a_config_deny_is_listed_in_the_ledger_history() {
    run_local(async {
        let h = Seat::new("deny-history", &gate(&[], "deny = [\"cat\"]"), false).await;
        std::fs::write(h.path("seed.txt"), "seed\n").unwrap();
        let sent = h.drive_to_end(vec![
            calls(vec![call("cat", "shell", serde_json::json!({ "command": "cat seed.txt" }))]),
            reply("refused"),
        ]).await;
        let (content, is_error) = result_of(&sent, "cat");
        assert!(is_error && content.contains("global config denies cat"), "{content}");

        let (_, listing, status) =
            common::shell_exec_wait(&h.reviewer_kj, "kj ledger list --status denied", h.reviewer_context).await;
        assert_eq!(status, Status::Done, "{listing}");
        assert!(listing.contains("cat seed.txt"), "the denied statement is in the history:\n{listing}");
        h.close().await;
    });
}

/// With host exec allowed and an uncovered tier that allows, a model runs
/// real git through `shell_write` and its `write` tool with no asks.
#[test]
fn a_contained_model_commits_with_real_git() {
    run_local(async {
        let h = Seat::new("git-commit", &gate(&[], "uncovered = \"allow\""), true).await;
        let git = "git -c user.name=seat -c user.email=seat@example.invalid -c commit.gpgsign=false";
        let sent = h.drive_to_end(vec![
            calls(vec![shell_write("init", "git init -q")]),
            calls(vec![call("write", "write", serde_json::json!({ "path": "hello.txt", "content": "hello from the seat\n" }))]),
            calls(vec![shell_write("add", "git add hello.txt")]),
            calls(vec![shell_write("commit", &format!("{git} commit -q -m 'seat: first commit'"))]),
            reply("committed"),
        ]).await;
        assert_eq!(sent.len(), 5);
        for (request, id) in sent[1..].iter().zip(["init", "write", "add", "commit"]) {
            let (content, is_error) = result_of(std::slice::from_ref(request), id);
            assert!(!is_error, "{id}: {content}");
        }
        assert!(h.asks().is_empty(), "the uncovered tier allows; nothing asks");
        assert_eq!(read(&h.path("hello.txt")), "hello from the seat\n");
        let git_out = |args: &[&str]| {
            let out = std::process::Command::new("git").args(args).current_dir(h.workspace.path()).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8(out.stdout).unwrap()
        };
        assert_eq!(git_out(&["log", "--format=%s"]), "seat: first commit\n");
        assert_eq!(git_out(&["show", "HEAD:hello.txt"]), "hello from the seat\n");
        assert_eq!(git_out(&["status", "--porcelain"]), "");
        h.close().await;
    });
}
