//! Run one scenario against an ACP agent and judge it.
//!
//! Each run gets a fresh scratch directory holding the workspace (the session
//! cwd and the agent's launch directory), the mock model script, the agent's
//! `TMPDIR`, and the gate policy when the scenario gives one. The agent is a
//! fresh process per scenario, so no state crosses between scenarios.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use serde_json::Value;

use crate::client::{self, AcpClient, AgentCommand, PermissionAnswer, PermissionPolicy, PermissionRecord};
use crate::scenario::{Prompt, Scenario, Verify, workspace_relative};

/// The model name the agent is started with, and so the mock script's file stem.
pub const MOCK_MODEL: &str = "fleet-mock";

#[derive(Debug, Clone)]
pub struct RunConfig {
    /// An ACP agent binary that accepts `--backend-kind mock --model <name>`
    /// and reads `KJ_MOCK_SCRIPT_DIR`: `kaijutsu-solo-acp` built with
    /// `--features test-mock`.
    pub agent: PathBuf,
    /// Where per-run scratch directories are made.
    pub scratch_root: PathBuf,
    /// Keep each run's scratch directory instead of removing it.
    pub keep: bool,
    /// Bounds each wait for a response from the agent.
    pub timeout: Duration,
    /// Print every ACP message to stderr.
    pub trace: bool,
}

impl RunConfig {
    pub fn new(agent: impl Into<PathBuf>, scratch_root: impl Into<PathBuf>) -> Self {
        Self {
            agent: agent.into(),
            scratch_root: scratch_root.into(),
            keep: false,
            timeout: Duration::from_secs(120),
            trace: false,
        }
    }
}

/// The verdict on one scenario.
#[derive(Debug)]
pub struct Outcome {
    pub name: String,
    /// Every expectation that did not hold, or the error that stopped the run.
    /// Empty means the scenario passed.
    pub failures: Vec<String>,
    pub elapsed: Duration,
    /// The agent's stderr tail, kept when the scenario failed.
    pub stderr_tail: String,
    /// The scratch directory, when it was kept.
    pub kept: Option<PathBuf>,
}

impl Outcome {
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }
}

/// The scenario's name: its file stem.
pub fn scenario_name(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
}

/// Load and run one scenario file. Never panics on a scenario problem; every
/// problem becomes a failure in the outcome.
pub fn run_file(path: &Path, config: &RunConfig) -> Outcome {
    let name = scenario_name(path);
    let started = Instant::now();
    match Scenario::load(path) {
        Ok(scenario) => run_scenario(&name, &scenario, config),
        Err(error) => Outcome {
            name,
            failures: vec![format!("{error:#}")],
            elapsed: started.elapsed(),
            stderr_tail: String::new(),
            kept: None,
        },
    }
}

pub fn run_scenario(name: &str, scenario: &Scenario, config: &RunConfig) -> Outcome {
    let started = Instant::now();
    let mut failures = Vec::new();
    let mut stderr_tail = String::new();
    let scratch = match Scratch::create(&config.scratch_root, name) {
        Ok(scratch) => Some(scratch),
        Err(error) => {
            failures.push(format!("{error:#}"));
            None
        }
    };
    if let Some(scratch) = &scratch
        && let Err(error) = drive(scenario, scratch, config, &mut failures, &mut stderr_tail)
    {
        failures.push(format!("{error:#}"));
    }
    let kept = match scratch {
        Some(scratch) if config.keep => Some(scratch.root.clone()),
        Some(scratch) => {
            if let Err(error) = std::fs::remove_dir_all(&scratch.root) {
                failures.push(format!("remove the scratch directory {}: {error}", scratch.root.display()));
            }
            None
        }
        None => None,
    };
    if failures.is_empty() {
        stderr_tail.clear();
    }
    Outcome { name: name.to_string(), failures, elapsed: started.elapsed(), stderr_tail, kept }
}

struct Scratch {
    root: PathBuf,
    workspace: PathBuf,
    mock: PathBuf,
    tmp: PathBuf,
}

