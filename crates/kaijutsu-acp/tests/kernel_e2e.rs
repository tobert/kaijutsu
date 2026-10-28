//! e2e: the whole loop through ACP. A real kernel, set up with the server's
//! bootstrap and a scripted mock model, and the real `kaijutsu-acp` binary
//! connected to it over SSH, driven over stdio the way an ACP client drives
//! it.
//!
//! Each test starts its own kernel on a thread of its own, with the model's
//! replies for that test. The connecting principal is the root character
//! `amy`, who reviews; the sessions are performed by the character `coder`.
//! See `docs/acp.md` and `docs/approval-identity.md`.
//!
//! The client-declared MCP server test launches `mcp-fixture`, which is built
//! only with `--features test-fixtures`:
//!
//! `cargo test -p kaijutsu-acp --features test-fixtures --test kernel_e2e`

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kaijutsu_kernel::llm::stream::StreamEvent;
use kaijutsu_kernel::llm::{MockClient, Provider};
use kaijutsu_server::bootstrap::{BackendChoice, BootstrapOptions, bootstrap};
use kaijutsu_server::{SshServer, SshServerConfig};
use kaijutsu_types::PrincipalId;
use serde_json::{Value, json};

const ACP_BIN: &str = env!("CARGO_BIN_EXE_kaijutsu-acp");

/// The root character: the connection the bridge makes, and the reviewer of
/// every session it opens.
const ROOT: &str = "amy";

/// The character that performs each ACP session.
const PERFORMER: &str = "coder";

/// How long any one response may take.
const REPLY_TIMEOUT: Duration = Duration::from_secs(60);

/// The text `kaijutsu-acp` sends when a session's cwd is `/`.
const ROOT_CWD_WARNING: &str = "warning: this session's cwd is /, which is almost always a mistake; \
                                start the client in your work tree, or `cd DIR` in the shell";

/// A gate with no rules: every statement a model runs through `shell_write`
/// is uncovered, so each one asks its reviewer.
const ASK_EVERYTHING_GATE: &str = "[global]\n";

// ---------------------------------------------------------------------------
// The kernel
// ---------------------------------------------------------------------------

/// The kernel's durable state, which a test reads and seeds directly.
type Db = Arc<parking_lot::Mutex<kaijutsu_kernel::kernel_db::KernelDb>>;

/// A bootstrapped kernel serving on a loopback port, on a thread of its own.
/// Dropping it stops the kernel and removes its state.
struct Kernel {
    port: u16,
    db: Db,
    /// Holds the root key file and the bridge's `HOME`.
    dir: tempfile::TempDir,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Kernel {
    /// Start a kernel whose model answers each completion with the next
    /// entry of `script`.
    fn start(script: Vec<Vec<StreamEvent>>) -> Self {
        let dir = tempfile::tempdir().expect("create the test directory");
        let key_path = dir.path().join("root_key");
        let (ready_tx, ready_rx) = channel::<(u16, Db)>();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build the kernel runtime");
            let _entered = runtime.enter();
            let local = tokio::task::LocalSet::new();
            runtime.block_on(local.run_until(serve_kernel(script, key_path, ready_tx, stop_rx)));
        });
        let (port, db) = ready_rx
            .recv_timeout(Duration::from_secs(60))
            .expect("the kernel never reported its port");
        Self { port, db, dir, stop: Some(stop_tx), thread: Some(thread) }
    }

    fn key_path(&self) -> PathBuf {
        self.dir.path().join("root_key")
    }

    /// Spawn `kaijutsu-acp` against this kernel and run `initialize`.
    fn connect(&self) -> AcpClient {
        let home = self.dir.path().join("home");
        std::fs::create_dir_all(&home).expect("create the bridge's HOME");
        let mut command = Command::new(ACP_BIN);
        command
            .args(["--connect", "--host", "127.0.0.1", "--port"])
            .arg(self.port.to_string())
            .arg("--key-file")
            .arg(self.key_path())
            .args(["--insecure", "--character", PERFORMER, "--user", ROOT])
            .env("HOME", &home)
            .env_remove("KAIJUTSU_KEY_FINGERPRINT")
            .env_remove("KAIJUTSU_KEY_FILE")
            .env("RUST_LOG", "info");
        let mut client = AcpClient::spawn(command);
        let init = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {},
                    "clientInfo": {"name": "kernel-e2e", "version": "0"},
                }),
            )
            .unwrap_or_else(|e| panic!("initialize: {e}"));
        assert_eq!(init.get("protocolVersion").and_then(Value::as_u64), Some(1), "ACP v1: {init}");
        client
    }
}

