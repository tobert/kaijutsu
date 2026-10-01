//! e2e: MCP servers an ACP client declares on `session/new`,
//! `session/load`, and `session/resume` (docs/acp.md, "Client-declared MCP
//! servers").
//!
//! Each test spawns the real `kaijutsu-solo-acp` with the scripted mock model
//! and declares `mcp-fixture` (`src/bin/mcp_fixture.rs`), a one-tool MCP
//! stdio server built by this crate. The fixture is the only host process a
//! test launches besides the agent itself.
//!
//! `cargo test -p kaijutsu-solo-acp --features test-mock --test acp_mcp_servers`

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kaijutsu_acp_fleet::client::{self, AcpClient, AgentCommand};
use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_kaijutsu-solo-acp");
const FIXTURE: &str = env!("CARGO_BIN_EXE_mcp-fixture");

/// Real disk under `$HOME/src`, the kernel's read-write VFS mount; see
/// `solo_acp_stdio.rs`.
const SCRATCH: &str = "/home/atobey/src/bench-work/solo";

const REPLY_TIMEOUT: Duration = Duration::from_secs(120);

fn scratch_dir(label: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    let path = PathBuf::from(SCRATCH).join(format!("{label}-{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&path).expect("create the scratch directory");
    path
}

/// The agent with the mock backend, replaying `replies` in order across
/// every turn of the run.
fn spawn_with_replies(replies: Value) -> AcpClient {
    let scripts = scratch_dir("mcp-scripts");
    std::fs::write(scripts.join("solo-mock.json"), replies.to_string()).expect("write the mock script");
    let command = AgentCommand::new(BIN)
        .arg("--backend-kind")
        .arg("mock")
        .arg("--model")
        .arg("solo-mock")
        .env("KJ_MOCK_SCRIPT_DIR", scripts)
        .env("TMPDIR", scratch_dir("tmp"))
        .env("RUST_LOG", "info");
    let mut agent = AcpClient::spawn(&command, REPLY_TIMEOUT).unwrap_or_else(|e| panic!("spawn {BIN}: {e:#}"));
    agent.initialize().expect("initialize");
    agent
}

fn tool_use(id: &str, name: &str, input: Value) -> Value {
    json!([
        {"ToolUse": {"id": id, "name": name, "input": input}},
        {"Done": {"stop_reason": "tool_use", "input_tokens": 1, "output_tokens": 1, "extra": null}}
    ])
}

/// A reply that says `reply` and ends the task with `done`, as a coder's
/// last reply must (`docs/conversation-session.md`, "Ending a task with done").
fn done(id: &str, reply: &str) -> Value {
    json!([
        "TextStart",
        {"TextDelta": reply},
        "TextEnd",
        {"ToolUse": {"id": id, "name": "done", "input": {"status": "done", "summary": reply}}},
        {"Done": {"stop_reason": "tool_use", "input_tokens": 1, "output_tokens": 1, "extra": null}}
    ])
}

/// The ACP `McpServerStdio` declaration for the fixture, writing its pid to
/// `pidfile`.
fn fixture_server(name: &str, pidfile: &Path) -> Value {
    json!({
        "name": name,
        "command": FIXTURE,
        "args": [],
        "env": [{"name": "MCP_FIXTURE_PIDFILE", "value": pidfile}],
    })
}

fn new_session(agent: &mut AcpClient, cwd: &Path, servers: Value) -> anyhow::Result<String> {
    let result = agent.request("session/new", json!({"cwd": cwd, "mcpServers": servers}))?;
    Ok(result
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("session/new returned no sessionId: {result}"))
        .to_string())
}

fn prompt(agent: &mut AcpClient, session: &str, text: &str) -> Value {
    agent.prompt(session, text).unwrap_or_else(|e| panic!("{e:#}"))
}

/// Wait for the fixture to write its pidfile, and return the pid.
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
fn process_alive(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => {
            let state = stat.rsplit(')').next().and_then(|rest| rest.split_whitespace().next());
            state != Some("Z") && state != Some("X")
        }
        Err(_) => false,
    }
}

