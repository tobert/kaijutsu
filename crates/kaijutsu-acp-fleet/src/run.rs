//! Run one scenario against an ACP agent and judge it.
//!
//! Each run gets a fresh scratch directory holding the workspace (the session
//! cwd and the agent's launch directory), the fleet files the agent reads (the
//! mock model script, the gate policy, and any rc overlay), and, in host mode,
//! the agent's `TMPDIR`. The agent is a fresh process per scenario, so no state
//! crosses between scenarios.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use serde_json::Value;

use crate::client::{self, AcpClient, AgentCommand, PermissionAnswer, PermissionPolicy, PermissionRecord};
use crate::container;
use crate::scenario::{Answer, Cancel, KnownGap, Mode, OnHold, Prompt, Scenario, Verify, workspace_relative};
use crate::shape::{self, PromptWire, Transcript};

/// Harbor-shape invariants the agent breaks wherever they apply, each with
/// the finding in `docs/issues.md`, "ACP fleet: what stays open" that
/// records it. A failure of one is excused and the scenario reports `GAP`.
/// A scenario that exercises one with no failure fails: the finding no
/// longer reproduces, so its line here must go.
pub const SHAPE_GAPS: &[(&str, &str)] = &[];

/// How long the update stream must stay silent after a prompt's response
/// before the prompt is judged. A model's turn holds on its own ask, so its
/// permission requests arrive before the response; the wait catches what
/// still comes later, such as a follow-up turn from an ask that did not hold
/// the turn, or a request a prompt expected not to raise.
pub const SETTLE: Duration = Duration::from_secs(3);

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
    /// The findings the scenario reproduces: its own known gap, and any
    /// Harbor-shape gap in [`SHAPE_GAPS`] it exercised.
    pub known_gaps: Vec<String>,
    /// Failures the known gap accounts for; they do not fail the scenario.
    pub excused: Vec<String>,
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
            known_gaps: Vec::new(),
            excused: Vec::new(),
            elapsed: started.elapsed(),
            stderr_tail: String::new(),
            kept: None,
        },
    }
}

pub fn run_scenario(name: &str, scenario: &Scenario, config: &RunConfig) -> Outcome {
    let started = Instant::now();
    let mut failures = Vec::new();
    // Errors that stopped the run. A known gap never excuses one.
    let mut errors = Vec::new();
    let mut stderr_tail = String::new();
    let mut transcript = None;
    let scratch = match Scratch::create(&config.scratch_root, name) {
        Ok(scratch) => Some(scratch),
        Err(error) => {
            errors.push(format!("{error:#}"));
            None
        }
    };
    if let Some(scratch) = &scratch
        && let Err(error) = drive(scenario, scratch, config, &mut failures, &mut stderr_tail, &mut transcript)
    {
        errors.push(format!("{error:#}"));
    }
    let kept = match scratch {
        Some(scratch) if config.keep => Some(scratch.root.clone()),
        Some(scratch) => {
            if let Err(error) = std::fs::remove_dir_all(&scratch.root) {
                errors.push(format!("remove the scratch directory {}: {error}", scratch.root.display()));
            }
            None
        }
        None => None,
    };
    let mut shape_excused = Vec::new();
    let mut known_gaps: Vec<String> = scenario.known_gap.iter().map(|g| g.finding.clone()).collect();
    if errors.is_empty()
        && let Some(transcript) = &transcript
    {
        let shape = judge_shape(&shape::check(transcript), SHAPE_GAPS);
        failures.extend(shape.failures);
        shape_excused = shape.excused;
        known_gaps.extend(shape.gaps);
    }
    let Judged { mut failures, mut excused } = if errors.is_empty() {
        judge_gap(scenario.known_gap.as_ref(), failures)
    } else {
        Judged { failures, excused: Vec::new() }
    };
    excused.extend(shape_excused);
    failures.extend(errors);
    if failures.is_empty() {
        stderr_tail.clear();
    }
    Outcome {
        name: name.to_string(),
        failures,
        known_gaps,
        excused,
        elapsed: started.elapsed(),
        stderr_tail,
        kept,
    }
}

/// The Harbor-shape verdict after [`SHAPE_GAPS`] is applied.
pub struct ShapeJudged {
    /// Failures no gap excuses, and gaps that no longer reproduce.
    pub failures: Vec<String>,
    pub excused: Vec<String>,
    /// The findings whose failures were excused.
    pub gaps: Vec<String>,
}