impl Drop for Kernel {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Bootstrap a kernel, add the performer and the test's gate, script the
/// model, and serve until `stop` fires.
async fn serve_kernel(
    script: Vec<Vec<StreamEvent>>,
    key_path: PathBuf,
    ready: std::sync::mpsc::Sender<(u16, Db)>,
    stop: tokio::sync::oneshot::Receiver<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let config = SshServerConfig::ephemeral_with_root(port, ROOT);
    let opts = BootstrapOptions {
        root: ROOT.to_string(),
        root_key: config.root_key().public_key().clone(),
        key_comment: None,
        backends: vec![BackendChoice::mock("mock-model")],
        console_performer: "root".to_string(),
    };
    bootstrap(&config, &opts).await.expect("bootstrap");

    let gate = config.config_mounts.host_dir(kaijutsu_types::paths::CONFIG_ROOT).join("gate.toml");
    std::fs::create_dir_all(gate.parent().expect("gate.toml has a parent")).expect("create the kernel config dir");
    std::fs::write(&gate, ASK_EVERYTHING_GATE).expect("write gate.toml");

    let openssh = config
        .root_key()
        .to_openssh(russh::keys::ssh_key::LineEnding::LF)
        .expect("serialize the root key");
    std::fs::write(&key_path, openssh.as_bytes()).expect("write the root key");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).expect("restrict the key");
    }

    let (kernel_tx, kernel_rx) = tokio::sync::oneshot::channel();
    tokio::task::spawn_local(async move {
        if let Err(e) = SshServer::new(config).run_on_listener_with_kernel_sink(listener, kernel_tx).await {
            eprintln!("kernel_e2e: server error: {e}");
        }
    });
    let shared = kernel_rx.await.expect("the server hands back its kernel");

    shared
        .kernel_db
        .lock()
        .insert_character(&kaijutsu_kernel::kernel_db::CharacterRow {
            principal_id: PrincipalId::new(),
            name: PERFORMER.into(),
            created_at: kaijutsu_types::now_millis() as i64,
            retired_at: None,
            handoff_ctx: None,
            root_ctx: None,
            root: false,
        })
        .expect("create the performer");
    let mock = MockClient::new("").with_scripted_stream(script);
    shared.kernel.llm().write().await.register("mock", Arc::new(Provider::Mock(mock)));

    ready.send((port, shared.kernel_db.clone())).expect("report the port");
    let _ = stop.await;
    let _ = shared.kernel.shutdown_runtime_worker().await;
}

// ---------------------------------------------------------------------------
// The scripted model
// ---------------------------------------------------------------------------

fn done(stop: &str) -> StreamEvent {
    StreamEvent::Done { stop_reason: Some(stop.into()), input_tokens: None, output_tokens: None, extra: None }
}

/// One completion that calls `shell_write` with `command`.
fn shell_write(id: &str, command: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolUse { id: id.into(), name: "shell_write".into(), input: json!({ "command": command }) },
        done("tool_use"),
    ]
}

/// One completion that says `text` and ends the turn.
fn reply(text: &str) -> Vec<StreamEvent> {
    vec![StreamEvent::TextStart, StreamEvent::TextDelta(text.into()), StreamEvent::TextEnd, done("end_turn")]
}

// ---------------------------------------------------------------------------
// The ACP client
// ---------------------------------------------------------------------------

/// A JSON-RPC error response, as `code` and the whole error object.
#[derive(Debug)]
struct RpcError {
    code: i64,
    error: Value,
    stderr: String,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}\n--- bridge stderr ---\n{}", self.code, self.error, self.stderr)
    }
}

/// `kaijutsu-acp` as a child process, and the client state around it:
/// every `session/update` it sent, every permission request, and the
/// answers queued for the next ones.
struct AcpClient {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    stderr: Arc<Mutex<String>>,
    next_id: i64,
    updates: Vec<Value>,
    /// Each `session/request_permission`'s params, in arrival order.
    permissions: Vec<Value>,
    /// The option kind to select for each next permission request. A
    /// request that finds the queue empty is answered `cancelled`.
    answers: VecDeque<&'static str>,
}

