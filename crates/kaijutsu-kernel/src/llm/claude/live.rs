//! Live tests of the Messages client against the real servers. All are
//! `#[ignore]`: they spend money and need a key, so they run only when asked.
//!
//! Each provider takes the path of a file that holds its API key; an unset
//! variable skips that provider's tests with a message on stderr.
//!
//! ```sh
//! KJ_LIVE_DEEPSEEK_KEY_FILE=~/.deepseek-key \
//! KJ_LIVE_ANTHROPIC_KEY_FILE=~/.anthropic-key.txt \
//!   systemd-run --user --scope -p CPUWeight=20 --nice=19 -- \
//!   cargo test -p kaijutsu-kernel --lib claude::live -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Filter with `live_deepseek` or `live_anthropic` to run one provider.
//! `KJ_LIVE_DEEPSEEK_MODEL` (default `deepseek-flash`) and
//! `KJ_LIVE_ANTHROPIC_MODEL` (default `claude-haiku-5-5`) pick the model.
//! Each test sends one or two small requests.

use std::time::Duration;

use super::Client;
use crate::llm::stream::{BuildOpts, StreamEvent, UsageExtra};
use crate::llm::{ContentBlock, Message, ToolDefinition};

/// A live request that outlasts this is a hang, not a slow answer.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// The key in the file named by `var`, or `None` (with a message) when `var`
/// is unset. A set variable that names an unreadable file fails the test.
fn key_from(var: &str) -> Option<String> {
    let Some(path) = std::env::var_os(var).filter(|p| !p.is_empty()) else {
        eprintln!("skipped: {var} is not set");
        return None;
    };
    let key = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{var} names {path:?}, which cannot be read: {e}"));
    Some(key.trim().to_string())
}

fn model_from(var: &str, default: &str) -> String {
    std::env::var(var).ok().filter(|m| !m.is_empty()).unwrap_or_else(|| default.to_string())
}

/// Everything one streamed reply carried.
#[derive(Debug, Default)]
struct Turn {
    /// One `(text, signature)` per thinking block, in order.
    thinking: Vec<(String, Option<String>)>,
    text: String,
    tool_uses: Vec<(String, String, serde_json::Value)>,
    stop_reason: Option<String>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    extra: Option<UsageExtra>,
}

async fn run_turn(client: &Client, opts: BuildOpts, messages: Vec<Message>) -> Turn {
    let mut stream = client.stream(opts, messages).await.expect("the stream opens");
    let mut turn = Turn::default();
    let mut done = false;
    while let Some(event) = stream.next_event().await {
        match event {
            StreamEvent::ThinkingStart => turn.thinking.push((String::new(), None)),
            StreamEvent::ThinkingDelta(t) => turn.thinking.last_mut().expect("a thinking run is open").0.push_str(&t),
            StreamEvent::ThinkingEnd { signature } => {
                turn.thinking.last_mut().expect("a thinking run is open").1 = signature
            }
            StreamEvent::TextDelta(t) => turn.text.push_str(&t),
            StreamEvent::ToolUse { id, name, input } => turn.tool_uses.push((id, name, input)),
            StreamEvent::Done { stop_reason, input_tokens, output_tokens, extra } => {
                done = true;
                turn.stop_reason = stop_reason;
                turn.input_tokens = input_tokens;
                turn.output_tokens = output_tokens;
                turn.extra = extra;
            }
            StreamEvent::Error(e) => panic!("live stream error: {e}"),
            StreamEvent::TextStart | StreamEvent::TextEnd => {}
            other => panic!("unexpected event {other:?}"),
        }
    }
    assert!(done, "the stream must end in Done: {turn:?}");
    println!("{turn:#?}");
    turn
}

fn assert_usage(turn: &Turn) {
    assert!(turn.input_tokens.unwrap_or(0) > 0, "input tokens: {turn:?}");
    assert!(turn.output_tokens.unwrap_or(0) > 0, "output tokens: {turn:?}");
    if let Some(UsageExtra::Claude(extra)) = &turn.extra {
        // Present only when the server reported a cache split.
        println!("cache: {extra:?}");
    }
}

async fn plain_turn(client: &Client, model: &str) {
    let opts = BuildOpts::new(model, 4096).with_max_tokens(1024).with_system("Answer in one short sentence.");
    let turn = run_turn(client, opts, vec![Message::user("Name one primary color.")]).await;
    assert!(!turn.text.trim().is_empty(), "{turn:?}");
    assert_eq!(turn.stop_reason.as_deref(), Some("end_turn"), "{turn:?}");
    assert_usage(&turn);
}

/// `effort` turns thinking on for DeepSeek, which otherwise relies on the
/// server default; Anthropic models think by default.
fn thinking_opts(model: &str) -> BuildOpts {
    BuildOpts::new(model, 4096).with_max_tokens(4096).with_effort("high")
}

async fn thinking_turn(client: &Client, model: &str) {
    let turn = run_turn(
        client,
        thinking_opts(model),
        vec![Message::user("What is 17 times 23? Work it out, then give the number.")],
    )
    .await;
    assert!(!turn.thinking.is_empty(), "a thinking block: {turn:?}");
    assert!(
        turn.thinking.iter().any(|(text, _)| !text.trim().is_empty()),
        "thinking text, not only signatures: {turn:?}"
    );
    for (text, signature) in &turn.thinking {
        assert!(signature.as_deref().is_some_and(|s| !s.is_empty()), "signature on {text:?}: {turn:?}");
    }
    assert!(turn.text.contains("391"), "{turn:?}");
    assert_usage(&turn);
}

