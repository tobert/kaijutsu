//! The scenario file: one TOML file per scenario. See `docs/acp-fleet.md`.
//!
//! A scenario names what the client says (`[[prompt]]`), what the scripted
//! model answers (`[[model]]`), how permission requests are answered, what the
//! ACP update stream must show, and what the workspace must hold afterward
//! (`[[verify]]`). Unknown keys are refused, so a misspelled expectation fails
//! the load instead of checking nothing.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::client::PermissionAnswer;

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
    /// The scripted model's replies, consumed in order across all prompts.
    #[serde(default)]
    pub model: Vec<ModelTurn>,
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
    /// The `stopReason` the prompt must end with.
    #[serde(default = "end_turn")]
    pub stop_reason: String,
    /// Substrings the agent's message text for this prompt must contain.
    #[serde(default)]
    pub text_contains: Vec<String>,
    /// When present, the tool calls this prompt must show, exactly and in order.
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCallExpect>>,
    /// When present, send `session/cancel` during this prompt's turn.
    #[serde(default)]
    pub cancel: Option<Cancel>,
    /// Answers for permission requests earlier prompts answered `hold`,
    /// oldest first, given once this prompt's turn has ended.
    #[serde(default)]
    pub release: Vec<Answer>,
    /// After the turn ends, wait until the agent's text for this prompt
    /// contains this, before the quiet wait. For a message that arrives
    /// later than the quiet wait allows, such as a permission timeout.
    #[serde(default)]
    pub wait_for_text: Option<String>,
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// The agent runs on the host, and verifiers are declarative.
    #[default]
    Host,
    /// The agent runs in a container with no network and no host home, and
    /// `[[verify]]` may run a script in another container.
    Contained,
}

fn end_turn() -> String {
    "end_turn".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Answer {
    Allow,
    Deny,
    Cancel,
    /// Leave the request unanswered until a later prompt's `release`.
    Hold,
}

impl From<Answer> for PermissionAnswer {
    fn from(answer: Answer) -> Self {
        match answer {
            Answer::Allow => PermissionAnswer::Allow,
            Answer::Deny => PermissionAnswer::Deny,
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
    /// A substring of the call's reported output text.
    #[serde(default)]
    pub output_contains: Option<String>,
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
        for path in self.files.keys() {
            workspace_relative(path)?;
        }
        if let Some(gap) = &self.known_gap
            && (gap.finding.trim().is_empty() || gap.fails.is_empty() || gap.fails.iter().any(|f| f.trim().is_empty()))
        {
            bail!("[known_gap] needs a `finding` and at least one non-empty `fails` substring");
        }
        for path in self.rc.keys() {
            workspace_relative(path).context("an [rc] path is relative to the rc tree")?;
        }
        let mut held = 0;
        for (n, prompt) in self.prompt.iter().enumerate() {
            if prompt.release.contains(&Answer::Hold) {
                bail!("[[prompt]] {}: `release` answers held requests; `hold` is not an answer there", n + 1);
            }
            if prompt.release.len() > held {
                bail!("[[prompt]] {}: `release` has {} answers, but earlier prompts hold only {held}", n + 1, prompt.release.len());
            }
            held = held - prompt.release.len() + prompt.permissions.iter().filter(|a| **a == Answer::Hold).count();
            if let Some(release) = prompt.cancel.as_ref().and_then(|c| c.release.as_ref()) {
                workspace_relative(release).with_context(|| format!("[[prompt]] {}: `cancel.release`", n + 1))?;
            }
            if let Some(titles) = &prompt.permission_titles
                && titles.len() != prompt.permissions.len()
            {
                bail!(
                    "[[prompt]] {}: `permission_titles` has {} entries for {} `permissions`",
                    n + 1,
                    titles.len(),
                    prompt.permissions.len()
                );
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
    fn a_release_needs_an_earlier_hold() {
        let text = MINIMAL.replace("text = \"say hi\"", "text = \"say hi\"\nrelease = [\"allow\"]");
        assert!(refusal(&text).contains("hold only 0"), "{}", refusal(&text));
        let held = format!("{}\n[[prompt]]\ntext = \"later\"\nrelease = [\"allow\"]\n",
            MINIMAL.replace("text = \"say hi\"", "text = \"say hi\"\npermissions = [\"hold\"]"));
        Scenario::parse(&held, "held").unwrap();
    }

    #[test]
    fn a_scenario_with_no_prompt_is_refused() {
        assert!(refusal("description = \"d\"\nprompt = []\n").contains("at least one"));
    }

    #[test]
    fn an_unknown_answer_is_refused() {
        let text = MINIMAL.replace("text = \"say hi\"", "text = \"say hi\"\npermissions = [\"maybe\"]");
        assert!(refusal(&text).contains("maybe"), "{}", refusal(&text));
    }
}