impl AcpClient {
    fn spawn(mut command: Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {ACP_BIN}: {e}"));
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&stderr);
        let pipe = child.stderr.take().expect("piped stderr");
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                let mut held = sink.lock().expect("stderr sink");
                held.push_str(&line);
                held.push('\n');
            }
        });
        let stdin = child.stdin.take().expect("piped stdin");
        Self {
            child,
            stdin,
            lines,
            stderr,
            next_id: 1,
            updates: Vec::new(),
            permissions: Vec::new(),
            answers: VecDeque::new(),
        }
    }

    fn stderr(&self) -> String {
        self.stderr.lock().expect("stderr sink").clone()
    }

    fn send(&mut self, message: &Value) {
        writeln!(self.stdin, "{message}").and_then(|()| self.stdin.flush()).expect("write to the bridge's stdin");
    }

    /// Send a request and wait for its response, answering every request
    /// the bridge sends in the meantime.
    fn request(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let deadline = Instant::now() + REPLY_TIMEOUT;
        loop {
            let message = self.next_message(deadline, method);
            if message.get("method").is_some() {
                self.dispatch(message);
                continue;
            }
            if message.get("id").and_then(Value::as_i64) != Some(id) {
                panic!("a response to a request we did not send: {message}");
            }
            if let Some(error) = message.get("error") {
                return Err(RpcError {
                    code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                    error: error.clone(),
                    stderr: self.stderr(),
                });
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    fn next_message(&mut self, deadline: Instant, waiting_for: &str) -> Value {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match self.lines.recv_timeout(remaining) {
            Ok(line) => serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("the bridge wrote a stdout line that is not JSON ({e}): {line:?}")),
            Err(RecvTimeoutError::Timeout) => {
                panic!("timed out waiting for {waiting_for}\n--- bridge stderr ---\n{}", self.stderr())
            }
            Err(RecvTimeoutError::Disconnected) => {
                panic!("the bridge closed stdout while we waited for {waiting_for}\n--- bridge stderr ---\n{}", self.stderr())
            }
        }
    }

    /// Record a notification, or answer a request the bridge sent.
    fn dispatch(&mut self, message: Value) {
        let method = message.get("method").and_then(Value::as_str).unwrap_or_default().to_string();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = message.get("id").cloned() else {
            if method == "session/update" {
                self.updates.push(params);
            }
            return;
        };
        if method == "session/request_permission" {
            let outcome = match self.answers.pop_front() {
                Some(kind) => {
                    let option = params
                        .get("options")
                        .and_then(Value::as_array)
                        .and_then(|options| {
                            options.iter().find(|o| o.get("kind").and_then(Value::as_str) == Some(kind))
                        })
                        .and_then(|o| o.get("optionId").cloned())
                        .unwrap_or_else(|| panic!("the request offered no {kind} option: {params}"));
                    json!({"outcome": "selected", "optionId": option})
                }
                None => json!({"outcome": "cancelled"}),
            };
            self.permissions.push(params);
            self.send(&json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": outcome}}));
            return;
        }
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": format!("kernel_e2e does not implement {method}")},
        }));
    }

    fn new_session(&mut self, cwd: &Path, mcp_servers: Value) -> Result<String, RpcError> {
        let result = self.request("session/new", json!({"cwd": cwd, "mcpServers": mcp_servers}))?;
        Ok(result
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("session/new returned no sessionId: {result}"))
            .to_string())
    }

    fn open(&mut self, cwd: &Path) -> String {
        self.new_session(cwd, json!([])).unwrap_or_else(|e| panic!("session/new in {}: {e}", cwd.display()))
    }

    /// Send a prompt, answering permission requests with `answers` in
    /// order, and return the prompt's stop reason.
    fn prompt(&mut self, session: &str, text: &str, answers: &[&'static str]) -> String {
        self.answers = answers.iter().copied().collect();
        let response = self
            .request("session/prompt", json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]}))
            .unwrap_or_else(|e| panic!("session/prompt {text:?}: {e}"));
        assert!(self.answers.is_empty(), "queued permission answers were never asked for: {:?}", self.answers);
        response
            .get("stopReason")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("session/prompt returned no stopReason: {response}"))
            .to_string()
    }

    /// Every `agent_message_chunk` text so far, joined.
    fn agent_text(&self) -> String {
        self.updates
            .iter()
            .filter(|u| u.pointer("/update/sessionUpdate").and_then(Value::as_str) == Some("agent_message_chunk"))
            .filter_map(|u| u.pointer("/update/content/text").and_then(Value::as_str))
            .collect()
    }

    /// The last status each `shell_write` tool call reported, in
    /// announcement order.
    fn shell_write_statuses(&self) -> Vec<String> {
        let mut calls: Vec<(String, String)> = Vec::new();
        for params in &self.updates {
            let Some(update) = params.get("update") else { continue };
            let Some(id) = update.get("toolCallId").and_then(Value::as_str) else { continue };
            let status = update.get("status").and_then(Value::as_str);
            match update.get("sessionUpdate").and_then(Value::as_str) {
                Some("tool_call") => {
                    let title = update.get("title").and_then(Value::as_str).unwrap_or_default();
                    if title.contains("shell_write") {
                        calls.push((id.to_string(), status.unwrap_or_default().to_string()));
                    }
                }
                Some("tool_call_update") => {
                    if let (Some(call), Some(status)) = (calls.iter_mut().find(|(seen, _)| seen == id), status) {
                        call.1 = status.to_string();
                    }
                }
                _ => {}
            }
        }
        calls.into_iter().map(|(_, status)| status).collect()
    }
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The text a permission request shows: its tool call's title.
fn permission_title(params: &Value) -> String {
    params.pointer("/toolCall/title").and_then(Value::as_str).unwrap_or_default().to_string()
}