impl Scratch {
    fn create(scratch_root: &Path, name: &str) -> Result<Self> {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .context("the clock reads before the epoch")?
            .as_nanos();
        let root = scratch_root.join(format!("{name}-{}-{stamp}", std::process::id()));
        let scratch = Self {
            workspace: root.join("workspace"),
            mock: root.join("mock"),
            tmp: root.join("tmp"),
            root,
        };
        for dir in [&scratch.workspace, &scratch.mock, &scratch.tmp] {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        Ok(scratch)
    }
}

/// Everything between setup and teardown. An `Err` is a run that could not
/// finish; expectation misses go into `failures` and the run continues.
fn drive(
    scenario: &Scenario,
    scratch: &Scratch,
    config: &RunConfig,
    failures: &mut Vec<String>,
    stderr_tail: &mut String,
) -> Result<()> {
    for (path, body) in &scenario.files {
        let target = scratch.workspace.join(workspace_relative(path)?);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        std::fs::write(&target, body).with_context(|| format!("seed {}", target.display()))?;
    }
    let script = serde_json::to_string_pretty(&scenario.mock_script()?)?;
    std::fs::write(scratch.mock.join(format!("{MOCK_MODEL}.json")), script).context("write the mock script")?;

    let mut command = AgentCommand::new(&config.agent)
        .arg("--backend-kind")
        .arg("mock")
        .arg("--model")
        .arg(MOCK_MODEL)
        .env("KJ_MOCK_SCRIPT_DIR", &scratch.mock)
        .env("TMPDIR", &scratch.tmp)
        .env("RUST_LOG", "info")
        .cwd(&scratch.workspace);
    let gate = match &scenario.gate {
        Some(gate) => gate.clone(),
        None => default_gate()?,
    };
    let gate_path = scratch.root.join("gate.toml");
    std::fs::write(&gate_path, gate).context("write the gate policy")?;
    command = command.arg("--gate-config").arg(&gate_path);

    let mut agent = AcpClient::spawn(&command, config.timeout)?;
    agent.set_trace(config.trace);
    let result = converse(&mut agent, scenario, &scratch.workspace, config.timeout, failures);
    // Close stdin even after a failed run so the agent removes its own
    // temporary state; the scratch directory is removed after this.
    let shutdown = agent.shutdown(Duration::from_secs(60));
    *stderr_tail = agent.stderr_tail();
    result?;
    match shutdown {
        Ok(status) if !status.success() => failures.push(format!("the agent exited with {status} after stdin closed")),
        Ok(_) => {}
        Err(error) => failures.push(format!("{error:#}")),
    }
    failures.extend(verify_workspace(&scenario.verify, &scratch.workspace));
    Ok(())
}

fn converse(
    agent: &mut AcpClient,
    scenario: &Scenario,
    workspace: &Path,
    timeout: Duration,
    failures: &mut Vec<String>,
) -> Result<()> {
    let init = agent.initialize()?;
    if init.get("protocolVersion").and_then(Value::as_u64) != Some(1) {
        failures.push(format!("initialize: expected protocolVersion 1, got {init}"));
    }
    let session = agent.new_session(workspace)?;
    for (n, prompt) in scenario.prompt.iter().enumerate() {
        let label = format!("prompt {}", n + 1);
        let answers: VecDeque<PermissionAnswer> = prompt.permissions.iter().map(|&a| a.into()).collect();
        agent.set_permission_policy(PermissionPolicy::Queue(answers));
        let updates_before = agent.updates().len();
        let permissions_before = agent.permissions().len();
        let response = agent.prompt(&session, &prompt.text).with_context(|| label.clone())?;
        // A gate ask does not hold the turn open: the tool call is refused as
        // pending, the turn ends, and the permission request follows. An
        // answer can start a follow-up model turn, whose text arrives later.
        let want = permissions_before + prompt.permissions.len();
        agent.pump_until(timeout, "permission requests", |a| a.permissions().len() >= want)?;
        agent.pump_until(timeout, "the expected agent text", |a| {
            let text = client::agent_text(&a.updates()[updates_before..]);
            prompt.text_contains.iter().all(|needle| text.contains(needle.as_str()))
        })?;
        let seen = Seen {
            response: &response,
            updates: &agent.updates()[updates_before..],
            permissions: &agent.permissions()[permissions_before..],
        };
        failures.extend(check_prompt(&label, prompt, &seen));
    }
    Ok(())
}

/// The shipped gate policy.
const SHIPPED_GATE: &str = include_str!("../../../assets/defaults/gate.toml");

/// The gate policy a scenario runs under when it names none: the shipped
/// policy without its `[classifier]` table. A run then never reaches for a
/// classifier on the network, and the advisory hook fails closed at once.
pub fn default_gate() -> Result<String> {
    let mut policy: toml::Table = SHIPPED_GATE.parse().context("parse the shipped gate policy")?;
    policy.remove("classifier");
    toml::to_string(&policy).context("write the default gate policy")
}

/// The text the ACP bridge sends as an agent message when a model turn fails
/// outside a prompt, such as a follow-up turn started by a permission answer.
/// The update stream carries no structured signal for that case.
pub const FAILED_TURN_TEXT: &str = "stream error:";

/// What one prompt produced on the wire: everything from sending it until the
/// runner stopped waiting for its permission requests and expected text.
pub struct Seen<'a> {
    pub response: &'a Value,
    pub updates: &'a [Value],
    pub permissions: &'a [PermissionRecord],
}