fn add_tool() -> ToolDefinition {
    ToolDefinition {
        name: "add".into(),
        description: "Add two integers.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"a": {"type": "integer"}, "b": {"type": "integer"}},
            "required": ["a", "b"],
        }),
    }
}

/// Turn one asks for the tool; turn two replays the assistant message with
/// its thinking blocks (empty ones included) and signatures, answers the
/// call, and must finish.
async fn tool_round_trip(client: &Client, model: &str) {
    let ask = Message::user("Use the add tool to add 2 and 3, then tell me the sum.");
    let first = run_turn(client, thinking_opts(model).with_tools(vec![add_tool()]), vec![ask.clone()]).await;
    assert_eq!(first.stop_reason.as_deref(), Some("tool_use"), "{first:?}");
    let (id, name, input) = first.tool_uses.first().cloned().expect("a tool call");
    assert_eq!(name, "add");
    assert_eq!(input, serde_json::json!({"a": 2, "b": 3}), "the assembled input: {first:?}");

    // The replay goes through the builder production uses.
    let replayed = Message::with_reasoning_text_and_tool_uses(
        first.thinking.clone(),
        (!first.text.is_empty()).then(|| first.text.clone()),
        vec![ContentBlock::ToolUse { id: id.clone(), name, input }],
    );
    let result = Message::tool_results(vec![ContentBlock::ToolResult {
        tool_use_id: id,
        content: "5".into(),
        is_error: false,
    }]);

    let second = run_turn(
        client,
        thinking_opts(model).with_tools(vec![add_tool()]),
        vec![ask, replayed, result],
    )
    .await;
    assert_eq!(second.stop_reason.as_deref(), Some("end_turn"), "{second:?}");
    assert!(second.text.contains('5'), "{second:?}");
    assert_usage(&second);
}

fn deepseek() -> Option<(Client, String)> {
    let key = key_from("KJ_LIVE_DEEPSEEK_KEY_FILE")?;
    Some((Client::deepseek(key).with_request_timeout(REQUEST_TIMEOUT), model_from("KJ_LIVE_DEEPSEEK_MODEL", "deepseek-flash")))
}

fn anthropic() -> Option<(Client, String)> {
    let key = key_from("KJ_LIVE_ANTHROPIC_KEY_FILE")?;
    Some((Client::new(key).with_request_timeout(REQUEST_TIMEOUT), model_from("KJ_LIVE_ANTHROPIC_MODEL", "claude-haiku-5-5")))
}

#[tokio::test]
#[ignore = "live: needs KJ_LIVE_DEEPSEEK_KEY_FILE"]
async fn live_deepseek_plain_turn() {
    if let Some((client, model)) = deepseek() {
        plain_turn(&client, &model).await;
    }
}

#[tokio::test]
#[ignore = "live: needs KJ_LIVE_DEEPSEEK_KEY_FILE"]
async fn live_deepseek_thinking_turn() {
    if let Some((client, model)) = deepseek() {
        thinking_turn(&client, &model).await;
    }
}

#[tokio::test]
#[ignore = "live: needs KJ_LIVE_DEEPSEEK_KEY_FILE"]
async fn live_deepseek_tool_round_trip_replays_thinking() {
    if let Some((client, model)) = deepseek() {
        tool_round_trip(&client, &model).await;
    }
}

/// A wrong model name is an `invalid_request_error` from the server, not a
/// stream.
#[tokio::test]
#[ignore = "live: needs KJ_LIVE_DEEPSEEK_KEY_FILE"]
async fn live_deepseek_unknown_model_is_an_invalid_request() {
    if let Some((client, _)) = deepseek() {
        let err = client
            .stream(BuildOpts::new("no-such-model", 4096).with_max_tokens(16), vec![Message::user("hi")])
            .await
            .err()
            .expect("the open fails");
        assert!(matches!(err, crate::llm::LlmError::InvalidRequest(_)), "{err:?}");
    }
}

#[tokio::test]
#[ignore = "live: needs KJ_LIVE_ANTHROPIC_KEY_FILE"]
async fn live_anthropic_plain_turn() {
    if let Some((client, model)) = anthropic() {
        plain_turn(&client, &model).await;
    }
}

#[tokio::test]
#[ignore = "live: needs KJ_LIVE_ANTHROPIC_KEY_FILE"]
async fn live_anthropic_thinking_turn() {
    if let Some((client, model)) = anthropic() {
        thinking_turn(&client, &model).await;
    }
}

#[tokio::test]
#[ignore = "live: needs KJ_LIVE_ANTHROPIC_KEY_FILE"]
async fn live_anthropic_tool_round_trip_replays_thinking() {
    if let Some((client, model)) = anthropic() {
        tool_round_trip(&client, &model).await;
    }
}

/// A current model accepts the effort token on the request's top-level
/// `output_config` beside adaptive thinking.
#[tokio::test]
#[ignore = "live: needs KJ_LIVE_ANTHROPIC_KEY_FILE"]
async fn live_anthropic_effort_is_accepted_beside_adaptive_thinking() {
    if let Some((client, _)) = anthropic() {
        let opts = BuildOpts::new("claude-sonnet-5-5", 4096).with_max_tokens(1024).with_effort("low");
        let turn = run_turn(&client, opts, vec![Message::user("Reply with the single word: ready.")]).await;
        assert!(!turn.text.is_empty(), "{turn:?}");
    }
}