/// A session directory on host disk under `/tmp`, which the kernel mounts
/// read-write, so the model's `mkdir` lands where the test can look.
fn workspace() -> tempfile::TempDir {
    tempfile::Builder::new().prefix("kaijutsu-acp-e2e-").tempdir_in("/tmp").expect("create the session directory")
}

// ---------------------------------------------------------------------------
// Permission round trip
// ---------------------------------------------------------------------------

/// A model's gated `shell_write` asks; the bridge raises
/// `session/request_permission`; the client allows; the held turn runs the
/// command and goes on to its reply.
#[test]
fn an_allowed_ask_runs_the_command_and_the_turn_goes_on() {
    let kernel = Kernel::start(vec![shell_write("w1", "mkdir granted"), reply("granted exists now")]);
    let mut client = kernel.connect();
    let cwd = workspace();
    let session = client.open(cwd.path());

    let stop = client.prompt(&session, "make the granted directory", &["allow_once"]);

    assert_eq!(stop, "end_turn");
    assert_eq!(client.permissions.len(), 1, "one ask: {:?}", client.permissions);
    let title = permission_title(&client.permissions[0]);
    assert!(title.contains("mkdir granted"), "the request names the statement: {title:?}");
    assert!(cwd.path().join("granted").is_dir(), "the allowed command ran");
    assert_eq!(client.shell_write_statuses(), ["completed"]);
    assert!(client.agent_text().contains("granted exists now"), "the same turn went on: {:?}", client.agent_text());
}

/// The same round trip answered deny: the command does not run, the tool
/// call fails, and the held turn still goes on to its reply.
#[test]
fn a_denied_ask_leaves_the_workspace_alone_and_the_turn_goes_on() {
    let kernel = Kernel::start(vec![shell_write("w1", "mkdir refused"), reply("refused was not made")]);
    let mut client = kernel.connect();
    let cwd = workspace();
    let session = client.open(cwd.path());

    let stop = client.prompt(&session, "make the refused directory", &["reject_once"]);

    assert_eq!(stop, "end_turn");
    assert_eq!(client.permissions.len(), 1, "one ask: {:?}", client.permissions);
    assert!(!cwd.path().join("refused").exists(), "a denied command must not run");
    assert_eq!(client.shell_write_statuses(), ["failed"]);
    assert!(client.agent_text().contains("refused was not made"), "the same turn went on: {:?}", client.agent_text());
}