/// Every way `seen` misses `prompt`'s expectations.
pub fn check_prompt(label: &str, prompt: &Prompt, seen: &Seen<'_>) -> Vec<String> {
    let mut failures = Vec::new();
    let stop = seen.response.get("stopReason").and_then(Value::as_str);
    if stop != Some(prompt.stop_reason.as_str()) {
        failures.push(format!("{label}: expected stopReason {:?}, got {stop:?} ({})", prompt.stop_reason, seen.response));
    }

    let text = client::agent_text(seen.updates);
    if text.contains(FAILED_TURN_TEXT) {
        failures.push(format!("{label}: a model turn failed; agent text was {text:?}"));
    }
    for needle in &prompt.text_contains {
        if !text.contains(needle.as_str()) {
            failures.push(format!("{label}: agent text does not contain {needle:?}; it was {text:?}"));
        }
    }

    if let Some(expected) = &prompt.tool_calls {
        let calls = client::tool_calls(seen.updates);
        let got: Vec<String> =
            calls.iter().map(|c| format!("{} ({})", c.title, c.status.as_deref().unwrap_or("no status"))).collect();
        let matches = calls.len() == expected.len()
            && calls.iter().zip(expected).all(|(call, want)| {
                call.title == want.title && want.status.as_ref().is_none_or(|s| call.status.as_ref() == Some(s))
            });
        if !matches {
            let want: Vec<String> = expected
                .iter()
                .map(|w| format!("{} ({})", w.title, w.status.as_deref().unwrap_or("any status")))
                .collect();
            failures.push(format!("{label}: expected tool calls {want:?}, got {got:?}"));
        }
    }

    if seen.permissions.len() != prompt.permissions.len() {
        failures.push(format!(
            "{label}: expected {} permission request(s), got {}",
            prompt.permissions.len(),
            seen.permissions.len()
        ));
    }
    for (i, record) in seen.permissions.iter().enumerate() {
        if let Some(problem) = &record.problem {
            failures.push(format!("{label}: permission request {}: {problem}", i + 1));
        }
    }
    failures
}

