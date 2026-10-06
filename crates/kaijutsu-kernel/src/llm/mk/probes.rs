//! Live probes for `docs/mk.md`, "Order of work", step 4. Ignored by default;
//! run by hand on zorak under the heavy lock, one at a time:
//!
//! `MK_URL=http://100.83.138.103:8090 flock -w 900 ~/.cache/zorak-heavy.lock
//!  cargo test -p kaijutsu-kernel --lib mk::probes -- --ignored --nocapture --test-threads=1`
//!
//! Each probe drives the provider the way the runtime does: the assistant's
//! thinking goes back signed, its calls go back as `ToolUse`, and canned
//! results answer them. Each prints its numbers; the assertions only check
//! that the loop ran.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{json, Value};

use super::Client;
use crate::llm::stream::{BuildOpts, MkUsageExtra, StreamEvent, UsageExtra};
use crate::llm::{ContentBlock, Message, MessageContent, Role, ToolDefinition};

fn client() -> Client {
    let base = std::env::var("MK_URL").expect("MK_URL names the service");
    Client::new("probe-mk", &base, Duration::from_secs(300), HashMap::new()).unwrap()
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        input_schema: json!({"type": "object", "properties": properties, "required": required}),
    }
}

fn tools() -> Vec<ToolDefinition> {
    vec![
        tool("ls", "List a directory.", json!({"path": {"type": "string"}}), &["path"]),
        tool("read_file", "Read a text file.", json!({"path": {"type": "string"}}), &["path"]),
        tool(
            "write_file",
            "Write a text file, replacing it.",
            json!({"path": {"type": "string"}, "content": {"type": "string"}}),
            &["path", "content"],
        ),
        tool(
            "grep",
            "Search files for a regular expression.",
            json!({"pattern": {"type": "string"}, "path": {"type": "string"}, "ignore_case": {"type": "boolean"}}),
            &["pattern", "path"],
        ),
        tool(
            "run",
            "Run a shell command and return its output.",
            json!({"command": {"type": "string"}, "timeout_secs": {"type": "integer"}}),
            &["command"],
        ),
    ]
}

fn opts(effort: &str, max_tokens: u64) -> BuildOpts {
    BuildOpts {
        model: "qwen".into(),
        system: Some(
            "You maintain a small Linux host. Use the tools to do what is asked, one step at a time. \
             When the task is done, say so in one sentence."
                .into(),
        ),
        max_tokens,
        temperature: Some(0.0),
        top_p: None,
        effort: Some(effort.into()),
        thinking_budget: None,
        thinking_style: None,
        tools: tools(),
        cache_breakpoints: Vec::new(),
    }
}

/// A canned result for a call, so the loop can continue.
fn answer(name: &str, input: &Value) -> String {
    let path = input.get("path").and_then(Value::as_str).unwrap_or("");
    match name {
        "ls" => format!("{path}:\nREADME.md\nnotes.txt\nsrc/\nlogs/"),
        "read_file" => format!("# {path}\nTODO: rotate the logs weekly\nTODO: check disk usage\n"),
        "write_file" => format!("wrote {} bytes to {path}", input.get("content").and_then(Value::as_str).map_or(0, str::len)),
        "grep" => format!("{path}/notes.txt:2:TODO: rotate the logs weekly\n{path}/notes.txt:3:TODO: check disk usage"),
        "run" => "Filesystem  Size  Used Avail Use%\n/dev/nvme0n1p2  1.8T  1.1T  700G  62% /".into(),
        _ => format!("unknown tool {name}"),
    }
}

/// One inference's outcome.
struct Turn {
    events: Vec<StreamEvent>,
    usage: MkUsageExtra,
    prompt: u64,
    completion: u64,
    stop: String,
}

