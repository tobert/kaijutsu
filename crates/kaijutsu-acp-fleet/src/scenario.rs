//! The scenario file: one TOML file per scenario. See `docs/acp-fleet.md`.
//!
//! A scenario names what the client says (`[[prompt]]`), what the scripted
//! model answers (`[[model]]`), how permission requests are answered, what the
//! ACP update stream must show, and what the workspace must hold afterward
//! (`[[verify]]`). Unknown keys are refused, so a misspelled expectation fails
//! the load instead of checking nothing.
//!
//! A `[live]` scenario talks to a real model API instead of the scripted one,
//! and a `[council]` scenario gets a council server for the run.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::client::PermissionAnswer;
use crate::container::Endpoint;
use crate::council::{Script, Undo, Verdict};

/// The text a scenario's `gate` uses for the council server's address. The
/// runner replaces it with the `[council]` server.
pub const COUNCIL_PLACEHOLDER: &str = "{council}";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    /// One sentence: what this scenario proves.
    pub description: String,
    /// Where the agent runs: on the host, or in a container.
    #[serde(default)]
    pub mode: Mode,
    /// The gate policy the agent runs under. Default in host mode: the
    /// shipped policy. Default in contained mode: every uncovered statement
    /// is allowed.
    #[serde(default)]
    pub gate: Option<String>,
    /// rc files installed over the seeded rc tree, by path relative to it
    /// (`--rc-overlay`). A scenario installs a pre_call hook this way.
    #[serde(default)]
    pub rc: BTreeMap<String, String>,
    /// Files written into the workspace before the agent starts, by
    /// workspace-relative path.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    /// A workspace-relative directory `session/new` names as the session
    /// cwd. The agent is still launched in the workspace root, so a relative
    /// path the model uses resolves here only if the session cwd is honored.
    /// Default: the workspace root.
    #[serde(default)]
    pub session_cwd: Option<String>,
    /// The scripted model's replies, consumed in order across all prompts.
    #[serde(default)]
    pub model: Vec<ModelTurn>,
    /// Talk to a real model API instead of the scripted model.
    #[serde(default)]
    pub live: Option<Live>,
    /// A council server for the gate: scripted for this run, or a real one.
    #[serde(default)]
    pub council: Option<Council>,
    /// The prompts the client sends, in order, each with its expectations.
    pub prompt: Vec<Prompt>,
    /// Checks on the workspace after the agent exits.
    #[serde(default)]
    pub verify: Vec<Verify>,
    /// A finding this scenario reproduces until it is fixed. The scenario
    /// must then fail, and only in the ways `fails` names.
    #[serde(default)]
    pub known_gap: Option<KnownGap>,
}

/// A real model API the agent talks to. A live run spends money.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Live {
    /// The agent's `--backend-kind`, such as `deepseek`.
    pub backend_kind: String,
    /// The agent's `--model`, such as `deepseek-flash`.
    pub model: String,
    /// A file holding the API key; a leading `~/` is the home directory. The
    /// runner reads it and hands the key to the agent in an environment
    /// variable it names with `--api-key-env`.
    pub api_key_file: String,
}

/// The model API host each backend kind talks to by default, which is
/// the one endpoint a contained live agent reaches for its model.
pub const API_HOSTS: &[(&str, &str)] =
    &[("anthropic", "api.anthropic.com"), ("deepseek", "api.deepseek.com"), ("openai", "api.openai.com")];

impl Live {
    /// The model API a contained agent must reach: the backend kind's
    /// default host, on 443. `None` for a backend kind the fleet does not
    /// know.
    pub fn api_endpoint(&self) -> Option<Endpoint> {
        API_HOSTS
            .iter()
            .find(|(kind, _)| *kind == self.backend_kind)
            .map(|(_, host)| Endpoint { host: host.to_string(), port: 443 })
    }

    /// `api_key_file` with a leading `~/` expanded from `$HOME`.
    pub fn key_path(&self) -> Result<PathBuf> {
        match self.api_key_file.strip_prefix("~/") {
            Some(rest) => {
                let home = std::env::var_os("HOME").context("`api_key_file` starts with ~/, and HOME is not set")?;
                Ok(PathBuf::from(home).join(rest))
            }
            None => Ok(PathBuf::from(&self.api_key_file)),
        }
    }
}