/// "Always allow" runs the command and remembers a rule: the next identical
/// statement runs with no permission request.
#[test]
fn an_always_allow_answer_lets_the_same_statement_run_without_asking() {
    let kernel = Kernel::start(vec![
        shell_write("w1", "mkdir -p made"),
        reply("ran once"),
        shell_write("w2", "mkdir -p made"),
        reply("ran twice"),
    ]);
    let mut client = kernel.connect();
    let cwd = workspace();
    let made = cwd.path().join("made");
    let session = client.open(cwd.path());

    assert_eq!(client.prompt(&session, "make the made directory", &["allow_always"]), "end_turn");
    assert_eq!(client.permissions.len(), 1, "the first statement asks: {:?}", client.permissions);
    assert!(made.is_dir(), "the always-allowed command ran");

    std::fs::remove_dir(&made).expect("clear the directory between prompts");
    assert_eq!(client.prompt(&session, "make it again", &[]), "end_turn");
    assert_eq!(
        client.permissions.len(),
        1,
        "the remembered rule covers the identical statement: {:?}",
        client.permissions
    );
    assert!(made.is_dir(), "the remembered statement ran again");
    assert_eq!(client.shell_write_statuses(), ["completed", "completed"]);
    assert!(client.agent_text().contains("ran twice"), "{:?}", client.agent_text());
}

// ---------------------------------------------------------------------------
// Chat
// ---------------------------------------------------------------------------

/// A plain prompt streams the model's text to the client as agent chunks
/// and ends with `end_turn`.
#[test]
fn a_chat_prompt_streams_agent_text_and_ends_the_turn() {
    let kernel = Kernel::start(vec![reply("mock speaking through acp")]);
    let mut client = kernel.connect();
    let cwd = workspace();
    let session = client.open(cwd.path());

    assert_eq!(client.prompt(&session, "say something", &[]), "end_turn");
    assert!(
        client.agent_text().contains("mock speaking through acp"),
        "the model's text reached the client: {:?}",
        client.agent_text()
    );
    assert!(client.permissions.is_empty(), "a chat asks nothing: {:?}", client.permissions);
}

// ---------------------------------------------------------------------------
// Session cwd
// ---------------------------------------------------------------------------

/// A cwd the kernel host does not have is `invalid_params` naming the cwd,
/// and a later session in a real directory still opens.
#[test]
fn a_session_cwd_missing_on_the_kernel_host_is_invalid_params() {
    let kernel = Kernel::start(Vec::new());
    let mut client = kernel.connect();
    let cwd = workspace();
    let missing = cwd.path().join("not-here");

    let error = client.new_session(&missing, json!([])).expect_err("a missing cwd must refuse session/new");
    assert_eq!(error.code, -32602, "invalid_params, not internal: {error}");
    assert!(error.error.to_string().contains(missing.to_str().expect("utf-8 path")), "the error names the cwd: {error}");

    client.open(cwd.path());
}

/// A session at `/` opens, and the client is told the cwd is almost always
/// a mistake. A session in a work tree is not.
#[test]
fn a_session_at_the_root_opens_with_a_warning() {
    let kernel = Kernel::start(Vec::new());
    let mut client = kernel.connect();
    let cwd = workspace();

    client.open(cwd.path());
    assert!(!client.agent_text().contains(ROOT_CWD_WARNING), "a work tree is not warned about: {:?}", client.agent_text());

    client.open(Path::new("/"));
    assert!(
        client.agent_text().contains(ROOT_CWD_WARNING),
        "the client is told; saw {:?}\n--- bridge stderr ---\n{}",
        client.agent_text(),
        client.stderr()
    );
}

// ---------------------------------------------------------------------------
// Session delete
// ---------------------------------------------------------------------------