impl Turn {
    fn calls(&self) -> Vec<(String, String, Value)> {
        self.events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolUse { id, name, input } => Some((id.clone(), name.clone(), input.clone())),
                _ => None,
            })
            .collect()
    }

    fn invalid(&self) -> Vec<(String, String)> {
        self.events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolUseInvalid { name, error, .. } => Some((name.clone(), error.clone())),
                _ => None,
            })
            .collect()
    }

    /// The assistant message the runtime would record and replay.
    fn assistant(&self) -> Message {
        let mut blocks = Vec::new();
        let mut thinking = String::new();
        let mut text = String::new();
        for e in &self.events {
            match e {
                StreamEvent::ThinkingDelta(t) => thinking.push_str(t),
                StreamEvent::ThinkingEnd { signature } => blocks.push(ContentBlock::Reasoning {
                    text: std::mem::take(&mut thinking),
                    signature: signature.clone(),
                }),
                StreamEvent::TextDelta(t) => text.push_str(t),
                StreamEvent::TextEnd => blocks.push(ContentBlock::Text { text: std::mem::take(&mut text) }),
                StreamEvent::ToolUse { id, name, input } => {
                    blocks.push(ContentBlock::ToolUse { id: id.clone(), name: name.clone(), input: input.clone() })
                }
                StreamEvent::ToolUseInvalid { id, name, arguments, .. } => blocks.push(ContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: json!({ "truncated_arguments": arguments }),
                }),
                _ => {}
            }
        }
        Message { role: Role::Assistant, content: MessageContent::Blocks(blocks) }
    }

    /// The results message answering every call, valid or not.
    fn results(&self) -> Option<Message> {
        let mut blocks = Vec::new();
        for e in &self.events {
            match e {
                StreamEvent::ToolUse { id, name, input } => blocks.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: answer(name, input),
                    is_error: false,
                }),
                StreamEvent::ToolUseInvalid { id, error, .. } => blocks.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: format!("This call did not run: its arguments did not parse ({error})."),
                    is_error: true,
                }),
                _ => {}
            }
        }
        (!blocks.is_empty()).then(|| Message::tool_results(blocks))
    }
}

async fn infer(c: &Client, o: &BuildOpts, history: &[Message]) -> Turn {
    let mut stream = c.stream(o.clone(), history.to_vec()).await.unwrap();
    let mut events = Vec::new();
    while let Some(e) = stream.next_event().await {
        events.push(e);
    }
    let Some(StreamEvent::Done { stop_reason, input_tokens, output_tokens, extra: Some(UsageExtra::Mk(usage)) }) =
        events.last().cloned()
    else {
        panic!("no done: {events:?}")
    };
    Turn {
        events,
        usage,
        prompt: input_tokens.unwrap_or(0),
        completion: output_tokens.unwrap_or(0),
        stop: stop_reason.unwrap_or_default(),
    }
}

fn report(label: &str, i: usize, t: &Turn) {
    println!(
        "{label} #{i}: stop={} prompt={} kept={} fed={} completion={} prefill_ms={} decode_ms={} first_token_ms={} wall_ms={} calls={} invalid={}",
        t.stop, t.prompt, t.usage.kept, t.usage.fed, t.completion, t.usage.prefill_ms, t.usage.decode_ms,
        t.usage.first_token_ms, t.usage.wall_ms, t.calls().len(), t.invalid().len()
    );
}

/// Runs inferences until the model answers without a call or `limit` is
/// reached, appending each turn and its results to `history`.
async fn run_loop(c: &Client, o: &BuildOpts, history: &mut Vec<Message>, label: &str, limit: usize) -> Vec<Turn> {
    let mut turns = Vec::new();
    for i in 0..limit {
        let t = infer(c, o, history).await;
        report(label, i, &t);
        history.push(t.assistant());
        let results = t.results();
        let more = results.is_some();
        if let Some(r) = results {
            history.push(r);
        }
        turns.push(t);
        if !more {
            break;
        }
    }
    turns
}