/// The council a scenario's gate reads: `verdicts` for a council the runner
/// serves on 127.0.0.1, or `server` for a real one. A contained agent reaches
/// either through the relay (`crate::relay`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Council {
    /// One verdict per decision, in order; the last one repeats.
    #[serde(default)]
    pub verdicts: Vec<Verdict>,
    /// The `undo` read every scripted decision carries.
    #[serde(default)]
    pub undo: Option<Undo>,
    /// A real council server, such as `http://zorak:8090`.
    #[serde(default)]
    pub server: Option<String>,
}

impl Council {
    /// The script for a council the runner serves, or `None` for a real one.
    pub fn script(&self) -> Option<Script> {
        (self.server.is_none()).then(|| Script { verdicts: self.verdicts.clone(), undo: self.undo })
    }
}

/// A recorded finding (`docs/issues.md`) the scenario fails on today.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnownGap {
    /// The finding's id, such as `F2`.
    pub finding: String,
    /// Substrings of the failures the finding causes. A scenario with a known
    /// gap passes only when at least one failure matches and every failure
    /// matches one of these.
    pub fails: Vec<String>,
}

/// One model reply. Either `text` and/or `tool_calls`, or raw `events`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelTurn {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolUse>,
    /// Raw stream events, exactly as the mock backend reads them.
    #[serde(default)]
    pub events: Option<Vec<toml::Value>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolUse {
    /// Defaults to `fleet-<reply>-<call>`, both counted from 1.
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    #[serde(default = "empty_table")]
    pub input: toml::Value,
}

fn empty_table() -> toml::Value {
    toml::Value::Table(toml::map::Map::new())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prompt {
    pub text: String,
    /// The answers given to this prompt's permission requests, in order. The
    /// prompt must raise exactly this many requests.
    #[serde(default)]
    pub permissions: Vec<Answer>,
    /// When present, one substring per permission request, in order, that
    /// the request's title must contain.
    #[serde(default)]
    pub permission_titles: Option<Vec<String>>,
    /// When present, one title per permission request, in order: the
    /// request's `toolCall.toolCallId` must name a `tool_call` this prompt
    /// announced with exactly this title.
    #[serde(default)]
    pub permission_tool_calls: Option<Vec<String>>,
    /// The `stopReason` the prompt must end with.
    #[serde(default = "end_turn")]
    pub stop_reason: String,
    /// Substrings the agent's message text for this prompt must contain.
    #[serde(default)]
    pub text_contains: Vec<String>,
    /// When present, the tool calls this prompt must show, exactly and in order.
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCallExpect>>,
    /// Substrings that must each appear in the output of at least one tool
    /// call this prompt shows, whatever the calls are. A live scenario
    /// checks this instead of an exact `tool_calls`.
    #[serde(default)]
    pub tool_output_contains: Vec<String>,
    /// The last `usage_update` before the prompt's response must carry a
    /// cost in USD, which is where Harbor reads a run's cost from.
    #[serde(default)]
    pub reports_cost: bool,
    /// When present, send `session/cancel` during this prompt's turn.
    #[serde(default)]
    pub cancel: Option<Cancel>,
    /// Answers for the requests this prompt answered `hold`, oldest first,
    /// given while the prompt is still open: once every request it holds
    /// has arrived, and after `on_hold`. Each answer after the first waits
    /// until the agent has recorded the one before it. A held request with
    /// no answer here stays unanswered.
    #[serde(default)]
    pub release: Vec<Answer>,
    /// What the runner does once every request this prompt holds has
    /// arrived, before it sends `release`.
    #[serde(default)]
    pub on_hold: Option<OnHold>,
    /// Replace the kernel's `gate.toml` with this before sending the prompt,
    /// as an operator editing it would. Host mode only.
    #[serde(default)]
    pub gate: Option<String>,
    /// From this prompt on, the scripted council answers these verdicts,
    /// starting from the first; the last one repeats. Needs `[council]
    /// verdicts`.
    #[serde(default)]
    pub council_verdicts: Vec<Verdict>,
}

/// When to send `session/cancel` during a prompt, and what to do after the
/// agent confirms it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cancel {
    /// Cancel once a tool call with this title is reported `in_progress`.
    pub after_tool_call: String,
    /// A workspace-relative file to write once the agent confirms the
    /// cancel. A command that waits for it is still running when the
    /// cancel lands.
    #[serde(default)]
    pub release: Option<String>,
}

/// Out-of-band work while a prompt's permission requests are held: the
/// model's turn is waiting on them, and a sibling tool call can act.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OnHold {
    /// A workspace-relative file to write, so a command that waits for it
    /// runs only after the asks exist.
    #[serde(default)]
    pub write: Option<String>,
    /// A workspace-relative path to wait for, after `write`, before the
    /// held requests are answered.
    #[serde(default)]
    pub wait_for: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// The agent runs on the host, and verifiers are declarative.
    #[default]
    Host,
    /// The agent runs in a container with no network of its own and no host
    /// home, reaching only its council and model API through the relay, and
    /// `[[verify]]` may run a script in another container.
    Contained,
}