/// Every way the workspace misses the `[[verify]]` checks.
pub fn verify_workspace(checks: &[Verify], workspace: &Path) -> Vec<String> {
    let mut failures = Vec::new();
    for check in checks {
        let path = workspace.join(&check.path);
        let exists = path.exists();
        if let Some(want) = check.exists
            && want != exists
        {
            let state = if exists { "exists" } else { "does not exist" };
            failures.push(format!("verify {}: expected exists = {want}, but it {state}", check.path));
            continue;
        }
        if check.equals.is_none() && check.contains.is_none() {
            continue;
        }
        let body = match std::fs::read_to_string(&path) {
            Ok(body) => body,
            Err(error) => {
                failures.push(format!("verify {}: cannot read it: {error}", check.path));
                continue;
            }
        };
        if let Some(want) = &check.equals
            && &body != want
        {
            failures.push(format!("verify {}: expected contents {want:?}, got {body:?}", check.path));
        }
        if let Some(needle) = &check.contains
            && !body.contains(needle.as_str())
        {
            failures.push(format!("verify {}: does not contain {needle:?}; it holds {body:?}", check.path));
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::Scenario;
    use serde_json::json;

    fn prompt(extra: &str) -> Prompt {
        let text = format!("description = \"d\"\n[[prompt]]\ntext = \"go\"\n{extra}\n");
        Scenario::parse(&text, "test").unwrap().prompt.remove(0)
    }

    fn chunk(text: &str) -> Value {
        json!({"update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}})
    }

    #[test]
    fn a_prompt_that_meets_every_expectation_has_no_failures() {
        let p = prompt("text_contains = [\"hello\"]\ntool_calls = []");
        let response = json!({"stopReason": "end_turn"});
        let updates = [chunk("hello there")];
        let seen = Seen { response: &response, updates: &updates, permissions: &[] };
        assert_eq!(check_prompt("p", &p, &seen), Vec::<String>::new());
    }

    #[test]
    fn each_missed_expectation_is_named() {
        let p = prompt("text_contains = [\"absent\"]\ntool_calls = [{ title = \"write\" }]\npermissions = [\"allow\"]");
        let response = json!({"stopReason": "cancelled"});
        let updates = [chunk("hello")];
        let seen = Seen { response: &response, updates: &updates, permissions: &[] };
        let failures = check_prompt("p", &p, &seen).join("\n");
        for needle in ["stopReason", "\"absent\"", "tool calls", "1 permission request(s), got 0"] {
            assert!(failures.contains(needle), "{needle} missing from:\n{failures}");
        }
    }

    #[test]
    fn a_failed_follow_up_turn_fails_the_prompt_even_when_the_text_matched() {
        let p = prompt("text_contains = [\"done\"]");
        let response = json!({"stopReason": "end_turn"});
        let updates = [chunk("done"), chunk("stream error: Model turn panicked; execution stopped")];
        let seen = Seen { response: &response, updates: &updates, permissions: &[] };
        let failures = check_prompt("p", &p, &seen);
        assert_eq!(failures.len(), 1, "{failures:#?}");
        assert!(failures[0].contains("a model turn failed"), "{failures:#?}");
    }

    #[test]
    fn a_tool_call_status_mismatch_fails() {
        let p = prompt("tool_calls = [{ title = \"write\", status = \"completed\" }]");
        let response = json!({"stopReason": "end_turn"});
        let updates = [json!({"update": {"sessionUpdate": "tool_call", "toolCallId": "a", "title": "write", "status": "failed"}})];
        let seen = Seen { response: &response, updates: &updates, permissions: &[] };
        assert_eq!(check_prompt("p", &p, &seen).len(), 1);
    }

    #[test]
    fn the_default_gate_keeps_the_shipped_tiers_and_drops_the_classifier() {
        let policy: toml::Table = default_gate().unwrap().parse().unwrap();
        assert!(!policy.contains_key("classifier"), "{policy:#?}");
        let shipped: toml::Table = SHIPPED_GATE.parse().unwrap();
        assert_eq!(policy.get("global"), shipped.get("global"));
        assert_eq!(policy.get("context_type"), shipped.get("context_type"));
    }

    #[test]
    fn workspace_checks_report_each_miss() {
        let dir = PathBuf::from(crate::DEFAULT_SCRATCH).join(format!("unit-verify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "alpha\n").unwrap();
        let checks: Vec<Verify> = Scenario::parse(
            r#"
description = "d"
[[prompt]]
text = "go"
[[verify]]
path = "a.txt"
equals = "alpha\n"
[[verify]]
path = "a.txt"
contains = "beta"
[[verify]]
path = "missing.txt"
exists = true
[[verify]]
path = "a.txt"
exists = false
"#,
            "verify",
        )
        .unwrap()
        .verify;
        let failures = verify_workspace(&checks, &dir);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(failures.len(), 3, "{failures:#?}");
        assert!(failures[0].contains("beta"));
        assert!(failures[1].contains("missing.txt"));
        assert!(failures[2].contains("exists = false"));
    }
}