/// Does a held prefix survive replay of a turn with thinking on?
#[tokio::test]
#[ignore = "live: needs MK_URL and the heavy lock"]
async fn probe_thinking_replay() {
    let c = client();
    let o = opts("low", 1024);
    let mut history = vec![Message::user("How full is the root filesystem? Use the run tool.")];
    let turns = run_loop(&c, &o, &mut history, "thinking", 4).await;
    assert!(turns.len() >= 2, "the model made no call");
}

/// Does a call with several arguments replay into the held prefix?
#[tokio::test]
#[ignore = "live: needs MK_URL and the heavy lock"]
async fn probe_multi_argument_replay() {
    let c = client();
    let o = opts("none", 512);
    let mut history = vec![Message::user(
        "Write a file /srv/notes/today.md with the content '# Today\\n- rotate logs', then confirm.",
    )];
    let turns = run_loop(&c, &o, &mut history, "multi-arg", 4).await;
    let args: Vec<String> = turns.iter().flat_map(|t| t.calls()).map(|(_, n, i)| format!("{n} {i}")).collect();
    println!("multi-arg calls: {args:?}");
    assert!(turns.len() >= 2, "the model made no call");
}

/// Ten inferences of one task, to see `kept` and `fed` as the history grows.
#[tokio::test]
#[ignore = "live: needs MK_URL and the heavy lock"]
async fn probe_ten_turn_loop() {
    let c = client();
    let o = opts("none", 512);
    let mut history = vec![Message::user(
        "Survey /srv: list it, read its notes, search it for TODO, and check disk usage. One tool call per step.",
    )];
    let mut turns = run_loop(&c, &o, &mut history, "loop", 10).await;
    let follow_ups = ["Now list /srv/logs.", "Read /srv/README.md.", "Search /srv/src for FIXME."];
    for follow in follow_ups {
        if turns.len() >= 10 {
            break;
        }
        history.push(Message::user(follow));
        let left = 10 - turns.len();
        turns.extend(run_loop(&c, &o, &mut history, "loop", left).await);
    }
    let total_prompt: u64 = turns.iter().map(|t| t.prompt).sum();
    let total_fed: u64 = turns.iter().map(|t| t.usage.fed).sum();
    println!("loop: {} inferences, prompt tokens {total_prompt}, fed {total_fed}", turns.len());
}

/// Twenty small tool tasks: how often does a call fail to parse?
#[tokio::test]
#[ignore = "live: needs MK_URL and the heavy lock"]
async fn probe_parse_failures() {
    let tasks = [
        "List /etc.",
        "Read /etc/hostname.",
        "Search /var/log for the word error, ignoring case.",
        "Write /tmp/a.txt containing hello.",
        "Run the command uptime.",
        "Write /tmp/b.json containing {\"ok\": true, \"n\": 3}.",
        "Search /srv for TODO in files under it.",
        "Run df -h with a 10 second timeout.",
        "Read /srv/notes.txt.",
        "Write /tmp/script.sh containing a bash script that prints the date, with a shebang line.",
        "List /home.",
        "Run the command: grep -c ssh /etc/services",
        "Write /tmp/quote.txt containing: She said \"it's <done> & dusted\".",
        "Search /etc for the pattern ^root: in passwd-like files.",
        "Read /var/log/syslog.",
        "Run ls -la /tmp | head -5",
        "Write /tmp/multi.txt containing three lines: one, two, three.",
        "List /srv/logs.",
        "Search /srv/src for fn main.",
        "Run the command free -m.",
    ];
    let c = client();
    let o = opts("none", 384);
    let (mut calls, mut invalid, mut none) = (0, 0, 0);
    for (i, task) in tasks.iter().enumerate() {
        let t = infer(&c, &o, &[Message::user(*task)]).await;
        report("task", i, &t);
        calls += t.calls().len();
        invalid += t.invalid().len();
        if t.calls().is_empty() && t.invalid().is_empty() {
            none += 1;
        }
        for (name, error) in t.invalid() {
            println!("  invalid {name}: {error}");
        }
    }
    println!("parse: {} tasks, {calls} parsed calls, {invalid} unparsed, {none} with no call", tasks.len());
}
