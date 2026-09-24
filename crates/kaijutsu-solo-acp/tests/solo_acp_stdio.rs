//! e2e: drive the `kaijutsu-solo-acp` binary as an ACP client would.
//!
//! Every test here spawns the real binary, which starts a real kernel in
//! process and serves ACP v1 on its stdin/stdout. The model is the scripted
//! mock backend (`KJ_MOCK_SCRIPT_DIR`, one script file per model name), so a
//! turn runs end to end with no provider and no spend.
//!
//! Framing is newline-delimited JSON-RPC 2.0 — what the
//! `agent-client-protocol` crate's `Stdio` transport reads and writes
//! (`stdio.rs`, `BufReader::new(stdin).lines()`). The client side is
//! hand-rolled here rather than taken from that crate: the tests need to
//! assert on raw notification shapes and on what does NOT appear on stdout,
//! which a typed client hides.
//!
//! Scratch state lives under `/home/atobey/src/bench-work/solo/` — a real
//! disk, and under `$HOME/src`, which is the kernel's read-write VFS mount
//! (`crates/kaijutsu-server/src/rpc.rs`, the `$HOME/src` mount). A session
//! cwd outside that mount is read-only to the model, so the file-writing
//! test would fail for a reason that has nothing to do with this binary.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The binary under test, built by cargo for this test target.
const BIN: &str = env!("CARGO_BIN_EXE_kaijutsu-solo-acp");

/// Where scratch state goes. Real disk: `/tmp` on this host is a small
/// tmpfs and a kernel database does not belong there.
const SCRATCH: &str = "/home/atobey/src/bench-work/solo";

/// A boot plus a first turn crosses an rc lifecycle and a model turn; 120s
/// is generous enough that a timeout means something is actually wedged.
const REPLY_TIMEOUT: Duration = Duration::from_secs(120);

