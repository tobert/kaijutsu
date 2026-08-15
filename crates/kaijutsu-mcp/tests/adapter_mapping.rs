//! Adapter field-mapping round-trip.
//!
//! The hook adapters (`contrib/adapters/*.sh`) reshape a source tool's hook
//! JSON into a kaijutsu `HookEvent`. The field map lives in a standalone jq
//! filter so it is testable without the socket round-trip. This test pipes
//! real Claude Code and Codex CLI payloads through the *actual* filters their
//! adapters use and deserializes the result into `HookEvent`, asserting that
//! fields survive.
//!
//! It exists to fail loudly on adapter↔core drift — e.g. a core field rename
//! (`agent_id` → `principal_id`) or a source-field change
//! (`tool_response` → `tool_output`) that would otherwise be dropped silently
//! by serde, mirroring nothing.
//!
//! Requires `jq` on PATH — the adapters depend on it at runtime, so a host
//! that runs them has it.

use std::path::PathBuf;
use std::process::Command;

use kaijutsu_mcp::hook_types::HookEvent;

/// Run the Claude field-map filter over a fixture, return the parsed HookEvent.
fn map_claude(fixture: &str, kj_event: &str) -> HookEvent {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let filter = manifest.join("../../contrib/adapters/claude-to-kaijutsu.jq");
    let fixture_path = manifest.join("tests/fixtures/claude").join(fixture);

    let payload = std::fs::read(&fixture_path)
        .unwrap_or_else(|e| panic!("read fixture {}: {e}", fixture_path.display()));

    let out = Command::new("jq")
        .arg("--arg")
        .arg("event")
        .arg(kj_event)
        .arg("-f")
        .arg(&filter)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(&payload)?;
            child.wait_with_output()
        })
        .expect("spawn jq (is jq installed?)");

    assert!(
        out.status.success(),
        "jq failed on {fixture}: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    serde_json::from_slice::<HookEvent>(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "deserialize HookEvent from {fixture}: {e}\njq output: {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

/// Run the Codex field-map filter over a fixture, return the parsed HookEvent.
fn map_codex(fixture: &str, kj_event: &str) -> HookEvent {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let filter = manifest.join("../../contrib/adapters/codex-to-kaijutsu.jq");
    let fixture_path = manifest.join("tests/fixtures/codex").join(fixture);

    let payload = std::fs::read(&fixture_path)
        .unwrap_or_else(|e| panic!("read fixture {}: {e}", fixture_path.display()));

    let out = Command::new("jq")
        .arg("--arg")
        .arg("event")
        .arg(kj_event)
        .arg("-f")
        .arg(&filter)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(&payload)?;
            child.wait_with_output()
        })
        .expect("spawn jq (is jq installed?)");

    assert!(
        out.status.success(),
        "jq failed on {fixture}: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    serde_json::from_slice::<HookEvent>(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "deserialize HookEvent from {fixture}: {e}\njq output: {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

#[test]
fn post_tool_use_carries_tool_output() {
    // Legacy/fallback shape: some producers emit `.tool_output` as a plain
    // string. The documented Claude Code field is `.tool_response` (see the
    // companion test); the filter must accept both.
    let ev = map_claude("post_tool_use.json", "tool.after");
    assert_eq!(ev.event, "tool.after");
    assert_eq!(ev.source, "claude-code");
    let tool = ev.tool.expect("tool present on tool.after");
    assert_eq!(tool.name, "Bash");
    assert!(
        tool.output.is_some(),
        "tool.output dropped — adapter no longer reads .tool_output fallback"
    );
    assert!(tool.output.unwrap().contains("total 0"));
}

#[test]
fn post_tool_use_carries_tool_response() {
    // Current Claude Code delivers the result under `.tool_response`, often as
    // a JSON object — the filter must map it (stringified when not a string).
    let ev = map_claude("post_tool_use_response.json", "tool.after");
    let tool = ev.tool.expect("tool present on tool.after");
    assert_eq!(tool.name, "Write");
    let output = tool
        .output
        .expect("tool.output dropped — adapter likely ignores .tool_response");
    assert!(
        output.contains("success"),
        "object tool_response not stringified into output: {output}"
    );
}

#[test]
fn adapter_script_emits_single_line_json() {
    // The hook socket listener reads exactly ONE line per event. The filter
    // alone can't prove the script sends compact JSON, so run the real
    // adapter in dry-run mode and assert its stdout is a single parseable
    // line. Guards the `jq -c` in claude.sh.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let script = manifest.join("../../contrib/adapters/claude.sh");
    let fixture = manifest.join("tests/fixtures/claude/post_tool_use_response.json");
    let payload = std::fs::read(&fixture).expect("read fixture");

    let out = Command::new("bash")
        .arg(&script)
        .env("KJ_HOOK_DRYRUN", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child.stdin.take().unwrap().write_all(&payload)?;
            child.wait_with_output()
        })
        .expect("run claude.sh");

    assert!(
        out.status.success(),
        "claude.sh failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    let trimmed = stdout.trim();
    assert!(
        !trimmed.is_empty() && !trimmed.contains('\n'),
        "adapter output is not a single line (listener reads one line per \
         event; claude.sh must use jq -c):\n{stdout}"
    );
    let ev: HookEvent = serde_json::from_str(trimmed).expect("parse adapter output");
    assert_eq!(ev.event, "tool.after");
    assert!(ev.tool.and_then(|t| t.output).is_some());
}

#[test]
fn post_tool_use_failure_carries_error() {
    let ev = map_claude("post_tool_use_failure.json", "tool.error");
    let tool = ev.tool.expect("tool present on tool.error");
    assert_eq!(tool.name, "Bash");
    assert_eq!(tool.error.as_deref(), Some("Command exited with code 1"));
}

#[test]
fn subagent_stop_carries_principal_id() {
    // Core renamed agent_id → principal_id; the adapter must emit the new key.
    let ev = map_claude("subagent_stop.json", "subagent.stop");
    assert_eq!(
        ev.principal_id.as_deref(),
        Some("agent-7f2654d6"),
        "principal_id dropped — adapter likely still emits the `agent_id` key"
    );
    assert_eq!(ev.agent_type.as_deref(), Some("Explore"));
}

#[test]
fn user_prompt_submit_carries_prompt() {
    let ev = map_claude("user_prompt_submit.json", "prompt.submit");
    assert_eq!(ev.prompt.as_deref(), Some("refactor the hook adapter"));
}

#[test]
fn session_start_carries_model_and_cwd() {
    let ev = map_claude("session_start.json", "session.start");
    assert_eq!(ev.model.as_deref(), Some("claude-opus-4-8"));
    assert_eq!(ev.cwd.as_deref(), Some("/home/user/src/demo"));
    assert_eq!(ev.session_id.as_deref(), Some("a1b2c3d4-0000-0000-0000-000000000005"));
}

#[test]
fn codex_post_tool_use_carries_tool_response() {
    let ev = map_codex("post_tool_use.json", "tool.after");
    assert_eq!(ev.event, "tool.after");
    assert_eq!(ev.source, "codex");
    assert_eq!(ev.session_id.as_deref(), Some("codex-thread-0001"));
    let tool = ev.tool.expect("tool present on Codex tool.after");
    assert_eq!(tool.name, "shell");
    let output = tool.output.expect("Codex tool_response was dropped");
    assert!(output.contains("stdout"));
    assert_eq!(tool.duration_ms, Some(27));
}

#[test]
fn codex_stop_carries_last_assistant_message() {
    let ev = map_codex("stop.json", "agent.stop");
    assert_eq!(ev.source, "codex");
    assert_eq!(
        ev.response.as_deref(),
        Some("The adapter is implemented and tested.")
    );
}

#[test]
fn codex_subagent_stop_carries_identity() {
    let ev = map_codex("subagent_stop.json", "subagent.stop");
    assert_eq!(ev.principal_id.as_deref(), Some("subagent-0007"));
    assert_eq!(ev.agent_type.as_deref(), Some("explorer"));
}

#[test]
fn codex_compaction_carries_trigger() {
    let pre = map_codex("pre_compact.json", "agent.compact");
    assert_eq!(pre.trigger.as_deref(), Some("auto"));
    let post = map_codex("post_compact.json", "agent.compact");
    assert_eq!(post.trigger.as_deref(), Some("manual"));
}

#[test]
fn codex_lifecycle_fields_survive() {
    let start = map_codex("session_start.json", "session.start");
    assert_eq!(start.model.as_deref(), Some("gpt-5-codex"));
    assert_eq!(start.cwd.as_deref(), Some("/home/user/src/demo"));
    assert_eq!(start.transcript_path.as_deref(), Some(
        "/home/user/.codex/sessions/2026/08/14/codex-thread-0001.jsonl"
    ));

    let prompt = map_codex("user_prompt_submit.json", "prompt.submit");
    assert_eq!(prompt.prompt.as_deref(), Some("refactor the hook adapter"));

    let end = map_codex("session_end.json", "session.end");
    assert_eq!(end.reason.as_deref(), Some("exit"));
}

#[test]
fn codex_adapter_script_emits_single_line_json() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let script = manifest.join("../../contrib/adapters/codex.sh");
    let fixture = manifest.join("tests/fixtures/codex/post_tool_use.json");
    let payload = std::fs::read(&fixture).expect("read fixture");

    let out = Command::new("bash")
        .arg(&script)
        .env("KJ_HOOK_DRYRUN", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child.stdin.take().unwrap().write_all(&payload)?;
            child.wait_with_output()
        })
        .expect("run codex.sh");

    assert!(
        out.status.success(),
        "codex.sh failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    let trimmed = stdout.trim();
    assert!(!trimmed.is_empty() && !trimmed.contains('\n'));
    let ev: HookEvent = serde_json::from_str(trimmed).expect("parse adapter output");
    assert_eq!(ev.source, "codex");
    assert_eq!(ev.event, "tool.after");
}