fn end_turn() -> String {
    "end_turn".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Answer {
    Allow,
    /// Allow, and remember the answer as a standing rule.
    AllowAlways,
    Deny,
    /// Deny, and remember the answer as a standing rule.
    DenyAlways,
    Cancel,
    /// Leave the request unanswered until a later prompt's `release`.
    Hold,
}

impl From<Answer> for PermissionAnswer {
    fn from(answer: Answer) -> Self {
        match answer {
            Answer::Allow => PermissionAnswer::Allow,
            Answer::AllowAlways => PermissionAnswer::AllowAlways,
            Answer::Deny => PermissionAnswer::Deny,
            Answer::DenyAlways => PermissionAnswer::DenyAlways,
            Answer::Cancel => PermissionAnswer::Cancel,
            Answer::Hold => PermissionAnswer::Hold,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCallExpect {
    pub title: String,
    /// The call's last reported status, such as `completed` or `failed`.
    #[serde(default)]
    pub status: Option<String>,
    /// The call's ACP `kind`, such as `execute` or `other`.
    #[serde(default)]
    pub kind: Option<String>,
    /// A substring of the call's reported output text, or a list of them.
    #[serde(default)]
    pub output_contains: Option<Substrings>,
}

/// One substring, or several that must all appear.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Substrings {
    One(String),
    All(Vec<String>),
}

impl Substrings {
    pub fn all(&self) -> &[String] {
        match self {
            Self::One(one) => std::slice::from_ref(one),
            Self::All(all) => all,
        }
    }
}

/// A check on the workspace after the run: a `path` with at least one of
/// `exists`, `equals`, or `contains`, or, in contained mode only, a `script`
/// run in a fresh container over the workspace that passes on exit 0.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verify {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub script: Option<String>,
    #[serde(default)]
    pub exists: Option<bool>,
    #[serde(default)]
    pub equals: Option<String>,
    #[serde(default)]
    pub contains: Option<String>,
}

impl Scenario {
    /// Parse and check a scenario. `origin` names it in errors.
    pub fn parse(text: &str, origin: &str) -> Result<Self> {
        let scenario: Self = toml::from_str(text).with_context(|| format!("parse {origin}"))?;
        scenario.check().with_context(|| format!("check {origin}"))?;
        Ok(scenario)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        Self::parse(&text, &path.display().to_string())
    }

    fn check(&self) -> Result<()> {
        if self.prompt.is_empty() {
            bail!("a scenario needs at least one [[prompt]]");
        }
        if let Some(live) = &self.live {
            if !self.model.is_empty() {
                bail!("[live] talks to a real model, so [[model]] replies would never be read; remove one or the other");
            }
            if self.mode == Mode::Contained && live.api_endpoint().is_none() {
                bail!(
                    "[live] a contained agent reaches only the model API the fleet knows for its backend, and it knows \
                     none for {:?}; use one of {:?}",
                    live.backend_kind,
                    API_HOSTS.iter().map(|(kind, _)| *kind).collect::<Vec<_>>()
                );
            }
            for (key, value) in [("backend_kind", &live.backend_kind), ("model", &live.model), ("api_key_file", &live.api_key_file)] {
                if value.trim().is_empty() {
                    bail!("[live] `{key}` is empty");
                }
            }
            if self.mode != Mode::Contained {
                bail!(
                    "[live] a live model runs only in a container, where it reaches nothing but its council and \
                     its model API: set mode = \"contained\" and put the scenario in fleet/contained/live/"
                );
            }
        }
        let placeholder = self
            .gate
            .iter()
            .chain(self.prompt.iter().filter_map(|p| p.gate.as_ref()))
            .any(|g| g.contains(COUNCIL_PLACEHOLDER));
        match &self.council {
            Some(council) => {
                match (&council.server, council.verdicts.is_empty()) {
                    (Some(_), false) => bail!("[council] give `verdicts` for a scripted council or `server` for a real one, not both"),
                    (None, true) => bail!("[council] needs `verdicts`, at least one, or a `server`"),
                    (Some(server), true) if council.undo.is_some() => {
                        bail!("[council] `undo` scripts the scripted council's answers; the server {server} answers for itself")
                    }
                    (Some(server), true) if !server.starts_with("http://") && !server.starts_with("https://") => {
                        bail!("[council] `server` {server:?} must start with http:// or https://")
                    }
                    (Some(server), true) if self.mode == Mode::Contained => {
                        Endpoint::from_url(server).context("[council] `server`")?;
                    }
                    _ => {}
                }
                if !placeholder {
                    bail!("[council] is set, but no `gate` names it: write `server = \"{COUNCIL_PLACEHOLDER}\"` under the gate's [council]");
                }
            }
            None if placeholder => {
                bail!("a `gate` names {COUNCIL_PLACEHOLDER}, but the scenario has no [council] to put there")
            }
            None => {}
        }
        for path in self.files.keys() {
            workspace_relative(path)?;
        }
        if let Some(cwd) = &self.session_cwd {
            workspace_relative(cwd).context("`session_cwd`")?;
        }
        if let Some(gap) = &self.known_gap
            && (gap.finding.trim().is_empty() || gap.fails.is_empty() || gap.fails.iter().any(|f| f.trim().is_empty()))
        {
            bail!("[known_gap] needs a `finding` and at least one non-empty `fails` substring");
        }
        for path in self.rc.keys() {
            workspace_relative(path).context("an [rc] path is relative to the rc tree")?;
        }
        for (n, prompt) in self.prompt.iter().enumerate() {
            if prompt.gate.is_some() && self.mode != Mode::Host {
                bail!("[[prompt]] {}: `gate` edits the kernel's config on the host; contained scenarios cannot use it", n + 1);
            }
            if !prompt.council_verdicts.is_empty() && self.council.as_ref().is_none_or(|c| c.verdicts.is_empty()) {
                bail!("[[prompt]] {}: `council_verdicts` restarts the scripted council, and [council] has no `verdicts`", n + 1);
            }
            if prompt.release.contains(&Answer::Hold) {
                bail!("[[prompt]] {}: `release` answers held requests; `hold` is not an answer there", n + 1);
            }
            let held = prompt.permissions.iter().filter(|a| **a == Answer::Hold).count();
            if prompt.release.len() > held {
                bail!("[[prompt]] {}: `release` has {} answers, but this prompt holds only {held}", n + 1, prompt.release.len());
            }
            if let Some(on_hold) = &prompt.on_hold {
                if held == 0 {
                    bail!("[[prompt]] {}: `on_hold` runs once this prompt's held requests arrive, but it holds none", n + 1);
                }
                if on_hold.write.is_none() && on_hold.wait_for.is_none() {
                    bail!("[[prompt]] {}: `on_hold` needs `write`, `wait_for`, or both", n + 1);
                }
                for path in on_hold.write.iter().chain(&on_hold.wait_for) {
                    workspace_relative(path).with_context(|| format!("[[prompt]] {}: `on_hold`", n + 1))?;
                }
            }
            if let Some(release) = prompt.cancel.as_ref().and_then(|c| c.release.as_ref()) {
                workspace_relative(release).with_context(|| format!("[[prompt]] {}: `cancel.release`", n + 1))?;
            }
            for (field, list) in [("permission_titles", &prompt.permission_titles), ("permission_tool_calls", &prompt.permission_tool_calls)] {
                if let Some(list) = list
                    && list.len() != prompt.permissions.len()
                {
                    bail!(
                        "[[prompt]] {}: `{field}` has {} entries for {} `permissions`",
                        n + 1,
                        list.len(),
                        prompt.permissions.len()
                    );
                }
            }
        }
        for (n, turn) in self.model.iter().enumerate() {
            let sugar = turn.text.is_some() || !turn.tool_calls.is_empty();
            match (&turn.events, sugar) {
                (Some(_), true) => bail!("[[model]] {}: use `events` alone, or `text`/`tool_calls`, not both", n + 1),
                (None, false) => bail!("[[model]] {}: give `text`, `tool_calls`, or `events`", n + 1),
                _ => {}
            }
        }
        for (n, verify) in self.verify.iter().enumerate() {
            let has_check = verify.exists.is_some() || verify.equals.is_some() || verify.contains.is_some();
            match (&verify.path, &verify.script) {
                (Some(_), Some(_)) => bail!("[[verify]] {}: give `path` or `script`, not both", n + 1),
                (None, None) => bail!("[[verify]] {}: give `path` or `script`", n + 1),
                (None, Some(_)) if has_check => {
                    bail!("[[verify]] {}: `script` stands alone; `exists`, `equals`, and `contains` need a `path`", n + 1)
                }
                (None, Some(_)) if self.mode != Mode::Contained => bail!(
                    "[[verify]] {}: a `script` verifier needs `mode = \"contained\"`; scripts never run on the host",
                    n + 1
                ),
                (None, Some(_)) => {}
                (Some(path), None) => {
                    workspace_relative(path)?;
                    if !has_check {
                        bail!("[[verify]] {path}: give `exists`, `equals`, or `contains`");
                    }
                    if verify.exists == Some(false) && (verify.equals.is_some() || verify.contains.is_some()) {
                        bail!("[[verify]] {path}: `exists = false` cannot also check contents");
                    }
                }
            }
        }
        Ok(())
    }

    /// The mock backend's script: one array of stream events per reply.
    pub fn mock_script(&self) -> Result<Value> {
        let mut turns = Vec::new();
        for (n, turn) in self.model.iter().enumerate() {
            if let Some(events) = &turn.events {
                turns.push(serde_json::to_value(events).context("convert raw events to JSON")?);
                continue;
            }
            let mut events = Vec::new();
            if let Some(text) = &turn.text {
                events.push(json!("TextStart"));
                events.push(json!({"TextDelta": text}));
                events.push(json!("TextEnd"));
            }
            for (i, call) in turn.tool_calls.iter().enumerate() {
                let id = call.id.clone().unwrap_or_else(|| format!("fleet-{}-{}", n + 1, i + 1));
                let input = serde_json::to_value(&call.input).context("convert tool input to JSON")?;
                events.push(json!({"ToolUse": {"id": id, "name": call.name, "input": input}}));
            }
            let stop = if turn.tool_calls.is_empty() { "end_turn" } else { "tool_use" };
            events.push(json!({"Done": {"stop_reason": stop, "input_tokens": 1, "output_tokens": 1, "extra": null}}));
            turns.push(Value::Array(events));
        }
        Ok(Value::Array(turns))
    }
}

/// A workspace-relative path with no `..`, or an error naming it.
pub fn workspace_relative(path: &str) -> Result<PathBuf> {
    let p = PathBuf::from(path);
    if path.is_empty() || p.components().any(|c| !matches!(c, Component::Normal(_))) {
        bail!("{path:?} must be a relative path inside the workspace, with no `..`");
    }
    Ok(p)
}

/// The scenario files a list of paths names: each file as given, and every
/// `*.toml` directly inside each directory, sorted by name.
pub fn discover(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for path in paths {
        if path.is_dir() {
            let mut inside: Vec<PathBuf> = std::fs::read_dir(path)
                .with_context(|| format!("read {}", path.display()))?
                .map(|entry| entry.map(|e| e.path()))
                .collect::<std::io::Result<_>>()
                .with_context(|| format!("list {}", path.display()))?;
            inside.retain(|p| p.extension().and_then(|e| e.to_str()) == Some("toml"));
            inside.sort();
            found.extend(inside);
        } else if path.is_file() {
            found.push(path.clone());
        } else {
            bail!("{} is neither a scenario file nor a directory of them", path.display());
        }
    }
    // A file named and also inside a named directory runs once.
    let mut seen = std::collections::HashSet::new();
    found.retain(|p| seen.insert(p.canonicalize().unwrap_or_else(|_| p.clone())));
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
description = "d"
[[model]]
text = "hi"
[[prompt]]
text = "say hi"
"#;

    #[test]
    fn a_text_reply_expands_to_a_bracketed_run_and_end_turn() {
        let scenario = Scenario::parse(MINIMAL, "minimal").unwrap();
        assert_eq!(
            scenario.mock_script().unwrap(),
            json!([[
                "TextStart",
                {"TextDelta": "hi"},
                "TextEnd",
                {"Done": {"stop_reason": "end_turn", "input_tokens": 1, "output_tokens": 1, "extra": null}}
            ]])
        );
        assert_eq!(scenario.prompt[0].stop_reason, "end_turn", "a prompt expects a clean end by default");
    }

    #[test]
    fn a_tool_reply_ends_with_tool_use_and_numbers_its_calls() {
        let text = r#"
description = "d"
[[model]]
tool_calls = [{ name = "write", input = { path = "a.txt", content = "x" } }]
[[prompt]]
text = "go"
"#;
        let script = Scenario::parse(text, "tool").unwrap().mock_script().unwrap();
        assert_eq!(
            script,
            json!([[
                {"ToolUse": {"id": "fleet-1-1", "name": "write", "input": {"path": "a.txt", "content": "x"}}},
                {"Done": {"stop_reason": "tool_use", "input_tokens": 1, "output_tokens": 1, "extra": null}}
            ]])
        );
    }

    #[test]
    fn raw_events_pass_through_unchanged() {
        let text = r#"
description = "d"
[[model]]
events = ["TextStart", { TextDelta = "raw" }, "TextEnd", { Done = { stop_reason = "end_turn" } }]
[[prompt]]
text = "go"
"#;
        let script = Scenario::parse(text, "raw").unwrap().mock_script().unwrap();
        assert_eq!(
            script,
            json!([["TextStart", {"TextDelta": "raw"}, "TextEnd", {"Done": {"stop_reason": "end_turn"}}]])
        );
    }

    fn refusal(text: &str) -> String {
        format!("{:#}", Scenario::parse(text, "bad").unwrap_err())
    }

    #[test]
    fn a_misspelled_key_is_refused() {
        let text = MINIMAL.replace("text = \"say hi\"", "text = \"say hi\"\ntext_contain = [\"hi\"]");
        assert!(refusal(&text).contains("text_contain"), "{}", refusal(&text));
    }

    #[test]
    fn a_verify_with_no_check_is_refused() {
        let text = format!("{MINIMAL}\n[[verify]]\npath = \"a.txt\"\n");
        assert!(refusal(&text).contains("give `exists`"), "{}", refusal(&text));
    }

    #[test]
    fn a_path_outside_the_workspace_is_refused() {
        for path in ["../x", "/etc/passwd", "a/../../b", ""] {
            let text = format!("{MINIMAL}\n[[verify]]\npath = {path:?}\nexists = true\n");
            assert!(refusal(&text).contains("inside the workspace"), "{path}: {}", refusal(&text));
        }
    }

    #[test]
    fn a_reply_mixing_events_and_sugar_is_refused() {
        let text = MINIMAL.replace("text = \"hi\"", "text = \"hi\"\nevents = [\"TextStart\"]");
        assert!(refusal(&text).contains("not both"), "{}", refusal(&text));
    }

    #[test]
    fn a_script_verifier_is_refused_outside_contained_mode() {
        let text = format!("{MINIMAL}\n[[verify]]\nscript = \"true\"\n");
        assert!(refusal(&text).contains("scripts never run on the host"), "{}", refusal(&text));
        let contained = format!("mode = \"contained\"\n{MINIMAL}\n[[verify]]\nscript = \"true\"\n");
        Scenario::parse(&contained, "contained").unwrap();
    }

    #[test]
    fn an_rc_path_outside_the_rc_tree_is_refused() {
        let text = format!("{MINIMAL}\n[rc]\n\"../escape.kai\" = \"x\"\n");
        assert!(refusal(&text).contains("rc tree"), "{}", refusal(&text));
    }

    #[test]
    fn a_cancel_release_outside_the_workspace_is_refused() {
        let text = MINIMAL.replace(
            "text = \"say hi\"",
            "text = \"say hi\"\ncancel = { after_tool_call = \"shell_write\", release = \"../go\" }",
        );
        assert!(refusal(&text).contains("cancel.release"), "{}", refusal(&text));
    }

    #[test]
    fn a_release_answers_only_its_own_prompts_holds() {
        let text = MINIMAL.replace("text = \"say hi\"", "text = \"say hi\"\nrelease = [\"allow\"]");
        assert!(refusal(&text).contains("holds only 0"), "{}", refusal(&text));
        let later = format!("{}\n[[prompt]]\ntext = \"later\"\nrelease = [\"allow\"]\n",
            MINIMAL.replace("text = \"say hi\"", "text = \"say hi\"\npermissions = [\"hold\"]"));
        assert!(refusal(&later).contains("holds only 0"), "{}", refusal(&later));
        let own = MINIMAL.replace("text = \"say hi\"", "text = \"say hi\"\npermissions = [\"hold\"]\nrelease = [\"allow\"]");
        Scenario::parse(&own, "held").unwrap();
    }

    #[test]
    fn on_hold_needs_a_hold_and_paths_inside_the_workspace() {
        let bare = MINIMAL.replace("text = \"say hi\"", "text = \"say hi\"\non_hold = { write = \"go\" }");
        assert!(refusal(&bare).contains("holds none"), "{}", refusal(&bare));
        let held = |on_hold: &str| {
            MINIMAL.replace("text = \"say hi\"", &format!("text = \"say hi\"\npermissions = [\"hold\"]\non_hold = {on_hold}"))
        };
        assert!(refusal(&held("{}")).contains("needs `write`"), "{}", refusal(&held("{}")));
        let escape = held("{ wait_for = \"../x\" }");
        assert!(refusal(&escape).contains("inside the workspace"), "{}", refusal(&escape));
        Scenario::parse(&held("{ write = \"go\", wait_for = \"done\" }"), "on_hold").unwrap();
    }

    #[test]
    fn output_contains_takes_one_substring_or_several() {
        let text = |value: &str| {
            format!("description = \"d\"\n[[prompt]]\ntext = \"go\"\ntool_calls = [{{ title = \"t\", output_contains = {value} }}]\n")
        };
        let one = Scenario::parse(&text("\"a\""), "one").unwrap();
        let calls = one.prompt[0].tool_calls.as_ref().unwrap();
        assert_eq!(calls[0].output_contains.as_ref().unwrap().all(), ["a".to_string()]);
        let many = Scenario::parse(&text("[\"a\", \"b\"]"), "many").unwrap();
        let calls = many.prompt[0].tool_calls.as_ref().unwrap();
        assert_eq!(calls[0].output_contains.as_ref().unwrap().all(), ["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn a_session_cwd_outside_the_workspace_is_refused() {
        let text = format!("session_cwd = \"../elsewhere\"\n{MINIMAL}");
        assert!(refusal(&text).contains("session_cwd"), "{}", refusal(&text));
        Scenario::parse(&format!("session_cwd = \"task\"\n{MINIMAL}"), "cwd").unwrap();
    }

    #[test]
    fn a_scenario_with_no_prompt_is_refused() {
        assert!(refusal("description = \"d\"\nprompt = []\n").contains("at least one"));
    }

    const LIVE: &str = r#"
description = "d"
[live]
backend_kind = "deepseek"
model = "deepseek-flash"
api_key_file = "~/.deepseek-key.txt"
[[prompt]]
text = "go"
permissions = ["allow"]
tool_output_contains = ["Bumped by the council"]
[[verify]]
path = "made.txt"
exists = true
"#;

    /// A file named and also inside a named directory is discovered once.
    #[test]
    fn a_file_named_twice_is_discovered_once() {
        let dir = std::env::temp_dir().join(format!("fleet-discover-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("one.toml");
        std::fs::write(&file, "").unwrap();
        let found = discover(&[file.clone(), dir.clone()]).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(found, [file]);
    }

    /// A live model runs only in a container. Amy, 2026-10-10: "let's wait
    /// on more tests if they're not contained in a container yet."
    #[test]
    fn a_live_scenario_runs_only_in_a_container() {
        assert!(refusal(LIVE).contains("runs only in a container"), "{}", refusal(LIVE));
        let host = format!("mode = \"host\"\n{LIVE}");
        assert!(refusal(&host).contains("runs only in a container"), "{}", refusal(&host));
    }

    #[test]
    fn a_live_scenario_names_its_model_and_key_file() {
        let scenario = Scenario::parse(&format!("mode = \"contained\"\n{LIVE}"), "live").unwrap();
        let live = scenario.live.as_ref().unwrap();
        assert_eq!((live.backend_kind.as_str(), live.model.as_str()), ("deepseek", "deepseek-flash"));
        let home = std::env::var("HOME").unwrap();
        assert_eq!(live.key_path().unwrap(), PathBuf::from(home).join(".deepseek-key.txt"));
        assert!(scenario.prompt[0].tool_calls.is_none(), "a live prompt needs no exact tool call list");
        assert_eq!(scenario.prompt[0].tool_output_contains, ["Bumped by the council"]);
        let absolute = format!("mode = \"contained\"\n{}", LIVE.replace("~/.deepseek-key.txt", "/keys/k"));
        assert_eq!(Scenario::parse(&absolute, "abs").unwrap().live.unwrap().key_path().unwrap(), PathBuf::from("/keys/k"));
    }

    #[test]
    fn a_contained_live_scenario_reaches_its_backends_api() {
        let contained = Scenario::parse(&format!("mode = \"contained\"\n{LIVE}"), "contained").unwrap();
        assert_eq!(contained.live.unwrap().api_endpoint(), Some(Endpoint { host: "api.deepseek.com".into(), port: 443 }));
        let unknown = format!("mode = \"contained\"\n{}", LIVE.replace("\"deepseek\"", "\"local\""));
        assert!(refusal(&unknown).contains("knows none for \"local\""), "{}", refusal(&unknown));
    }

    #[test]
    fn a_live_scenario_refuses_scripted_replies_and_stray_keys() {
        let scripted = format!("{LIVE}\n[[model]]\ntext = \"hi\"\n");
        assert!(refusal(&scripted).contains("[[model]] replies would never be read"), "{}", refusal(&scripted));
        let stray = LIVE.replace("model = \"deepseek-flash\"", "model = \"deepseek-flash\"\napi_key = \"sk-x\"");
        assert!(refusal(&stray).contains("api_key"), "{}", refusal(&stray));
        let empty = LIVE.replace("deepseek-flash", " ");
        assert!(refusal(&empty).contains("`model` is empty"), "{}", refusal(&empty));
    }

    fn with_council(council: &str, gate: &str) -> String {
        format!("gate = {gate:?}\n{MINIMAL}\n[council]\n{council}\n")
    }

    const COUNCIL_GATE: &str = "[council]\nserver = \"{council}\"\n";

    #[test]
    fn a_scripted_council_reads_its_verdicts_in_order() {
        let text = with_council("verdicts = [\"try_harder\", \"do_less\", \"proceed\"]\nundo = \"irreversible\"", COUNCIL_GATE);
        let script = Scenario::parse(&text, "council").unwrap().council.unwrap().script().unwrap();
        assert_eq!(script.verdicts, [Verdict::TryHarder, Verdict::DoLess, Verdict::Proceed]);
        assert_eq!(script.undo, Some(Undo::Irreversible));
        let real = with_council("server = \"http://zorak:8090\"", COUNCIL_GATE);
        assert!(Scenario::parse(&real, "real").unwrap().council.unwrap().script().is_none(), "a real server is not scripted");
    }

    #[test]
    fn a_council_needs_exactly_one_source_and_a_gate_that_names_it() {
        let both = with_council("verdicts = [\"proceed\"]\nserver = \"http://zorak:8090\"", COUNCIL_GATE);
        assert!(refusal(&both).contains("not both"), "{}", refusal(&both));
        let neither = with_council("", COUNCIL_GATE);
        assert!(refusal(&neither).contains("needs `verdicts`"), "{}", refusal(&neither));
        let unknown = with_council("verdicts = [\"maybe\"]", COUNCIL_GATE);
        assert!(refusal(&unknown).contains("maybe"), "{}", refusal(&unknown));
        let unnamed = with_council("verdicts = [\"proceed\"]", "[global]\n");
        assert!(refusal(&unnamed).contains("no `gate` names it"), "{}", refusal(&unnamed));
        let undo_on_real = with_council("server = \"http://zorak:8090\"\nundo = \"normal\"", COUNCIL_GATE);
        assert!(refusal(&undo_on_real).contains("answers for itself"), "{}", refusal(&undo_on_real));
        let orphan = format!("gate = {COUNCIL_GATE:?}\n{MINIMAL}");
        assert!(refusal(&orphan).contains("no [council]"), "{}", refusal(&orphan));
    }

    #[test]
    fn a_prompt_restarts_only_a_scripted_council() {
        let restart = |council: &str| {
            with_council(council, COUNCIL_GATE).replace("text = \"say hi\"", "text = \"say hi\"\ncouncil_verdicts = [\"do_less\"]")
        };
        let scenario = Scenario::parse(&restart("verdicts = [\"proceed\"]"), "scripted").unwrap();
        assert_eq!(scenario.prompt[0].council_verdicts, [Verdict::DoLess]);
        let real = restart("server = \"http://zorak:8090\"");
        assert!(refusal(&real).contains("has no `verdicts`"), "{}", refusal(&real));
        let none = MINIMAL.replace("text = \"say hi\"", "text = \"say hi\"\ncouncil_verdicts = [\"do_less\"]");
        assert!(refusal(&none).contains("has no `verdicts`"), "{}", refusal(&none));
    }

    #[test]
    fn a_contained_council_is_scripted_or_a_named_server() {
        let contained = |council: &str| format!("mode = \"contained\"\n{}", with_council(council, COUNCIL_GATE));
        Scenario::parse(&contained("verdicts = [\"proceed\"]"), "scripted").unwrap();
        Scenario::parse(&contained("server = \"http://zorak:8090\""), "named").unwrap();
        let address = contained("server = \"http://192.168.1.5:8090\"");
        assert!(refusal(&address).contains("name the host"), "{}", refusal(&address));
        Scenario::parse(&with_council("server = \"http://192.168.1.5:8090\"", COUNCIL_GATE), "host").unwrap();
    }

    #[test]
    fn an_unknown_answer_is_refused() {
        let text = MINIMAL.replace("text = \"say hi\"", "text = \"say hi\"\npermissions = [\"maybe\"]");
        assert!(refusal(&text).contains("maybe"), "{}", refusal(&text));
    }
}