/// A fresh directory under [`SCRATCH`], unique per call.
fn scratch_dir(label: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    let path = PathBuf::from(SCRATCH).join(format!("{label}-{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&path).expect("create the scratch directory");
    path
}

/// One scripted model, by the directory holding `<model>.json`.
fn mock_scripts(case: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/mock_scripts")
        .join(case)
}


/// A running `kaijutsu-solo-acp`, with its stdout and stderr drained by
/// reader threads so neither pipe can fill and deadlock the child.
struct Agent {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    stderr: Arc<Mutex<String>>,
    /// Bytes the agent has written to stdout. stdout is the ACP wire, so a
    /// run that was never asked anything must leave this at zero.
    seen_stdout: Arc<AtomicUsize>,
    next_id: i64,
    /// `session/update` notifications, in arrival order.
    updates: Vec<Value>,
    /// How many `session/request_permission` requests we answered.
    permissions: usize,
}

impl Agent {
    /// Spawn the binary with the mock backend and a scripted model.
    fn spawn_mock(case: &str) -> Self {
        Self::spawn_mock_with(case, &[])
    }

    /// Spawn the binary with the mock backend, plus extra flags.
    fn spawn_mock_with(case: &str, extra: &[&str]) -> Self {
        Self::build(case, None, extra)
    }

    /// Spawn the binary from a chosen launch directory, which is what the
    /// default workspace mount is about.
    fn spawn_mock_from(case: &str, cwd: &Path, extra: &[&str]) -> Self {
        Self::build(case, Some(cwd), extra)
    }

    fn build(case: &str, cwd: Option<&Path>, extra: &[&str]) -> Self {
        let mut command = Command::new(BIN);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        command
            .arg("--backend-kind")
            .arg("mock")
            .arg("--model")
            .arg("solo-mock")
            .args(extra)
            .env("KJ_MOCK_SCRIPT_DIR", mock_scripts(case))
            // The default state directory is a temp dir; keep it off the
            // host's small /tmp tmpfs.
            .env("TMPDIR", scratch_dir("tmp"))
            .env("RUST_LOG", "info");
        Self::spawn(command)
    }

    fn spawn(mut command: Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {BIN}: {e}"));

        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, lines) = channel();
        let seen_stdout = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&seen_stdout);
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        counter.fetch_add(line.len() + 1, Ordering::SeqCst);
                        if tx.send(line).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        });

        let stderr_pipe = child.stderr.take().expect("piped stderr");
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&stderr);
        std::thread::spawn(move || {
            for line in BufReader::new(stderr_pipe).lines().map_while(Result::ok) {
                let mut held = sink.lock().expect("stderr sink poisoned");
                held.push_str(&line);
                held.push('\n');
            }
        });

        Self {
            stdin: child.stdin.take(),
            child,
            lines,
            stderr,
            seen_stdout,
            next_id: 1,
            updates: Vec::new(),
            permissions: 0,
        }
    }

    fn stderr(&self) -> String {
        self.stderr.lock().expect("stderr sink poisoned").clone()
    }

    fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin still open");
        writeln!(stdin, "{message}").expect("write a request");
        stdin.flush().expect("flush a request");
    }

    /// Send a request and return its result, answering anything the agent
    /// asks us in the meantime.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));

        let deadline = Instant::now() + REPLY_TIMEOUT;
        loop {
            let message = self.next_message(deadline, method);
            if message.get("id").and_then(Value::as_i64) == Some(id)
                && message.get("method").is_none()
            {
                if let Some(error) = message.get("error") {
                    panic!("{method} failed: {error}\n--- stderr ---\n{}", self.stderr());
                }
                return message.get("result").cloned().unwrap_or(Value::Null);
            }
            self.dispatch(message);
        }
    }

    /// Read one JSON message, failing loudly on a timeout or a dead child.
    fn next_message(&mut self, deadline: Instant, waiting_for: &str) -> Value {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match self.lines.recv_timeout(remaining) {
            Ok(line) => serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("stdout line is not JSON: {line:?} ({e})")),
            Err(RecvTimeoutError::Timeout) => panic!(
                "timed out waiting for {waiting_for}\n--- stderr ---\n{}",
                self.stderr()
            ),
            Err(RecvTimeoutError::Disconnected) => panic!(
                "the agent closed stdout while we waited for {waiting_for}\n--- stderr ---\n{}",
                self.stderr()
            ),
        }
    }

    /// Record a notification, or answer a request the agent sent us. An
    /// unanswered request would wedge the turn, so every method gets a
    /// reply — an error reply for anything this client does not implement.
    fn dispatch(&mut self, message: Value) {
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            // A response to a request we are no longer waiting for.
            return;
        };
        let method = method.to_string();
        let Some(id) = message.get("id").cloned() else {
            if method == "session/update" {
                self.updates
                    .push(message.get("params").cloned().unwrap_or(Value::Null));
            }
            return;
        };
        if method == "session/request_permission" {
            self.permissions += 1;
            let option = message
                .pointer("/params/options/0/optionId")
                .and_then(Value::as_str)
                .unwrap_or("allow")
                .to_string();
            self.send(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"outcome": {"outcome": "selected", "optionId": option}},
            }));
            return;
        }
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": format!("test client does not implement {method}")},
        }));
    }

    fn initialize(&mut self) -> Value {
        self.request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {},
                "clientInfo": {"name": "solo-acp-test", "version": "0"},
            }),
        )
    }

    fn new_session(&mut self, cwd: &Path) -> String {
        let result = self.request("session/new", json!({"cwd": cwd, "mcpServers": []}));
        result
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("session/new returned no sessionId: {result}"))
            .to_string()
    }

    fn prompt(&mut self, session: &str, text: &str) -> Value {
        self.request(
            "session/prompt",
            json!({
                "sessionId": session,
                "prompt": [{"type": "text", "text": text}],
            }),
        )
    }

    /// Every `agent_message_chunk` text seen so far, joined.
    fn agent_text(&self) -> String {
        self.updates
            .iter()
            .filter(|params| {
                params.pointer("/update/sessionUpdate").and_then(Value::as_str)
                    == Some("agent_message_chunk")
            })
            .filter_map(|params| {
                params
                    .pointer("/update/content/text")
                    .and_then(Value::as_str)
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// Close stdin and wait for the process to exit, the way an ACP client
    /// shutting down does.
    fn close_stdin_and_wait(&mut self, within: Duration) -> std::process::ExitStatus {
        drop(self.stdin.take());
        self.wait_for_exit("stdin EOF", within)
    }

    /// Send a signal to the running agent, the way a service manager or a
    /// Ctrl-C does.
    fn signal(&self, signal: i32) {
        let pid = self.child.id() as i32;
        // SAFETY: `kill(2)` on a child this process spawned and has not
        // reaped; an invalid pid would only return an error we ignore.
        let sent = unsafe { libc::kill(pid, signal) };
        assert_eq!(sent, 0, "kill({pid}, {signal}) failed");
    }

    fn wait_for_exit(&mut self, after: &str, within: Duration) -> std::process::ExitStatus {
        let deadline = Instant::now() + within;
        loop {
            match self.child.try_wait().expect("poll the child") {
                Some(status) => return status,
                None if Instant::now() > deadline => {
                    let _ = self.child.kill();
                    panic!(
                        "the agent did not exit within {within:?} of {after}\n\
                         --- stderr ---\n{}",
                        self.stderr()
                    );
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }

    fn stdout_bytes(&self) -> usize {
        self.seen_stdout.load(Ordering::SeqCst)
    }

    /// Wait until the agent's stderr contains `needle`.
    fn wait_for_stderr(&self, needle: &str, within: Duration) {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if self.stderr().contains(needle) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "{needle:?} never appeared on stderr within {within:?}\n--- stderr ---\n{}",
            self.stderr()
        );
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The state directory the binary reports at startup, so a test can watch
/// it appear and (for a temp one) disappear.
fn state_dir_from(stderr: &str) -> PathBuf {
    for line in stderr.lines() {
        if let Some(rest) = line.split("solo state directory: ").nth(1) {
            return PathBuf::from(rest.trim());
        }
    }
    panic!("no 'solo state directory:' line on stderr:\n{stderr}");
}

/// Poll until `check` passes, failing loudly with stderr on timeout.
fn wait_for(label: &str, agent: &Agent, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!(
        "timed out waiting for {label}\n--- stderr ---\n{}",
        agent.stderr()
    );
}

#[test]
fn a_prompt_runs_a_turn_and_ends_it() {
    let cwd = scratch_dir("chat-cwd");
    let mut agent = Agent::spawn_mock("chat");

    let init = agent.initialize();
    assert_eq!(
        init.get("protocolVersion").and_then(Value::as_u64),
        Some(1),
        "we speak ACP v1: {init}"
    );

    let session = agent.new_session(&cwd);
    let response = agent.prompt(&session, "say something");
    assert_eq!(
        response.get("stopReason").and_then(Value::as_str),
        Some("end_turn"),
        "a scripted turn ends cleanly: {response}"
    );
    assert!(
        agent.agent_text().contains("solo mock speaking"),
        "the model's text reached the client as a chunk; saw {:?}",
        agent.agent_text()
    );
}

#[test]
fn a_file_tool_call_writes_inside_the_session_cwd() {
    let cwd = scratch_dir("file-cwd");
    let mut agent = Agent::spawn_mock("file");

    agent.initialize();
    let session = agent.new_session(&cwd);
    let response = agent.prompt(&session, "write the file");
    assert_eq!(
        response.get("stopReason").and_then(Value::as_str),
        Some("end_turn"),
        "the scripted tool turn ends cleanly: {response}"
    );

    // The script's `write` names a RELATIVE path, so this also proves the
    // ACP session cwd reached the kernel's file tool.
    let written = cwd.join("solo-wrote-this.txt");
    assert!(
        written.is_file(),
        "the file tool wrote nothing at {}\n--- stderr ---\n{}",
        written.display(),
        agent.stderr()
    );
    assert_eq!(
        std::fs::read_to_string(&written).expect("read what the model wrote"),
        "written by the solo kernel\n"
    );
}

/// A directory outside every fixed read-write mount, removed at the end of
/// the test. `/var/tmp` is deliberately neither `$HOME/src` nor `/tmp`:
/// `/home/atobey/src/bench-work` is under `$HOME/src` and is already
/// writable, so it could not tell a working mount from a missing one.
struct Outside(PathBuf);

impl Outside {
    fn new(label: &str) -> Self {
        let path = PathBuf::from("/var/tmp").join(format!(
            "kaijutsu-solo-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("create a directory outside the fixed mounts");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn wrote(&self) -> bool {
        self.0.join("solo-wrote-this.txt").is_file()
    }
}

impl Drop for Outside {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_named_mount_makes_a_directory_writable() {
    let outside = Outside::new("mounted");
    let mut agent = Agent::spawn_mock_with(
        "file",
        &["--mount", outside.path().to_str().expect("utf-8 path")],
    );

    agent.initialize();
    let session = agent.new_session(outside.path());
    let response = agent.prompt(&session, "write the file");
    assert_eq!(
        response.get("stopReason").and_then(Value::as_str),
        Some("end_turn"),
        "the scripted tool turn ends cleanly: {response}"
    );

    assert!(
        outside.wrote(),
        "a --mount directory must be writable by the model's file tools\n\
         --- stderr ---\n{}",
        agent.stderr()
    );
    assert_eq!(
        std::fs::read_to_string(outside.path().join("solo-wrote-this.txt"))
            .expect("read what the model wrote"),
        "written by the solo kernel\n",
        "the bytes are on the host disk, not only in the kernel"
    );
}

#[test]
fn an_unmounted_directory_stays_read_only() {
    let outside = Outside::new("unmounted");
    // Launched from the crate directory (under $HOME/src, already writable),
    // so the default cwd mount adds nothing and this session cwd is reached
    // only through the read-only root.
    let mut agent = Agent::spawn_mock("file");

    agent.initialize();
    let session = agent.new_session(outside.path());
    agent.prompt(&session, "write the file");

    assert!(
        !outside.wrote(),
        "without a mount the model must not reach outside the perimeter"
    );
}

#[test]
fn the_launch_cwd_is_mounted_by_default() {
    let outside = Outside::new("launch-cwd");
    let mut agent = Agent::spawn_mock_from("file", outside.path(), &[]);

    agent.initialize();
    let session = agent.new_session(outside.path());
    agent.prompt(&session, "write the file");

    assert!(
        outside.wrote(),
        "the directory the agent was launched in is its workspace\n\
         --- stderr ---\n{}",
        agent.stderr()
    );
}

#[test]
fn no_cwd_mount_leaves_the_launch_directory_read_only() {
    let outside = Outside::new("no-cwd-mount");
    let mut agent = Agent::spawn_mock_from("file", outside.path(), &["--no-cwd-mount"]);

    agent.initialize();
    let session = agent.new_session(outside.path());
    agent.prompt(&session, "write the file");

    assert!(
        !outside.wrote(),
        "--no-cwd-mount means what it says\n--- stderr ---\n{}",
        agent.stderr()
    );
}

#[test]
fn an_unusable_mount_refuses_the_boot() {
    let missing = Outside::new("missing").path().join("not-here");
    let cases: [(&str, &[&str]); 4] = [
        ("absolute", &["--mount", "work/project"]),
        (
            "does not exist",
            &["--mount", missing.to_str().expect("utf-8 path")],
        ),
        ("read-only root", &["--mount", "/"]),
        ("reserved", &["--mount", "/config"]),
    ];
    for (reason, args) in cases {
        let output = Command::new(BIN)
            .arg("--backend-kind")
            .arg("mock")
            .arg("--model")
            .arg("solo-mock")
            .args(args)
            .env("TMPDIR", scratch_dir("tmp"))
            .output()
            .unwrap_or_else(|e| panic!("run {args:?}: {e}"));
        assert!(
            !output.status.success(),
            "{args:?} must refuse the boot, got {:?}",
            output.status
        );
        assert!(
            output.stdout.is_empty(),
            "stdout stays empty on a refusal: {:?}",
            String::from_utf8_lossy(&output.stdout)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(reason),
            "{args:?} must say why ({reason}):\n{stderr}"
        );
        assert!(
            stderr.contains(args[1]),
            "{args:?} must name the path:\n{stderr}"
        );
    }
}

#[test]
fn no_provider_key_refuses_before_anything_starts() {
    let mut command = Command::new(BIN);
    command
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env("TMPDIR", scratch_dir("tmp"));
    let output = command.output().expect("run the binary with no provider");

    assert!(
        !output.status.success(),
        "a kernel with no model must not start: {:?}",
        output.status
    );
    assert!(
        output.stdout.is_empty(),
        "stdout is the ACP wire and must stay empty on a refusal: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    for var in ["DEEPSEEK_API_KEY", "ANTHROPIC_API_KEY", "OPENAI_API_KEY"] {
        assert!(
            stderr.contains(var),
            "the refusal names what to set; {var} missing from:\n{stderr}"
        );
    }
}

#[test]
fn an_unknown_provider_is_refused_on_stderr() {
    let output = Command::new(BIN)
        .arg("--backend-kind")
        .arg("groq")
        .output()
        .expect("run the binary with an unknown provider");
    assert!(!output.status.success(), "an unknown provider is refused");
    assert!(
        output.stdout.is_empty(),
        "stdout stays empty: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    for known in ["anthropic", "deepseek", "openai"] {
        assert!(
            stderr.contains(known),
            "the refusal lists the providers it does know; {known} missing from:\n{stderr}"
        );
    }
}

#[test]
fn a_named_gate_policy_lands_in_this_kernels_config() {
    let state = scratch_dir("gate-state").join("state");
    let policy = scratch_dir("gate-policy").join("gate.toml");
    let body = "[global]\nallow = [\n  \"ls\",\n]\n";
    std::fs::write(&policy, body).expect("write the gate policy");

    let mut agent = Agent::spawn_mock_with(
        "chat",
        &[
            "--state-dir",
            state.to_str().expect("utf-8 state path"),
            "--gate-config",
            policy.to_str().expect("utf-8 policy path"),
        ],
    );
    // The policy is installed once the kernel is up, which `initialize`
    // proves.
    agent.initialize();

    let installed = state.join("config").join("kernel").join("gate.toml");
    wait_for("the gate policy to be installed", &agent, || {
        installed.is_file()
    });
    assert_eq!(
        std::fs::read_to_string(&installed).expect("read the installed policy"),
        body,
        "the policy is copied verbatim"
    );

    let status = agent.close_stdin_and_wait(Duration::from_secs(60));
    assert_eq!(status.code(), Some(0));
    assert!(state.is_dir(), "a named state directory survives the exit");
}

/// A minimal rc overlay: `coder/create/S00-stance.kai` reads its companion
/// Markdown the same way `contrib/bench/rc-variants/coder-driven` does, and
/// the companion carries `marker` as its whole body. The shipped
/// `coder/create/S00-stance.kai` is already a regular file (not a symlink),
/// so this overlay exercises the plain-replace path; the symlink-unlink path
/// is covered in `state::tests`.
fn write_marker_overlay(dir: &Path, marker: &str) {
    let coder_dir = dir.join("coder").join("create");
    std::fs::create_dir_all(&coder_dir).expect("create the overlay's coder dir");
    std::fs::write(dir.join("README.md"), "test fixture, not applied\n")
        .expect("write the overlay README");
    std::fs::write(
        coder_dir.join("S00-stance.kai"),
        "set -e\nkj block create --role system --kind text --content-type text/markdown < \"$(dirname \"$0\")/$(basename \"$0\" .kai).md\"\n",
    )
    .expect("write the overlay stance script");
    std::fs::write(coder_dir.join("S00-stance.md"), format!("{marker}\n"))
        .expect("write the overlay stance companion");
}

/// The overlay's marker reaches the coder context's durable system
/// instructions. Observable: reopen the state dir's `kernel.db` after the
/// binary exits (stdin EOF checkpoints it) and read the context's blocks
/// back through the same `BlockStore::load_from_db` + `block_snapshots` path
/// the kernel itself uses — the ACP `sessionId` IS the context id's hex form
/// (`kaijutsu_acp::rank::session_id_of`), so no extra lookup is needed to
/// find which context to read.
#[test]
fn an_rc_overlay_marker_reaches_the_contexts_system_instructions() {
    let state = scratch_dir("rc-overlay-state").join("state");
    let overlay = scratch_dir("rc-overlay-variant");
    let marker = "MARKER-rc-overlay-test-3f9c7a21-unique-instruction-sentence";
    write_marker_overlay(&overlay, marker);

    let mut agent = Agent::spawn_mock_with(
        "chat",
        &[
            "--state-dir",
            state.to_str().expect("utf-8 state path"),
            "--rc-overlay",
            overlay.to_str().expect("utf-8 overlay path"),
        ],
    );
    agent.initialize();
    let cwd = scratch_dir("rc-overlay-cwd");
    // `session/new` runs the create lifecycle synchronously, so the
    // instructions are already durable blocks once this returns; no prompt
    // is needed.
    let session = agent.new_session(&cwd);

    let status = agent.close_stdin_and_wait(Duration::from_secs(60));
    assert_eq!(status.code(), Some(0), "a clean exit checkpoints the db\n--- stderr ---\n{}", agent.stderr());

    let context_id = kaijutsu_types::ContextId::parse(&session)
        .unwrap_or_else(|e| panic!("session id {session:?} is not a context id: {e}"));
    let db = Arc::new(parking_lot::Mutex::new(
        kaijutsu_kernel::kernel_db::KernelDb::open(state.join("kernel.db"))
            .expect("reopen the kernel db the binary just closed"),
    ));
    let blocks = kaijutsu_kernel::block_store::shared_block_store_with_db(
        db,
        kaijutsu_types::WorkspaceId::new(),
        kaijutsu_types::PrincipalId::system(),
    );
    blocks.load_from_db().expect("load documents from the reopened db");
    let snapshots = blocks
        .block_snapshots(context_id)
        .unwrap_or_else(|e| panic!("read blocks for context {context_id}: {e}"));

    let marker_block = snapshots
        .iter()
        .find(|b| b.content.contains(marker))
        .unwrap_or_else(|| {
            let bodies: Vec<&str> = snapshots.iter().map(|b| b.content.as_str()).collect();
            panic!("no block carried the overlay marker {marker:?}; saw {bodies:?}")
        });
    assert_eq!(
        marker_block.role,
        kaijutsu_types::Role::System,
        "the overlay's stance script authors a system block, same as the shipped one"
    );
}

#[test]
fn an_unusable_rc_overlay_refuses_the_boot() {
    let missing = scratch_dir("rc-overlay-refusals").join("missing");
    let plain_file = scratch_dir("rc-overlay-refusals").join("a-file");
    std::fs::write(&plain_file, "not a directory\n").expect("write a plain file");
    let readme_only = scratch_dir("rc-overlay-refusals").join("readme-only");
    std::fs::create_dir_all(&readme_only).expect("create the readme-only overlay");
    std::fs::write(readme_only.join("README.md"), "docs only\n").expect("write the readme");
    let typo = scratch_dir("rc-overlay-refusals").join("typo");
    std::fs::create_dir_all(typo.join("coderr").join("create")).expect("create the typo'd dir");
    std::fs::write(typo.join("coderr").join("create").join("S00-base.kai"), "# typo\n")
        .expect("write the typo'd file");

    let cases: [(&str, &std::path::Path); 4] = [
        ("does not exist", &missing),
        ("is not a directory", &plain_file),
        ("nothing to apply", &readme_only),
        ("no seeded directory", &typo),
    ];
    for (reason, overlay) in cases {
        let output = Command::new(BIN)
            .arg("--backend-kind")
            .arg("mock")
            .arg("--model")
            .arg("solo-mock")
            .arg("--rc-overlay")
            .arg(overlay)
            .env("TMPDIR", scratch_dir("tmp"))
            .output()
            .unwrap_or_else(|e| panic!("run --rc-overlay {}: {e}", overlay.display()));
        assert!(
            !output.status.success(),
            "--rc-overlay {} must refuse the boot, got {:?}",
            overlay.display(),
            output.status
        );
        assert!(
            output.stdout.is_empty(),
            "stdout stays empty on a refusal: {:?}",
            String::from_utf8_lossy(&output.stdout)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(reason),
            "--rc-overlay {} must say why ({reason}):\n{stderr}",
            overlay.display()
        );
    }
}

/// The line the binary prints once the ACP bridge is up. Every exit test
/// waits for it, so a signal always lands on a fully-started process.
const SERVING: &str = "serving ACP v1 on stdio";

#[test]
fn a_signal_removes_the_temp_state_before_exiting() {
    for signal in [libc::SIGTERM, libc::SIGINT] {
        let mut agent = Agent::spawn_mock("chat");
        agent.wait_for_stderr(SERVING, Duration::from_secs(60));
        let state = state_dir_from(&agent.stderr());
        assert!(state.is_dir(), "{} exists while serving", state.display());

        agent.signal(signal);
        let status = agent.wait_for_exit("the signal", Duration::from_secs(30));
        assert!(
            status.code().is_some(),
            "signal {signal}: the agent exits rather than dying on the signal: {status:?}"
        );
        assert_eq!(
            agent.stdout_bytes(),
            0,
            "signal {signal}: nothing was asked, so stdout must have stayed empty"
        );
        wait_for("the temp state directory to be removed", &agent, || {
            !state.exists()
        });
    }
}

#[test]
fn a_signal_leaves_a_named_state_directory_alone() {
    let state = scratch_dir("signal-state").join("state");
    let mut agent = Agent::spawn_mock_with(
        "chat",
        &["--state-dir", state.to_str().expect("utf-8 state path")],
    );
    agent.wait_for_stderr(SERVING, Duration::from_secs(60));

    agent.signal(libc::SIGTERM);
    agent.wait_for_exit("the signal", Duration::from_secs(30));
    assert!(
        state.join("kernel.db").is_file(),
        "a named state directory is the operator's, never ours to remove"
    );
}

/// The kernel failing while it serves: the process must say so, exit
/// non-zero, and still take its temp state with it.
#[test]
fn a_kernel_that_fails_while_serving_exits_and_cleans_up() {
    let mut agent = Agent::spawn_mock_with("chat", &["--fail-kernel-after-serving"]);
    agent.wait_for_stderr(SERVING, Duration::from_secs(60));
    let state = state_dir_from(&agent.stderr());

    let status = agent.wait_for_exit("the kernel failure", Duration::from_secs(30));
    assert_eq!(
        status.code(),
        Some(1),
        "a dead kernel is a failed run\n--- stderr ---\n{}",
        agent.stderr()
    );
    assert!(
        agent.stderr().contains("the kernel failed"),
        "the reason reaches stderr:\n{}",
        agent.stderr()
    );
    wait_for("the temp state directory to be removed", &agent, || {
        !state.exists()
    });
}

/// A panic on the kernel thread is the same kind of event as a failure: it
/// must not leave the ACP client waiting on a kernel that no longer exists.
#[test]
fn a_kernel_panic_while_serving_exits_and_cleans_up() {
    let mut agent = Agent::spawn_mock_with("chat", &["--panic-kernel-after-serving"]);
    agent.wait_for_stderr(SERVING, Duration::from_secs(60));
    let state = state_dir_from(&agent.stderr());

    let status = agent.wait_for_exit("the kernel panic", Duration::from_secs(30));
    assert_eq!(
        status.code(),
        Some(1),
        "a panicked kernel is a failed run\n--- stderr ---\n{}",
        agent.stderr()
    );
    assert!(
        agent.stderr().contains("the kernel panicked"),
        "the panic is named on stderr:\n{}",
        agent.stderr()
    );
    wait_for("the temp state directory to be removed", &agent, || {
        !state.exists()
    });
}

#[test]
fn a_state_dir_inside_the_operators_kaijutsu_refuses() {
    let home = scratch_dir("operator-home");
    let data = home.join("data");
    let config = home.join("config");
    for inside in [data.join("kaijutsu").join("kernel"), config.join("kaijutsu")] {
        let output = Command::new(BIN)
            .arg("--backend-kind")
            .arg("mock")
            .arg("--model")
            .arg("solo-mock")
            .arg("--state-dir")
            .arg(&inside)
            .env("XDG_DATA_HOME", &data)
            .env("XDG_CONFIG_HOME", &config)
            .env("TMPDIR", scratch_dir("tmp"))
            .output()
            .expect("run the binary against the operator's own tree");
        assert!(
            !output.status.success(),
            "{} is the operator's kernel, not a solo one",
            inside.display()
        );
        assert!(output.stdout.is_empty(), "stdout stays empty on a refusal");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&inside.display().to_string()),
            "the refusal names the directory:\n{stderr}"
        );
        assert!(
            !inside.exists(),
            "a refused state directory is not created: {}",
            inside.display()
        );
    }
}

/// A model-spawned process running as the same uid as this agent must not
/// be able to read the provider API key out of `/proc/<pid>/environ` — see
/// docs/solo-acp.md, "The key and /proc". This proves it from the outside,
/// the way any other same-uid process on the host would try: reading
/// `/proc/<agent pid>/environ` once the agent is up must fail with
/// permission denied, never succeed and hand back a sentinel we set in this
/// process's own environment when we spawned it.
#[test]
fn proc_environ_is_unreadable_to_same_uid_readers() {
    // SAFETY: geteuid takes no arguments and only reads this process's own
    // credentials.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!(
            "skipping proc_environ_is_unreadable_to_same_uid_readers: this test \
             process runs as root, which can always read /proc/<pid>/environ \
             regardless of PR_SET_DUMPABLE"
        );
        return;
    }

    let sentinel = format!("SOLO_ACP_PROC_ENVIRON_SENTINEL_{}", std::process::id());
    let mut command = Command::new(BIN);
    command
        .arg("--backend-kind")
        .arg("mock")
        .arg("--model")
        .arg("solo-mock")
        .env("KJ_MOCK_SCRIPT_DIR", mock_scripts("chat"))
        .env("TMPDIR", scratch_dir("tmp"))
        .env("RUST_LOG", "info")
        .env(&sentinel, "leaked-if-this-is-readable");
    let agent = Agent::spawn(command);
    agent.wait_for_stderr(SERVING, Duration::from_secs(60));

    let pid = agent.child.id();
    let environ_path = format!("/proc/{pid}/environ");
    match std::fs::read(&environ_path) {
        Err(e) => {
            assert_eq!(
                e.kind(),
                std::io::ErrorKind::PermissionDenied,
                "{environ_path} must be unreadable to a same-uid process with EACCES, got {e}"
            );
        }
        Ok(bytes) => {
            let leaked = String::from_utf8_lossy(&bytes).contains(&sentinel);
            panic!(
                "{environ_path} was readable by a same-uid process (sentinel present: \
                 {leaked}); the PR_SET_DUMPABLE mitigation is missing or not taking effect"
            );
        }
    }
}

/// `--max-tokens` writes into `llm_defaults.max_tokens`
/// (`kaijutsu_kernel::kernel_db::LlmDefaultsRow`), a plain DB row rather than
/// something the ACP wire reports — so this reads it back through the
/// library the kernel itself uses, from a state directory the binary was
/// told to keep.
#[test]
fn max_tokens_overrides_the_written_defaults_row() {
    let state = scratch_dir("max-tokens-state").join("state");
    let mut agent = Agent::spawn_mock_with(
        "chat",
        &["--state-dir", state.to_str().expect("utf-8 state path"), "--max-tokens", "4096"],
    );
    agent.initialize();

    let status = agent.close_stdin_and_wait(Duration::from_secs(60));
    assert_eq!(status.code(), Some(0));

    let db = kaijutsu_kernel::kernel_db::KernelDb::open(state.join("kernel.db"))
        .expect("reopen the kernel db the binary just closed");
    let defaults = db
        .get_llm_defaults()
        .expect("read the model defaults")
        .expect("the binary wrote a defaults row");
    assert_eq!(
        defaults.max_tokens,
        Some(4096),
        "--max-tokens 4096 must land in the written defaults row"
    );
}

/// Without the flag, the factory ceiling from `seed_backends` stands — the
/// same default `docs/solo-acp.md` documents.
#[test]
fn without_max_tokens_the_factory_ceiling_stands_in_the_written_defaults_row() {
    let state = scratch_dir("max-tokens-default-state").join("state");
    let mut agent = Agent::spawn_mock_with(
        "chat",
        &["--state-dir", state.to_str().expect("utf-8 state path")],
    );
    agent.initialize();

    let status = agent.close_stdin_and_wait(Duration::from_secs(60));
    assert_eq!(status.code(), Some(0));

    let db = kaijutsu_kernel::kernel_db::KernelDb::open(state.join("kernel.db"))
        .expect("reopen the kernel db the binary just closed");
    let defaults = db
        .get_llm_defaults()
        .expect("read the model defaults")
        .expect("the binary wrote a defaults row");
    assert_eq!(defaults.max_tokens, Some(16384), "the factory ceiling stands unless asked");
}

#[test]
fn stdin_eof_exits_clean_and_removes_the_temp_state() {
    let mut agent = Agent::spawn_mock("chat");
    agent.initialize();

    let state = state_dir_from(&agent.stderr());
    assert!(state.is_dir(), "{} should exist while serving", state.display());

    let status = agent.close_stdin_and_wait(Duration::from_secs(60));
    assert_eq!(
        status.code(),
        Some(0),
        "a client disconnect is a clean exit\n--- stderr ---\n{}",
        agent.stderr()
    );
    wait_for("the temp state directory to be removed", &agent, || {
        !state.exists()
    });
}