/// Sort Harbor-shape failures by whether a gap in `gaps`, as
/// `(finding, invariant)`, accounts for them. A gap whose invariant was
/// exercised with no failure is itself a failure.
pub fn judge_shape(checks: &[shape::Check], gaps: &[(&str, &str)]) -> ShapeJudged {
    let mut judged = ShapeJudged { failures: Vec::new(), excused: Vec::new(), gaps: Vec::new() };
    for check in checks {
        let gap = gaps.iter().find(|(_, invariant)| *invariant == check.invariant);
        match gap {
            None => judged.failures.extend(check.failures.iter().cloned()),
            Some((finding, _)) if !check.failures.is_empty() => {
                judged.excused.extend(check.failures.iter().cloned());
                judged.gaps.push(finding.to_string());
            }
            Some((finding, invariant)) if check.exercised => judged.failures.push(format!(
                "known shape gap {finding} ({invariant}) no longer reproduces: this scenario exercised it and it \
                 held. If {finding} is fixed, remove it from SHAPE_GAPS in crates/kaijutsu-acp-fleet/src/run.rs \
                 and its line in docs/issues.md"
            )),
            Some(_) => {}
        }
    }
    judged
}

/// Failures sorted by whether a known gap accounts for them.
pub struct Judged {
    pub failures: Vec<String>,
    pub excused: Vec<String>,
}

/// Excuse the failures `gap` names. A gap that caused no failure is itself a
/// failure: the finding no longer reproduces, so the marker must go.
pub fn judge_gap(gap: Option<&KnownGap>, failures: Vec<String>) -> Judged {
    let Some(gap) = gap else {
        return Judged { failures, excused: Vec::new() };
    };
    let (excused, mut failures): (Vec<String>, Vec<String>) =
        failures.into_iter().partition(|f| gap.fails.iter().any(|needle| f.contains(needle.as_str())));
    if excused.is_empty() {
        failures.push(format!(
            "known gap {} no longer reproduces: none of {:?} failed. If {} is fixed, remove [known_gap] \
             and its line in docs/issues.md",
            gap.finding, gap.fails, gap.finding
        ));
    }
    Judged { failures, excused }
}

struct Scratch {
    root: PathBuf,
    /// The run's unique directory name, reused as its container names.
    id: String,
    workspace: PathBuf,
    /// What the agent reads: `mock/`, `gate.toml`, and `rc/`.
    fleet: PathBuf,
    tmp: PathBuf,
    /// The agent's named state directory, used when a prompt edits the
    /// kernel's config. The agent creates it.
    state: PathBuf,
}

