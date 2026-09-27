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
    /// The gate policy the agent runs under. Default: the shipped policy
    /// without its `[classifier]` table.
    #[serde(default)]
    pub gate: Option<String>,
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
    /// The `stopReason` the prompt must end with.
    #[serde(default = "end_turn")]
    pub stop_reason: String,
    /// Substrings the agent's message text for this prompt must contain.
    #[serde(default)]
    pub text_contains: Vec<String>,
    /// When present, the tool calls this prompt must show, exactly and in order.
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCallExpect>>,
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
}

impl From<Answer> for PermissionAnswer {
    fn from(answer: Answer) -> Self {
        match answer {
            Answer::Allow => PermissionAnswer::Allow,
            Answer::Deny => PermissionAnswer::Deny,
            Answer::Cancel => PermissionAnswer::Cancel,
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
}

/// A check on one workspace path. At least one of `exists`, `equals`, or
/// `contains` must be set.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verify {
    pub path: String,
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
        for (n, turn) in self.model.iter().enumerate() {
            let sugar = turn.text.is_some() || !turn.tool_calls.is_empty();
            match (&turn.events, sugar) {
                (Some(_), true) => bail!("[[model]] {}: use `events` alone, or `text`/`tool_calls`, not both", n + 1),
                (None, false) => bail!("[[model]] {}: give `text`, `tool_calls`, or `events`", n + 1),
                _ => {}
            }
        }
        for verify in &self.verify {
            workspace_relative(&verify.path)?;
            if verify.exists.is_none() && verify.equals.is_none() && verify.contains.is_none() {
                bail!("[[verify]] {}: give `exists`, `equals`, or `contains`", verify.path);
            }
            if verify.exists == Some(false) && (verify.equals.is_some() || verify.contains.is_some()) {
                bail!("[[verify]] {}: `exists = false` cannot also check contents", verify.path);
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
    fn a_scenario_with_no_prompt_is_refused() {
        assert!(refusal("description = \"d\"\nprompt = []\n").contains("at least one"));
    }

    #[test]
    fn an_unknown_answer_is_refused() {
        let text = MINIMAL.replace("text = \"say hi\"", "text = \"say hi\"\npermissions = [\"maybe\"]");
        assert!(refusal(&text).contains("maybe"), "{}", refusal(&text));
    }
}