/// `session/delete` archives the session's context through `kj context
/// archive`, so the context type's archive rc lifecycle runs: a coder's
/// judge shadow is archived with its seat. A retried delete succeeds.
#[test]
fn deleting_a_session_archives_its_context_and_runs_its_archive_lifecycle() {
    let kernel = Kernel::start(Vec::new());
    let mut client = kernel.connect();
    let cwd = workspace();
    let session = client.open(cwd.path());
    let seat = kaijutsu_types::ContextId::parse(&session).expect("a session id is a context id");

    let shadow = kaijutsu_types::ContextId::new();
    {
        let db = kernel.db.lock();
        let mut row = db.get_context(seat).expect("read the seat").expect("the seat exists");
        assert_eq!(row.context_type, "coder", "the bridge creates coder seats");
        row.context_id = shadow;
        row.label = Some("judge-shadow".into());
        row.context_type = "judge".into();
        let workspace = db.get_or_create_default_workspace(row.created_by).expect("default workspace");
        db.insert_context_with_document(&row, workspace).expect("insert the shadow");
        db.insert_edge(&kaijutsu_kernel::kernel_db::ContextEdgeRow {
            edge_id: uuid::Uuid::now_v7(),
            source_id: seat,
            target_id: shadow,
            kind: kaijutsu_types::EdgeKind::Structural,
            metadata: None,
            created_at: kaijutsu_types::now_millis() as i64,
        })
        .expect("seat the shadow under the session");
    }

    client
        .request("session/delete", json!({"sessionId": session}))
        .unwrap_or_else(|e| panic!("session/delete: {e}\n--- bridge stderr ---\n{}", client.stderr()));

    client
        .request("session/delete", json!({"sessionId": session}))
        .unwrap_or_else(|e| panic!("a retried session/delete succeeds: {e}"));

    let db = kernel.db.lock();
    let archived = |id| db.get_context(id).expect("read").expect("rows are retained").archived_at.is_some();
    assert!(archived(seat), "session/delete archives the session's context");
    assert!(archived(shadow), "the coder's archive rc lifecycle archived its judge shadow");
}

// ---------------------------------------------------------------------------
// Client-declared MCP servers
// ---------------------------------------------------------------------------

#[cfg(feature = "test-fixtures")]
const MCP_FIXTURE: &str = env!("CARGO_BIN_EXE_mcp-fixture");

/// Wait for the fixture to write its pidfile, and return the pid.
#[cfg(feature = "test-fixtures")]
fn wait_for_pid(pidfile: &Path) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(text) = std::fs::read_to_string(pidfile)
            && let Ok(pid) = text.trim().parse()
        {
            return pid;
        }
        assert!(Instant::now() < deadline, "the fixture never wrote {}", pidfile.display());
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// True while `pid` names a process that has not exited. A zombie has
/// exited; only its parent's reaping is left.
#[cfg(feature = "test-fixtures")]
fn process_alive(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => {
            let state = stat.rsplit(')').next().and_then(|rest| rest.split_whitespace().next());
            state != Some("Z") && state != Some("X")
        }
        Err(_) => false,
    }
}

/// `session/resume` and `session/load` with the server declarations the
/// session opened with keep the running server process: no second process
/// starts, and the first one keeps running.
#[test]
#[cfg_attr(
    not(feature = "test-fixtures"),
    ignore = "needs `--features test-fixtures`, which builds the mcp-fixture server it declares"
)]
fn resuming_with_the_same_servers_keeps_the_running_process() {
    #[cfg(feature = "test-fixtures")]
    {
        let kernel = Kernel::start(Vec::new());
        let mut client = kernel.connect();
        let cwd = workspace();
        let pidfile = cwd.path().join("fixture.pid");
        let servers = json!([{
            "name": "fixture",
            "command": MCP_FIXTURE,
            "args": [],
            "env": [{"name": "MCP_FIXTURE_PIDFILE", "value": pidfile}],
        }]);

        let session = client
            .new_session(cwd.path(), servers.clone())
            .unwrap_or_else(|e| panic!("session/new with the fixture: {e}"));
        let pid = wait_for_pid(&pidfile);
        assert!(process_alive(pid), "the fixture runs once session/new returns");
        std::fs::remove_file(&pidfile).expect("clear the pidfile");

        client
            .request("session/resume", json!({"sessionId": session, "cwd": cwd.path(), "mcpServers": servers}))
            .unwrap_or_else(|e| panic!("session/resume: {e}"));
        client
            .request("session/load", json!({"sessionId": session, "cwd": cwd.path(), "mcpServers": servers}))
            .unwrap_or_else(|e| panic!("session/load: {e}"));

        std::thread::sleep(Duration::from_millis(500));
        assert!(!pidfile.exists(), "a second fixture process started on resume or load");
        assert!(process_alive(pid), "the original fixture process stopped on resume or load");
    }
}