impl Scratch {
    fn create(scratch_root: &Path, name: &str) -> Result<Self> {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .context("the clock reads before the epoch")?
            .as_nanos();
        let id = format!("{name}-{}-{stamp}", std::process::id());
        let root = scratch_root.join(&id);
        let scratch = Self {
            workspace: root.join("workspace"),
            fleet: root.join("fleet"),
            tmp: root.join("tmp"),
            state: root.join("state"),
            root,
            id,
        };
        for dir in [&scratch.workspace, &scratch.fleet.join("mock"), &scratch.tmp] {
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
    transcript: &mut Option<Transcript>,
) -> Result<()> {
    for (path, body) in &scenario.files {
        let target = scratch.workspace.join(workspace_relative(path)?);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        std::fs::write(&target, body).with_context(|| format!("seed {}", target.display()))?;
    }
    let script = serde_json::to_string_pretty(&scenario.mock_script()?)?;
    std::fs::write(scratch.fleet.join("mock").join(format!("{MOCK_MODEL}.json")), script)
        .context("write the mock script")?;

    if scenario.mode == Mode::Contained {
        container::preflight()?;
    }
    std::fs::write(scratch.fleet.join("gate.toml"), gate_policy(scenario)).context("write the gate policy")?;
    let overlay = &scenario.rc;
    for (path, body) in overlay {
        let target = scratch.fleet.join("rc").join(workspace_relative(path)?);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        std::fs::write(&target, body).with_context(|| format!("write {}", target.display()))?;
    }

    let (command, session_cwd) = match scenario.mode {
        Mode::Host => {
            let mut command = AgentCommand::new(&config.agent)
                .arg("--backend-kind")
                .arg("mock")
                .arg("--model")
                .arg(MOCK_MODEL)
                .arg("--gate-config")
                .arg(scratch.fleet.join("gate.toml"))
                .env("KJ_MOCK_SCRIPT_DIR", scratch.fleet.join("mock"))
                .env("TMPDIR", &scratch.tmp)
                .env("RUST_LOG", "info")
                .cwd(&scratch.workspace);
            if !overlay.is_empty() {
                command = command.arg("--rc-overlay").arg(scratch.fleet.join("rc"));
            }
            if scenario.prompt.iter().any(|p| p.gate.is_some()) {
                command = command.arg("--state-dir").arg(&scratch.state);
            }
            (command, scratch.workspace.clone())
        }
        Mode::Contained => {
            let gate = format!("{}/gate.toml", container::FLEET);
            let rc = format!("{}/rc", container::FLEET);
            let mut args = vec!["--backend-kind", "mock", "--model", MOCK_MODEL, "--gate-config", &gate];
            if !overlay.is_empty() {
                args.extend(["--rc-overlay", &rc]);
            }
            let command =
                container::agent_command(&config.agent, &scratch.workspace, &scratch.fleet, &scratch.id, &args);
            (command, PathBuf::from(container::WORKSPACE))
        }
    };
    let session_cwd = match &scenario.session_cwd {
        Some(relative) => {
            let relative = workspace_relative(relative)?;
            let host = scratch.workspace.join(&relative);
            std::fs::create_dir_all(&host).with_context(|| format!("create the session cwd {}", host.display()))?;
            session_cwd.join(relative)
        }
        None => session_cwd,
    };

    let mut agent = AcpClient::spawn(&command, config.timeout)?;
    agent.set_trace(config.trace);
    let result = converse(&mut agent, scenario, &session_cwd, scratch, config.timeout, failures)
        .map(|wire| *transcript = Some(wire));
    // Close stdin even after a failed run so the agent removes its own
    // temporary state; the scratch directory is removed after this.
    let shutdown = agent.shutdown(Duration::from_secs(60));
    *stderr_tail = agent.stderr_tail();
    drop(agent);
    if scenario.mode == Mode::Contained {
        container::remove(&scratch.id);
    }
    result?;
    match shutdown {
        Ok(status) if !status.success() => failures.push(format!("the agent exited with {status} after stdin closed")),
        Ok(_) => {}
        Err(error) => failures.push(format!("{error:#}")),
    }
    failures.extend(verify_workspace(&scenario.verify, &scratch.workspace));
    for (n, check) in scenario.verify.iter().enumerate() {
        if let Some(script) = &check.script {
            let name = format!("{}-verify-{}", scratch.id, n + 1);
            let run = container::run_script(&scratch.workspace, script, &name, config.timeout)?;
            if !run.success {
                failures.push(format!(
                    "verify script {}: exited {}\n{}",
                    n + 1,
                    run.code.map_or("on a signal".to_string(), |c| c.to_string()),
                    run.output.trim_end()
                ));
            }
        }
    }
    Ok(())
}

/// The gate policy a contained scenario runs under when it names none: every
/// statement no key covers runs, with no ask.
pub const YOLO_GATE: &str = "\
# Contained ACP fleet (docs/acp-fleet.md): a throwaway kernel in a container.
[global]
uncovered = \"allow\"
";

/// The gate policy for a run: the scenario's, or the mode's default.
fn gate_policy(scenario: &Scenario) -> String {
    match (&scenario.gate, scenario.mode) {
        (Some(gate), _) => gate.clone(),
        (None, Mode::Host) => SHIPPED_GATE.to_string(),
        (None, Mode::Contained) => YOLO_GATE.to_string(),
    }
}

/// `workspace` is the session cwd as the agent sees it.
fn converse(
    agent: &mut AcpClient,
    scenario: &Scenario,
    workspace: &Path,
    scratch: &Scratch,
    timeout: Duration,
    failures: &mut Vec<String>,
) -> Result<Transcript> {
    let init = agent.initialize()?;
    if init.get("protocolVersion").and_then(Value::as_u64) != Some(1) {
        failures.push(format!("initialize: expected protocolVersion 1, got {init}"));
    }
    let session_new = agent.open_session(workspace)?;
    let session = session_new["sessionId"].as_str().context("session/new returned no sessionId")?.to_string();
    let mut prompts = Vec::new();
    for (n, prompt) in scenario.prompt.iter().enumerate() {
        let label = format!("prompt {}", n + 1);
        let answers: VecDeque<PermissionAnswer> = prompt.permissions.iter().map(|&a| a.into()).collect();
        agent.set_permission_policy(PermissionPolicy::Queue(answers));
        let sent = agent.arrival();
        let updates_before = sent.updates;
        let permissions_before = sent.permissions;
        if let Some(gate) = &prompt.gate {
            let path = scratch.state.join("config").join("kernel").join("gate.toml");
            std::fs::write(&path, gate).with_context(|| format!("{label}: replace {}", path.display()))?;
        }
        let id = agent.start_prompt(&session, &prompt.text).with_context(|| label.clone())?;
        if let Some(cancel) = &prompt.cancel {
            cancel_mid_call(agent, &session, id, cancel, &scratch.workspace, timeout).with_context(|| label.clone())?;
        }
        release_holds(agent, id, prompt, permissions_before, &scratch.workspace, timeout)
            .with_context(|| label.clone())?;
        let (response, answered) = agent.wait_response_at(id, "session/prompt").with_context(|| label.clone())?;
        agent.pump_until_quiet(SETTLE, timeout, &label)?;
        let seen = Seen {
            response: &response,
            updates: &agent.updates()[updates_before..],
            answered: answered.updates - updates_before,
            permissions: &agent.permissions()[permissions_before..],
        };
        failures.extend(check_prompt(&label, prompt, &seen));
        prompts.push(PromptWire { label, sent, response, answered, ended: agent.arrival() });
    }
    failures.extend(answers_shown_as_tool_calls(agent.updates()));
    Ok(Transcript {
        initialize: init,
        session_new,
        updates: agent.updates().to_vec(),
        permissions: agent.permissions().to_vec(),
        prompts,
    })
}

/// Answer the requests `prompt` holds while its turn waits on them. Once
/// every held request has arrived, run `on_hold`, then send each `release`
/// answer, waiting after each one but the last until the ledger has taken
/// it up: the agent logs [`ASK_ANSWER_LOGGED`]. The next answer is then
/// decided under whatever the previous one changed, such as a remembered
/// rule.
fn release_holds(
    agent: &mut AcpClient,
    request: i64,
    prompt: &Prompt,
    permissions_before: usize,
    workspace_host: &Path,
    timeout: Duration,
) -> Result<()> {
    let holds = prompt.permissions.iter().filter(|a| **a == Answer::Hold).count();
    if holds == 0 {
        return Ok(());
    }
    let what = format!("{holds} held permission request(s)");
    agent.pump_until_state(Some(request), &what, timeout, |a| a.held_count(permissions_before) >= holds)?;
    if let Some(OnHold { write, wait_for }) = &prompt.on_hold {
        if let Some(write) = write {
            let target = workspace_host.join(workspace_relative(write)?);
            std::fs::write(&target, "").with_context(|| format!("on_hold: write {}", target.display()))?;
        }
        if let Some(wait_for) = wait_for {
            let target = workspace_host.join(workspace_relative(wait_for)?);
            let what = format!("on_hold: {wait_for} to exist");
            agent.pump_until_state(Some(request), &what, timeout, |_| target.exists())?;
        }
    }
    for (n, answer) in prompt.release.iter().enumerate() {
        let mark = agent.stderr().len();
        agent.release_held(permissions_before, (*answer).into()).context("release a held permission request")?;
        if n + 1 < prompt.release.len() {
            let what = format!("the ledger to take up release answer {}", n + 1);
            agent.pump_until_state(Some(request), &what, timeout, |a| {
                a.stderr().get(mark..).is_some_and(|tail| tail.contains(ASK_ANSWER_LOGGED))
            })?;
        }
    }
    Ok(())
}

/// What the ACP bridge logs each time the ledger takes up a permission
/// answer, recorded or refused (`kaijutsu_acp::permission::ASK_ANSWER_LOGGED`).
/// An answer authors no block, so the update stream never shows it.
pub const ASK_ANSWER_LOGGED: &str = "ask_answer=";

/// A failure for every tool call in `updates` that shows a permission
/// answer: a `kj` call that names the ledger, or any call whose input runs
/// `ledger allow` or `ledger deny`. The ledger row is the record of an
/// answer, and answering authors no block in any transcript
/// (`docs/acp.md`, "Permission asks, ledger-driven").
pub fn answers_shown_as_tool_calls(updates: &[Value]) -> Vec<String> {
    client::tool_calls(updates)
        .iter()
        .filter(|call| {
            let words = call.raw_input.as_ref().map(input_words).unwrap_or_default();
            let names_ledger = words.iter().any(|w| w == "ledger");
            let answers = words.windows(2).any(|pair| pair[0] == "ledger" && (pair[1] == "allow" || pair[1] == "deny"));
            (call.title == "kj" && names_ledger) || answers
        })
        .map(|call| {
            format!(
                "a permission answer appeared in the update stream as a tool call: {:?} {}; an answer is recorded \
                 in the ledger and authors no block",
                call.title,
                call.raw_input.as_ref().map_or_else(String::new, Value::to_string)
            )
        })
        .collect()
}

/// Every whitespace-separated word in the strings of `input`, in order.
fn input_words(input: &Value) -> Vec<String> {
    fn collect(value: &Value, words: &mut Vec<String>) {
        match value {
            Value::String(text) => words.extend(text.split_whitespace().map(str::to_string)),
            Value::Array(items) => items.iter().for_each(|item| collect(item, words)),
            Value::Object(fields) => fields.values().for_each(|field| collect(field, words)),
            _ => {}
        }
    }
    let mut words = Vec::new();
    collect(input, &mut words);
    words
}

/// What the solo agent's kernel logs once an interrupt has marked a running
/// turn to stop. `session/cancel` is a notification, so ACP gives the client
/// no acknowledgment to wait on. The bridge's own "soft interrupt sent" line
/// does not serve: it also appears when no turn was running.
pub const CANCEL_CONFIRMED: &str = "turn_interrupted=true";

/// What the kernel logs when an interrupt found no running turn.
pub const CANCEL_FOUND_NOTHING: &str = "turn_interrupted=false";

/// Wait for `cancel.after_tool_call` to be in progress, send
/// `session/cancel`, wait for the agent to confirm it, then write
/// `cancel.release` so a command waiting on it can finish.
fn cancel_mid_call(
    agent: &mut AcpClient,
    session: &str,
    prompt: i64,
    cancel: &Cancel,
    workspace_host: &Path,
    timeout: Duration,
) -> Result<()> {
    let title = cancel.after_tool_call.as_str();
    let what = format!("a {title:?} tool call in progress, to cancel");
    agent.pump_until(Some(prompt), &what, timeout, |updates| {
        client::tool_calls(updates).iter().any(|c| c.title == title && c.status.as_deref() == Some("in_progress"))
    })?;
    let mark = agent.stderr().len();
    agent.cancel(session)?;
    let deadline = Instant::now() + timeout;
    loop {
        let since = agent.stderr().get(mark..).map(str::to_string).unwrap_or_default();
        if since.contains(CANCEL_CONFIRMED) {
            break;
        }
        if since.contains(CANCEL_FOUND_NOTHING) {
            bail!("session/cancel found no running turn while {title:?} was in progress\n--- agent stderr (tail) ---\n{}", agent.stderr_tail());
        }
        if Instant::now() > deadline {
            bail!("the agent did not confirm session/cancel within {timeout:?}\n--- agent stderr (tail) ---\n{}", agent.stderr_tail());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if let Some(release) = &cancel.release {
        let target = workspace_host.join(workspace_relative(release)?);
        std::fs::write(&target, "").with_context(|| format!("write the release file {}", target.display()))?;
    }
    Ok(())
}

/// The gate policy a host scenario runs under when it names none.
const SHIPPED_GATE: &str = include_str!("../../../assets/defaults/gate.toml");

/// The text the ACP bridge sends as an agent message when a model turn fails
/// outside a prompt, such as a follow-up turn started by a permission answer.
/// The update stream carries no structured signal for that case.
pub const FAILED_TURN_TEXT: &str = "stream error:";

/// What one prompt produced on the wire: everything from sending it until the
/// runner stopped waiting for its permission requests and expected text.
pub struct Seen<'a> {
    pub response: &'a Value,
    pub updates: &'a [Value],
    /// How many of `updates` arrived before the response.
    pub answered: usize,
    pub permissions: &'a [PermissionRecord],
}

/// A call's ACP kind. ACP v1 reads an omitted `kind` as `other`.
fn acp_kind(call: &client::ToolCallSeen) -> &str {
    call.kind.as_deref().unwrap_or("other")
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
        let got: Vec<String> = calls
            .iter()
            .map(|c| {
                let kind = acp_kind(c);
                format!("{} ({}, kind {kind})", c.title, c.status.as_deref().unwrap_or("no status"))
            })
            .collect();
        let matches = calls.len() == expected.len()
            && calls.iter().zip(expected).all(|(call, want)| {
                call.title == want.title
                    && want.status.as_ref().is_none_or(|s| call.status.as_ref() == Some(s))
                    && want.kind.as_ref().is_none_or(|k| acp_kind(call) == k)
                    && want.output_contains.as_ref().is_none_or(|o| o.all().iter().all(|n| call.output.contains(n.as_str())))
            });
        if !matches {
            let want: Vec<String> = expected
                .iter()
                .map(|w| {
                    let output = w.output_contains.as_ref().map_or(String::new(), |o| format!(", output contains {:?}", o.all()));
                    let kind = w.kind.as_ref().map_or(String::new(), |k| format!(", kind {k}"));
                    format!("{} ({}{kind}{output})", w.title, w.status.as_deref().unwrap_or("any status"))
                })
                .collect();
            let outputs: Vec<&str> = calls.iter().map(|c| c.output.as_str()).collect();
            failures.push(format!("{label}: expected tool calls {want:?}, got {got:?} with outputs {outputs:?}"));
        }
    }

    if prompt.reports_cost {
        let cost = seen.updates[..seen.answered.min(seen.updates.len())]
            .iter()
            .rev()
            .filter(|u| u.pointer("/update/sessionUpdate").and_then(Value::as_str) == Some("usage_update"))
            .find_map(|u| u.pointer("/update/cost"));
        let usd = cost.is_some_and(|c| {
            c.get("currency").and_then(Value::as_str).is_some_and(|x| x.eq_ignore_ascii_case("USD"))
                && c.get("amount").is_some_and(Value::is_number)
        });
        if !usd {
            let usage = seen
                .updates
                .iter()
                .filter(|u| u.pointer("/update/sessionUpdate").and_then(Value::as_str) == Some("usage_update"))
                .count();
            failures.push(format!(
                "{label}: reports_cost: no usage_update before the response carries a USD cost; \
                 {usage} usage_update(s) arrived, last cost {cost:?}"
            ));
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
        let want = prompt.permission_titles.as_ref().and_then(|titles| titles.get(i));
        let title = record.params.pointer("/toolCall/title").and_then(Value::as_str).unwrap_or("");
        if let Some(want) = want
            && !title.contains(want.as_str())
        {
            failures.push(format!("{label}: permission request {} title does not contain {want:?}; it was {title:?}", i + 1));
        }
        if let Some(want) = prompt.permission_tool_calls.as_ref().and_then(|calls| calls.get(i)) {
            let id = record.params.pointer("/toolCall/toolCallId").and_then(Value::as_str).unwrap_or("");
            let announced = seen.updates.iter().find(|u| {
                u.pointer("/update/sessionUpdate").and_then(Value::as_str) == Some("tool_call")
                    && u.pointer("/update/toolCallId").and_then(Value::as_str) == Some(id)
            });
            match announced.map(|u| u.pointer("/update/title").and_then(Value::as_str).unwrap_or("")) {
                Some(title) if title == want => {}
                Some(title) => failures.push(format!(
                    "{label}: permission request {} names tool call {id:?}, titled {title:?}, not {want:?}",
                    i + 1
                )),
                None => failures.push(format!(
                    "{label}: permission request {} names tool call {id:?}, which this prompt never announced",
                    i + 1
                )),
            }
        }
    }
    failures
}

/// Every way the workspace misses the `[[verify]]` checks.
pub fn verify_workspace(checks: &[Verify], workspace: &Path) -> Vec<String> {
    let mut failures = Vec::new();
    for check in checks {
        let Some(relative) = &check.path else { continue };
        let path = workspace.join(relative);
        let exists = path.exists();
        if let Some(want) = check.exists
            && want != exists
        {
            let state = if exists { "exists" } else { "does not exist" };
            failures.push(format!("verify {relative}: expected exists = {want}, but it {state}"));
            continue;
        }
        if check.equals.is_none() && check.contains.is_none() {
            continue;
        }
        let body = match std::fs::read_to_string(&path) {
            Ok(body) => body,
            Err(error) => {
                failures.push(format!("verify {relative}: cannot read it: {error}"));
                continue;
            }
        };
        if let Some(want) = &check.equals
            && &body != want
        {
            failures.push(format!("verify {relative}: expected contents {want:?}, got {body:?}"));
        }
        if let Some(needle) = &check.contains
            && !body.contains(needle.as_str())
        {
            failures.push(format!("verify {relative}: does not contain {needle:?}; it holds {body:?}"));
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::{KnownGap, Scenario};
    use serde_json::json;

    fn prompt(extra: &str) -> Prompt {
        let text = format!("description = \"d\"\n[[prompt]]\ntext = \"go\"\n{extra}\n");
        Scenario::parse(&text, "test").unwrap().prompt.remove(0)
    }

    fn chunk(text: &str) -> Value {
        json!({"update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}})
    }

    fn call(id: &str, title: &str, input: Value) -> Value {
        json!({"update": {"sessionUpdate": "tool_call", "toolCallId": id, "title": title, "status": "completed", "rawInput": input}})
    }

    #[test]
    fn a_permission_answer_shown_as_a_tool_call_fails_the_run() {
        let updates = vec![
            call("a", "kj", json!({"argv": ["ledger", "allow", "req-1"]})),
            call("b", "shell_write", json!({"command": "kj ledger deny req-2 --cancelled"})),
            call("c", "kj", json!({"argv": ["ledger", "show", "req-1"]})),
        ];
        let failures = answers_shown_as_tool_calls(&updates);
        assert_eq!(failures.len(), 3, "{failures:#?}");
        assert!(failures[0].contains("\"kj\""), "{}", failures[0]);
    }

    #[test]
    fn a_model_reading_the_ledger_through_its_shell_is_not_an_answer() {
        let updates = vec![
            call("a", "shell_write", json!({"command": "kj ledger show \"$(kj ledger list --history | jq -r '.[0]')\""})),
            call("b", "shell_write", json!({"command": "kj ledger list --history --status denied"})),
            call("c", "kj", json!({"argv": ["context", "list"]})),
            call("d", "done", json!({"status": "done", "feedback": "allow deny ledger"})),
        ];
        assert_eq!(answers_shown_as_tool_calls(&updates), Vec::<String>::new());
    }

    #[test]
    fn a_prompt_that_meets_every_expectation_has_no_failures() {
        let p = prompt("text_contains = [\"hello\"]\ntool_calls = []");
        let response = json!({"stopReason": "end_turn"});
        let updates = [chunk("hello there")];
        let seen = Seen { response: &response, updates: &updates, answered: updates.len(), permissions: &[] };
        assert_eq!(check_prompt("p", &p, &seen), Vec::<String>::new());
    }

    #[test]
    fn each_missed_expectation_is_named() {
        let p = prompt("text_contains = [\"absent\"]\ntool_calls = [{ title = \"write\" }]\npermissions = [\"allow\"]");
        let response = json!({"stopReason": "cancelled"});
        let updates = [chunk("hello")];
        let seen = Seen { response: &response, updates: &updates, answered: updates.len(), permissions: &[] };
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
        let seen = Seen { response: &response, updates: &updates, answered: updates.len(), permissions: &[] };
        let failures = check_prompt("p", &p, &seen);
        assert_eq!(failures.len(), 1, "{failures:#?}");
        assert!(failures[0].contains("a model turn failed"), "{failures:#?}");
    }

    fn gap(fails: &[&str]) -> KnownGap {
        KnownGap { finding: "F2".into(), fails: fails.iter().map(|f| f.to_string()).collect() }
    }

    #[test]
    fn a_gap_that_reproduces_excuses_only_its_own_failures() {
        let judged = judge_gap(Some(&gap(&["verify escaped"])), vec!["verify escaped: expected exists = false".into()]);
        assert_eq!(judged.failures, Vec::<String>::new());
        assert_eq!(judged.excused.len(), 1);
    }

    #[test]
    fn a_gap_that_no_longer_reproduces_fails_and_says_to_remove_the_marker() {
        let judged = judge_gap(Some(&gap(&["verify escaped"])), Vec::new());
        assert_eq!(judged.failures.len(), 1, "{:?}", judged.failures);
        assert!(judged.failures[0].contains("F2") && judged.failures[0].contains("known_gap"), "{:?}", judged.failures);
    }

    #[test]
    fn a_failure_the_gap_does_not_name_still_fails() {
        let judged = judge_gap(
            Some(&gap(&["verify escaped"])),
            vec!["verify escaped: expected exists = false".into(), "prompt 1: expected stopReason".into()],
        );
        assert_eq!(judged.failures, vec!["prompt 1: expected stopReason".to_string()]);
    }

    #[test]
    fn with_no_gap_every_failure_stands() {
        let judged = judge_gap(None, vec!["a".into()]);
        assert_eq!(judged.failures, vec!["a".to_string()]);
        assert!(judged.excused.is_empty());
    }

    fn shape_check(invariant: &'static str, exercised: bool, failures: &[&str]) -> shape::Check {
        shape::Check { invariant, exercised, failures: failures.iter().map(|f| f.to_string()).collect() }
    }

    #[test]
    fn a_shape_gap_excuses_its_invariant_and_names_its_finding() {
        let checks = [
            shape_check(shape::PERMISSION_TOOL_CALL, true, &["harbor shape permission-tool-call: x"]),
            shape_check(shape::STOP_REASON, true, &["harbor shape stop-reason: y"]),
        ];
        let judged = judge_shape(&checks, &[("H1", shape::PERMISSION_TOOL_CALL)]);
        assert_eq!(judged.failures, vec!["harbor shape stop-reason: y".to_string()]);
        assert_eq!(judged.excused, vec!["harbor shape permission-tool-call: x".to_string()]);
        assert_eq!(judged.gaps, vec!["H1".to_string()]);
    }

    #[test]
    fn a_shape_gap_that_held_where_it_applied_fails() {
        let checks = [shape_check(shape::PERMISSION_TOOL_CALL, true, &[])];
        let judged = judge_shape(&checks, &[("H1", shape::PERMISSION_TOOL_CALL)]);
        assert_eq!(judged.failures.len(), 1, "{:?}", judged.failures);
        assert!(judged.failures[0].contains("H1") && judged.failures[0].contains("SHAPE_GAPS"), "{:?}", judged.failures);
        assert!(judged.gaps.is_empty());
    }

    #[test]
    fn a_shape_gap_with_nothing_to_check_says_nothing() {
        let checks = [shape_check(shape::PERMISSION_TOOL_CALL, false, &[])];
        let judged = judge_shape(&checks, &[("H1", shape::PERMISSION_TOOL_CALL)]);
        assert!(judged.failures.is_empty() && judged.excused.is_empty() && judged.gaps.is_empty());
    }

    #[test]
    fn a_missing_cost_fails_reports_cost() {
        let p = prompt("reports_cost = true");
        let response = json!({"stopReason": "end_turn"});
        let usage = |cost: Value| json!({"update": {"sessionUpdate": "usage_update", "used": 1, "size": 9, "cost": cost}});
        let updates = [usage(Value::Null)];
        let seen = Seen { response: &response, updates: &updates, answered: 1, permissions: &[] };
        assert!(check_prompt("p", &p, &seen).join("\n").contains("reports_cost"));
        let updates = [usage(json!({"amount": 0.5, "currency": "USD"}))];
        let seen = Seen { response: &response, updates: &updates, answered: 1, permissions: &[] };
        assert_eq!(check_prompt("p", &p, &seen), Vec::<String>::new());
        let late = Seen { response: &response, updates: &updates, answered: 0, permissions: &[] };
        assert_eq!(check_prompt("p", &p, &late).len(), 1, "a cost after the response is one Harbor never reads");
    }

    #[test]
    fn a_permission_request_must_name_the_expected_tool_call() {
        let p = prompt("permissions = [\"allow\"]\npermission_tool_calls = [\"shell_write\"]");
        let response = json!({"stopReason": "end_turn"});
        let updates = [
            json!({"update": {"sessionUpdate": "tool_call", "toolCallId": "model", "title": "shell_write"}}),
            json!({"update": {"sessionUpdate": "tool_call", "toolCallId": "operation", "title": "shell"}}),
        ];
        let request = |id: &str| [PermissionRecord {
            params: json!({"toolCall": {"toolCallId": id, "title": "mkdir x"}}),
            answer: Some(PermissionAnswer::Allow),
            option_id: Some("allow".into()),
            problem: None,
            updates_seen: 2,
        }];
        let judge = |id: &str| {
            let permissions = request(id);
            let seen = Seen { response: &response, updates: &updates, answered: updates.len(), permissions: &permissions };
            check_prompt("p", &p, &seen).join("\n")
        };
        assert_eq!(judge("model"), "");
        assert!(judge("operation").contains("titled \"shell\", not \"shell_write\""), "{}", judge("operation"));
        assert!(judge("ask-1").contains("never announced"), "{}", judge("ask-1"));
        let uneven = format!("description = \"d\"\n[[prompt]]\ntext = \"go\"\npermission_tool_calls = [\"shell_write\"]\n");
        let error = format!("{:#}", Scenario::parse(&uneven, "test").unwrap_err());
        assert!(error.contains("`permission_tool_calls` has 1 entries for 0"), "{error}");
    }

    fn scenario(text: &str) -> Scenario {
        Scenario::parse(&format!("description = \"d\"\n{text}\n[[prompt]]\ntext = \"go\"\n"), "test").unwrap()
    }

    #[test]
    fn each_mode_has_its_own_default_gate() {
        assert_eq!(gate_policy(&scenario("")), SHIPPED_GATE);
        let contained: toml::Table = gate_policy(&scenario("mode = \"contained\"")).parse().unwrap();
        assert_eq!(contained["global"]["uncovered"].as_str(), Some("allow"));
        let own = scenario("gate = \"[global]\\nallow = [\\\"mkdir\\\"]\\n\"");
        assert_eq!(gate_policy(&own), "[global]\nallow = [\"mkdir\"]\n");
    }

    #[test]
    fn tool_output_and_permission_titles_are_checked() {
        let p = prompt(
            "tool_calls = [{ title = \"shell\", output_contains = \"shell_write\" }]\n\
             permissions = [\"deny\"]\npermission_titles = [\"ask tier\"]",
        );
        let response = json!({"stopReason": "end_turn"});
        let updates = [json!({"update": {"sessionUpdate": "tool_call", "toolCallId": "a", "title": "shell",
            "content": [{"type": "content", "content": {"type": "text", "text": "external commands are disabled"}}]}})];
        let permissions = [PermissionRecord {
            params: json!({"toolCall": {"title": "fleet hook asks"}}),
            answer: Some(PermissionAnswer::Deny),
            option_id: Some("deny".into()),
            problem: None,
            updates_seen: 0,
        }];
        let seen = Seen { response: &response, updates: &updates, answered: updates.len(), permissions: &permissions };
        let failures = check_prompt("p", &p, &seen).join("\n");
        assert!(failures.contains("output contains") && failures.contains("external commands are disabled"), "{failures}");
        assert!(failures.contains("title does not contain \"ask tier\""), "{failures}");
    }

    #[test]
    fn a_tool_call_status_mismatch_fails() {
        let p = prompt("tool_calls = [{ title = \"write\", status = \"completed\" }]");
        let response = json!({"stopReason": "end_turn"});
        let updates = [json!({"update": {"sessionUpdate": "tool_call", "toolCallId": "a", "title": "write", "status": "failed"}})];
        let seen = Seen { response: &response, updates: &updates, answered: updates.len(), permissions: &[] };
        assert_eq!(check_prompt("p", &p, &seen).len(), 1);
    }

    #[test]
    fn a_tool_call_kind_mismatch_fails() {
        let p = prompt("tool_calls = [{ title = \"done\", kind = \"other\" }]");
        let response = json!({"stopReason": "end_turn"});
        let updates = [json!({"update": {"sessionUpdate": "tool_call", "toolCallId": "a", "title": "done", "kind": "execute"}})];
        let seen = Seen { response: &response, updates: &updates, answered: updates.len(), permissions: &[] };
        let failures = check_prompt("p", &p, &seen);
        assert_eq!(failures.len(), 1, "{failures:#?}");
        assert!(failures[0].contains("kind other") && failures[0].contains("kind execute"), "{failures:#?}");
    }

    #[test]
    fn an_omitted_tool_call_kind_is_other() {
        let p = prompt("tool_calls = [{ title = \"done\", kind = \"other\" }]");
        let response = json!({"stopReason": "end_turn"});
        let updates = [json!({"update": {"sessionUpdate": "tool_call", "toolCallId": "a", "title": "done"}})];
        let seen = Seen { response: &response, updates: &updates, answered: updates.len(), permissions: &[] };
        assert_eq!(check_prompt("p", &p, &seen), Vec::<String>::new());
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