fn wait_until_gone(pid: i32, agent: &AcpClient, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while process_alive(pid) {
        assert!(
            Instant::now() < deadline,
            "MCP fixture pid {pid} still running after {what}\n--- agent stderr ---\n{}",
            agent.stderr_tail()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_declared_server_is_callable_and_stops_when_the_session_closes() {
    let cwd = scratch_dir("mcp-call");
    let pidfile = cwd.join("fixture.pid");
    let mut agent = spawn_with_replies(json!([
        tool_use("mcp-call-1", "fixture_echo", json!({"text": "hi"})),
        done("mcp-call-done", "echoed"),
    ]));

    let session = new_session(&mut agent, &cwd, json!([fixture_server("fixture", &pidfile)]))
        .unwrap_or_else(|e| panic!("{e:#}"));
    let pid = wait_for_pid(&pidfile);
    assert!(process_alive(pid), "the fixture should be running once session/new returns");

    let response = prompt(&mut agent, &session, "echo hi through the fixture");
    assert_eq!(response.get("stopReason").and_then(Value::as_str), Some("end_turn"), "{response}");
    let calls = client::tool_calls(agent.updates());
    let call = calls
        .iter()
        .find(|c| c.title.contains("fixture_echo"))
        .unwrap_or_else(|| panic!("no fixture_echo tool call in {calls:?}"));
    assert_eq!(call.status.as_deref(), Some("completed"), "{call:?}");
    assert!(call.output.contains("fixture echo: hi"), "tool output: {call:?}");

    agent
        .request("session/close", json!({"sessionId": session}))
        .unwrap_or_else(|e| panic!("session/close: {e:#}"));
    wait_until_gone(pid, &agent, "session/close");
}

#[test]
fn a_server_stops_when_the_client_disconnects() {
    let cwd = scratch_dir("mcp-disconnect");
    let pidfile = cwd.join("fixture.pid");
    let mut agent = spawn_with_replies(json!([]));

    new_session(&mut agent, &cwd, json!([fixture_server("fixture", &pidfile)]))
        .unwrap_or_else(|e| panic!("{e:#}"));
    let pid = wait_for_pid(&pidfile);

    agent.shutdown(Duration::from_secs(30)).unwrap_or_else(|e| panic!("{e:#}"));
    wait_until_gone(pid, &agent, "the client closed stdin");
}

#[test]
fn resuming_with_the_same_servers_keeps_the_running_process() {
    let cwd = scratch_dir("mcp-resume");
    let pidfile = cwd.join("fixture.pid");
    let mut agent = spawn_with_replies(json!([]));
    let servers = json!([fixture_server("fixture", &pidfile)]);

    let session = new_session(&mut agent, &cwd, servers.clone()).unwrap_or_else(|e| panic!("{e:#}"));
    let pid = wait_for_pid(&pidfile);
    std::fs::remove_file(&pidfile).expect("clear the pidfile");

    agent
        .request("session/resume", json!({"sessionId": session, "cwd": cwd, "mcpServers": servers}))
        .unwrap_or_else(|e| panic!("session/resume: {e:#}"));
    agent
        .request("session/load", json!({"sessionId": session, "cwd": cwd, "mcpServers": servers}))
        .unwrap_or_else(|e| panic!("session/load: {e:#}"));

    std::thread::sleep(Duration::from_millis(500));
    assert!(!pidfile.exists(), "a second fixture process started on resume or load");
    assert!(process_alive(pid), "the original fixture process stopped on resume or load");
}

#[test]
fn duplicate_server_names_are_invalid_params() {
    let cwd = scratch_dir("mcp-dup");
    let mut agent = spawn_with_replies(json!([]));
    let err = new_session(
        &mut agent,
        &cwd,
        json!([fixture_server("twin", &cwd.join("a.pid")), fixture_server("twin", &cwd.join("b.pid"))]),
    )
    .expect_err("two servers named 'twin' must be refused");
    let message = format!("{err:#}");
    assert!(message.contains("-32602"), "expected invalid_params: {message}");
    assert!(message.contains("twin"), "the error should name the server: {message}");
    assert!(!cwd.join("a.pid").exists() && !cwd.join("b.pid").exists(), "nothing should start");
}

#[test]
fn a_server_that_fails_to_start_fails_session_new() {
    let cwd = scratch_dir("mcp-fail");
    let mut agent = spawn_with_replies(json!([]));
    let err = new_session(
        &mut agent,
        &cwd,
        json!([{"name": "ghost", "command": "/definitely/not/an/mcp-server", "args": [], "env": []}]),
    )
    .expect_err("an unstartable server must fail session/new");
    let message = format!("{err:#}");
    assert!(message.contains("ghost"), "the error should name the server: {message}");
}

#[test]
fn an_undeclared_transport_is_refused() {
    let cwd = scratch_dir("mcp-sse");
    let mut agent = spawn_with_replies(json!([]));
    let err = new_session(
        &mut agent,
        &cwd,
        json!([{"type": "sse", "name": "streamer", "url": "http://127.0.0.1:9/sse", "headers": []}]),
    )
    .expect_err("an sse server must be refused");
    let message = format!("{err:#}");
    assert!(message.contains("-32602"), "expected invalid_params: {message}");
    assert!(message.contains("streamer"), "the error should name the server: {message}");
}

#[test]
fn another_session_does_not_see_the_server() {
    let cwd = scratch_dir("mcp-scope");
    let pidfile = cwd.join("fixture.pid");
    let mut agent = spawn_with_replies(json!([
        tool_use("mcp-scope-1", "fixture_echo", json!({"text": "elsewhere"})),
        done("mcp-scope-done", "tried"),
    ]));

    let declaring = new_session(&mut agent, &cwd, json!([fixture_server("fixture", &pidfile)]))
        .unwrap_or_else(|e| panic!("{e:#}"));
    let pid = wait_for_pid(&pidfile);
    // Another cwd gives another session label, so another context.
    let other_cwd = scratch_dir("mcp-scope-other");
    let other = new_session(&mut agent, &other_cwd, json!([])).unwrap_or_else(|e| panic!("{e:#}"));
    assert_ne!(declaring, other, "the second session must be a different context");

    prompt(&mut agent, &other, "call the fixture from a session that did not declare it");
    let calls = client::tool_calls(agent.updates());
    let call = calls
        .iter()
        .find(|c| c.title.contains("fixture_echo"))
        .unwrap_or_else(|| panic!("the mock's fixture_echo call was not announced: {calls:?}"));
    assert_eq!(call.status.as_deref(), Some("failed"), "{call:?}");
    assert!(
        !call.output.contains("fixture echo: elsewhere"),
        "a session that did not declare the server reached it: {call:?}"
    );
    assert!(process_alive(pid), "the declaring session's server must keep running");
}
