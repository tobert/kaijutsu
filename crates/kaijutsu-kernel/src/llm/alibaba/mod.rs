//! Alibaba Model Studio provider — a preset over the generic OpenAI-compatible
//! [`crate::llm::openai`] client.
//!
//! Model Studio speaks the OpenAI `/chat/completions` dialect, so the wire
//! machinery (SSE parsing, the streaming state machine, error mapping) lives in
//! `openai`. This module pins the Alibaba configuration:
//!
//! - **Base URL** from the backend row, always: the Token Plan and a workspace
//!   endpoint are different hosts with different keys.
//! - **API key required** — bearer auth.
//! - **[`openai::build::Dialect::Alibaba`]** — per-model thinking controls
//!   ([`models`]), multi-turn reasoning, explicit cache markers, and GLM's
//!   string content.
//!
//! See `docs/alibaba-backend.md`.

pub mod cache;
pub mod models;

use super::openai;
use crate::llm::stream::BuildOpts;
use crate::llm::{LlmResult, Message};

/// Whether a request to `model` with no tools must send text as strings:
/// glm drops array content when the request has no tools.
pub(crate) fn flattens_text(model: &str, has_tools: bool) -> bool {
    !has_tools && model.to_ascii_lowercase().starts_with("glm-")
}

/// Send each text-only content array as one string, parts joined with a
/// blank line. Content with an image keeps its parts.
pub(crate) fn flatten_text(messages: &mut [openai::types::RequestMessage]) {
    use openai::types::{ContentPart, MessageContent};
    for msg in messages {
        let Some(MessageContent::Parts(parts)) = &msg.content else { continue };
        let texts: Option<Vec<&str>> = parts
            .iter()
            .map(|p| match p {
                ContentPart::Text { text, .. } => Some(text.as_str()),
                ContentPart::ImageUrl { .. } => None,
            })
            .collect();
        if let Some(texts) = texts {
            msg.content = Some(MessageContent::Text(texts.join("\n\n")));
        }
    }
}

/// Alibaba Model Studio chat-completions client — a configured
/// [`openai::Client`].
#[derive(Clone, Debug)]
pub struct Client(openai::Client);

impl Client {
    /// Construct a client for the backend `name` at `base_url`, with bearer
    /// auth.
    pub fn new(name: impl Into<String>, base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self(
            openai::Client::new(name)
                .with_base_url(base_url)
                .with_api_key(api_key)
                .with_dialect(openai::build::Dialect::Alibaba)
                .with_header("X-DashScope-CacheControl", "enable"),
        )
    }

    /// Set the per-request HTTP timeout (see `openai::Client::with_request_timeout`).
    pub fn with_request_timeout(self, timeout: std::time::Duration) -> Self {
        Self(self.0.with_request_timeout(timeout))
    }

    /// Take a slot from `endpoint` before each request (see
    /// `openai::Client::with_endpoint`).
    pub fn with_endpoint(self, endpoint: std::sync::Arc<crate::llm::endpoint::Endpoint>) -> Self {
        Self(self.0.with_endpoint(endpoint))
    }

    /// The endpoint this client takes slots from.
    pub fn endpoint(&self) -> Option<&std::sync::Arc<crate::llm::endpoint::Endpoint>> {
        self.0.endpoint()
    }

    /// The backend name this client reports.
    pub fn provider_name(&self) -> &str {
        self.0.provider_name()
    }

    /// Models surfaced by default: none, since the model list is the
    /// backend's rows.
    pub fn available_models(&self) -> Vec<&'static str> {
        Vec::new()
    }

    /// The request options a one-shot prompt runs with: thinking off where
    /// the model allows it (see `openai::Client::one_shot_opts`).
    pub fn one_shot_opts(&self, opts: BuildOpts) -> BuildOpts {
        self.0.one_shot_opts(opts)
    }

    /// Start a streaming completion.
    pub async fn stream(
        &self,
        opts: BuildOpts,
        messages: Vec<Message>,
    ) -> LlmResult<openai::Stream> {
        self.0.stream(opts, messages).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::http_user_agent;
    use kaijutsu_mk::test_server::{serve, sse};

    const DONE: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

    /// The kaijutsu identity rides through the preset to the wire, and the
    /// bearer key goes to the configured endpoint.
    ///
    /// Falsified by a request with another User-Agent, or none.
    #[tokio::test]
    async fn outbound_requests_carry_the_kaijutsu_user_agent() {
        let (base, seen) = serve(vec![sse(DONE)]).await;
        let mut stream = Client::new("alibaba-token", &base, "fake-key")
            .stream(BuildOpts::new("qwen3.8-flash", 4096), vec![Message::user("hi")])
            .await
            .expect("stream round-trip against loopback");
        while stream.next_event().await.is_some() {}
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].header("user-agent"), Some(http_user_agent()));
        assert_eq!(seen[0].header("authorization"), Some("Bearer fake-key"));
        assert_eq!(seen[0].path, "/chat/completions");
    }

    /// The body the server received for `opts` and `messages`.
    async fn body(opts: BuildOpts, messages: Vec<Message>) -> serde_json::Value {
        let (base, seen) = serve(vec![sse(DONE)]).await;
        let mut stream = Client::new("alibaba", &base, "k").stream(opts, messages).await.expect("request is sent");
        while stream.next_event().await.is_some() {}
        let seen = seen.lock().unwrap();
        serde_json::from_str(&seen[0].body).unwrap()
    }

    /// The error a refused request fails with; the server receives nothing.
    async fn refused(opts: BuildOpts) -> crate::llm::LlmError {
        let (base, seen) = serve(vec![sse(DONE)]).await;
        let err = Client::new("alibaba", &base, "k").stream(opts, vec![Message::user("hi")]).await.err().expect("refused");
        assert!(seen.lock().unwrap().is_empty(), "a refused request is not sent");
        assert!(matches!(err, crate::llm::LlmError::InvalidRequest(_)), "{err:?}");
        err
    }

    fn opts(model: &str) -> BuildOpts {
        BuildOpts::new(model, 4096)
    }

    const THINKING_FIELDS: [&str; 4] = ["reasoning_effort", "enable_thinking", "thinking_budget", "preserve_thinking"];

    /// The thinking fields of a body, absent ones as null.
    fn thinking(v: &serde_json::Value) -> serde_json::Value {
        THINKING_FIELDS.iter().map(|k| (k.to_string(), v.get(*k).cloned().unwrap_or_default())).collect()
    }

    fn fields(effort: Option<&str>, enable: Option<bool>, budget: Option<u64>, preserve: bool) -> serde_json::Value {
        serde_json::json!({
            "reasoning_effort": effort,
            "enable_thinking": enable,
            "thinking_budget": budget,
            "preserve_thinking": preserve.then_some(true),
        })
    }

    /// qwen3.8 takes its effort ladder as `reasoning_effort` alone, and
    /// `none` turns thinking off; thinking on preserves earlier turns'
    /// reasoning.
    ///
    /// Falsified by an effort that is dropped, or one sent beside
    /// `enable_thinking`.
    #[tokio::test]
    async fn qwen38_takes_its_effort_as_reasoning_effort() {
        for model in ["qwen3.8-max", "qwen3.8-flash"] {
            for level in ["low", "medium", "high", "xhigh", "max"] {
                let v = body(opts(model).with_effort(level), vec![Message::user("hi")]).await;
                assert_eq!(thinking(&v), fields(Some(level), None, None, true), "{model} {level}");
            }
            let v = body(opts(model), vec![Message::user("hi")]).await;
            assert_eq!(thinking(&v), fields(None, None, None, true), "{model} default");
            let v = body(opts(model).with_effort("none"), vec![Message::user("hi")]).await;
            assert_eq!(thinking(&v), fields(Some("none"), None, None, false), "{model} off");
        }
    }

    /// qwen3.7, qwen3.6, glm-5.2, glm-5.1 and kimi-k2.5/k2.6 take only
    /// `enable_thinking`, on by default and off for `effort = "none"` or
    /// `thinking_style = "off"`.
    ///
    /// Falsified by a `reasoning_effort` on a toggle-only model.
    #[tokio::test]
    async fn toggle_models_take_enable_thinking() {
        for model in ["qwen3.7-plus", "qwen3.7-max", "qwen3.6-flash", "glm-5.2", "glm-5.1", "kimi-k2.5", "kimi-k2.6"] {
            let v = body(opts(model), vec![Message::user("hi")]).await;
            assert_eq!(thinking(&v), fields(None, Some(true), None, true), "{model} default");
            let v = body(opts(model).with_effort("none"), vec![Message::user("hi")]).await;
            assert_eq!(thinking(&v), fields(None, Some(false), None, false), "{model} effort none");
            let v = body(opts(model).with_thinking_style("off"), vec![Message::user("hi")]).await;
            assert_eq!(thinking(&v), fields(None, Some(false), None, false), "{model} style off");
        }
    }

    /// glm-5.3, kimi-k2.7-code and MiniMax-M2.5 think on every request: a
    /// request to turn thinking off is refused here, naming the model, and
    /// never sent.
    ///
    /// Falsified by a thinking-off request that reaches the server.
    #[tokio::test]
    async fn always_on_models_refuse_thinking_off_locally() {
        for model in ["glm-5.3", "kimi-k2.7-code", "MiniMax-M2.5"] {
            let v = body(opts(model), vec![Message::user("hi")]).await;
            assert_eq!(thinking(&v), fields(None, None, None, true), "{model} default");
            for off in [opts(model).with_effort("none"), opts(model).with_thinking_style("off")] {
                let err = refused(off).await;
                assert!(err.to_string().contains(model), "{model}: {err}");
            }
        }
    }

    /// DeepSeek V4 on Alibaba takes efforts `low`, `high` and `max`, and
    /// `enable_thinking: false` to turn thinking off.
    #[tokio::test]
    async fn deepseek_v4_on_alibaba_takes_low_high_max() {
        for model in ["deepseek-v4-pro", "deepseek-v4-flash", "deepseek-v4.1-flash"] {
            for level in ["low", "high", "max"] {
                let v = body(opts(model).with_effort(level), vec![Message::user("hi")]).await;
                assert_eq!(thinking(&v), fields(Some(level), None, None, true), "{model} {level}");
            }
            let v = body(opts(model).with_effort("none"), vec![Message::user("hi")]).await;
            assert_eq!(thinking(&v), fields(None, Some(false), None, false), "{model} off");
        }
    }

    /// An effort the model does not take is refused here, naming the model
    /// and what it does take, rather than clamped to a neighbor.
    ///
    /// Falsified by a request sent with a substituted effort.
    #[tokio::test]
    async fn an_effort_the_model_does_not_take_is_refused() {
        for (model, effort, names) in [
            ("qwen3.8-flash", "minimal", "low, medium, high, xhigh, max"),
            ("deepseek-v4-pro", "medium", "low, high, max"),
            ("qwen3.7-plus", "high", "on or off"),
            ("glm-5.3", "high", "every request"),
        ] {
            let err = refused(opts(model).with_effort(effort)).await;
            let msg = err.to_string();
            assert!(msg.contains(model) && msg.contains(effort) && msg.contains(names), "{model}: {msg}");
        }
    }

    /// qwen3.8 refuses `reasoning_effort` beside `thinking_budget`, so the
    /// pair is refused here; a budget alone rides as `thinking_budget`.
    #[tokio::test]
    async fn effort_and_thinking_budget_never_ride_together() {
        let err = refused(opts("qwen3.8-flash").with_effort("high").with_thinking_budget(2048)).await;
        assert!(err.to_string().contains("thinking_budget"), "{err}");
        let v = body(opts("qwen3.8-flash").with_thinking_budget(2048), vec![Message::user("hi")]).await;
        assert_eq!(thinking(&v), fields(None, None, Some(2048), true));
        let v = body(opts("qwen3.7-plus").with_thinking_budget(2048), vec![Message::user("hi")]).await;
        assert_eq!(thinking(&v), fields(None, Some(true), Some(2048), true));
    }

    /// A thinking style outside `auto`, `adaptive`, `budget` and `off` is
    /// refused, and `budget` needs a `thinking_budget`.
    #[tokio::test]
    async fn an_unknown_thinking_style_is_refused() {
        let err = refused(opts("qwen3.8-flash").with_thinking_style("deep")).await;
        assert!(err.to_string().contains("deep"), "{err}");
        let err = refused(opts("qwen3.8-flash").with_thinking_style("budget")).await;
        assert!(err.to_string().contains("thinking_budget"), "{err}");
    }

    /// Thinking carries across turns: each assistant message's reasoning goes
    /// back as `reasoning_content`, whichever provider wrote it, beside
    /// `preserve_thinking: true`. A turn with no reasoning sends none, and
    /// nothing is synthesized for it.
    ///
    /// Falsified by reasoning dropped from the history, or a marker invented
    /// for a turn that had none.
    #[tokio::test]
    async fn earlier_reasoning_goes_back_with_preserve_thinking() {
        use crate::llm::ContentBlock;
        let call = |id: &str| ContentBlock::ToolUse { id: id.into(), name: "ls".into(), input: serde_json::json!({}) };
        let result = |id: &str| ContentBlock::ToolResult { tool_use_id: id.into(), content: "ok".into(), is_error: false };
        let history = vec![
            Message::user("look around"),
            Message::with_reasoning_text_and_tool_uses(
                vec![("claude thought".into(), Some("anthropic-signature".into()))],
                Some("checking".into()),
                vec![call("c1")],
            ),
            Message::tool_results(vec![result("c1")]),
            Message::with_tool_uses(None, vec![call("c2")]),
            Message::tool_results(vec![result("c2")]),
        ];
        let v = body(opts("qwen3.8-flash").with_tools(vec![crate::llm::ToolDefinition {
            name: "ls".into(),
            description: "list".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }]), history).await;
        assert_eq!(v["preserve_thinking"], true, "{v}");
        let messages = v["messages"].as_array().unwrap();
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["reasoning_content"], "claude thought", "{}", messages[1]);
        assert_eq!(messages[3]["role"], "assistant");
        assert!(messages[3].get("reasoning_content").is_none(), "{}", messages[3]);
    }

    fn ls_tool() -> crate::llm::ToolDefinition {
        crate::llm::ToolDefinition {
            name: "ls".into(),
            description: "list".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    fn marked(text: &str) -> serde_json::Value {
        serde_json::json!([{"type": "text", "text": text, "cache_control": {"type": "ephemeral"}}])
    }

    /// The captured request and its body, for one request.
    async fn request(opts: BuildOpts, messages: Vec<Message>) -> (kaijutsu_mk::test_server::Captured, serde_json::Value) {
        let (base, seen) = serve(vec![sse(DONE)]).await;
        let mut stream = Client::new("alibaba", &base, "k").stream(opts, messages).await.expect("request is sent");
        while stream.next_event().await.is_some() {}
        let captured = seen.lock().unwrap().remove(0);
        let v = serde_json::from_str(&captured.body).unwrap();
        (captured, v)
    }

    /// `cache_breakpoints` become `cache_control` markers on message text:
    /// `System` on the system message, `MessageIndex` on the last text part
    /// of that message, and one on the conversation's last message, so each
    /// request's tail marker is within the 20 content blocks Alibaba looks
    /// back from the next one. Tools never carry a marker, and every request
    /// enables caching with `X-DashScope-CacheControl`.
    ///
    /// Falsified by a marker on a tool, a breakpoint that lands on another
    /// message, or a request without the header.
    #[tokio::test]
    async fn cache_breakpoints_become_markers_on_message_text() {
        use crate::llm::stream::{CacheTarget, CacheTtl};
        let o = opts("qwen3.8-flash")
            .with_system("be terse")
            .with_tools(vec![ls_tool()])
            .with_cache_breakpoints(vec![CacheTarget::System(CacheTtl::Ephemeral), CacheTarget::MessageIndex(1, CacheTtl::Ephemeral)]);
        let history = vec![Message::user("first"), Message::assistant("second"), Message::user("third")];
        let (captured, v) = request(o, history).await;
        assert_eq!(captured.header("x-dashscope-cachecontrol"), Some("enable"));
        let m = &v["messages"];
        assert_eq!(m[0]["content"], marked("be terse"), "{v}");
        assert_eq!(m[1]["content"], "first", "{v}");
        assert_eq!(m[2]["content"], marked("second"), "{v}");
        assert_eq!(m[3]["content"], marked("third"), "{v}");
        assert!(!v["tools"].to_string().contains("cache_control"), "{v}");
    }

    /// A marker lands on text: a breakpoint on an assistant message that
    /// only calls tools moves back to the nearest message with text, and a
    /// tool result at the tail carries its marker on its own text.
    #[tokio::test]
    async fn a_marker_moves_back_to_the_nearest_text() {
        use crate::llm::ContentBlock;
        use crate::llm::stream::{CacheTarget, CacheTtl};
        let history = vec![
            Message::user("look"),
            Message::with_tool_uses(None, vec![ContentBlock::ToolUse { id: "c1".into(), name: "ls".into(), input: serde_json::json!({}) }]),
            Message::tool_results(vec![ContentBlock::ToolResult { tool_use_id: "c1".into(), content: "ok".into(), is_error: false }]),
        ];
        let o = opts("qwen3.8-flash")
            .with_tools(vec![ls_tool()])
            .with_cache_breakpoints(vec![CacheTarget::MessageIndex(1, CacheTtl::Ephemeral)]);
        let (_, v) = request(o, history).await;
        let m = &v["messages"];
        assert_eq!(m[0]["content"], marked("look"), "{v}");
        assert!(m[1].get("content").is_none(), "{v}");
        assert_eq!(m[2]["role"], "tool");
        assert_eq!(m[2]["content"], marked("ok"), "{v}");
    }

    /// No breakpoints, no markers. `Tools` and `System` share the system
    /// message's marker. At most 4 markers ride on one request, explicit ones
    /// first, and a breakpoint past the history is dropped.
    #[tokio::test]
    async fn markers_are_deduplicated_and_capped_at_four() {
        use crate::llm::stream::{CacheTarget, CacheTtl::Ephemeral};
        let history: Vec<Message> =
            (0..6).map(|i| if i % 2 == 0 { Message::user(format!("u{i}")) } else { Message::assistant(format!("a{i}")) }).collect();
        let count = |v: &serde_json::Value| v.to_string().matches("cache_control").count();
        let base = || opts("qwen3.8-flash").with_system("sys").with_tools(vec![ls_tool()]);

        let (_, v) = request(base(), history.clone()).await;
        assert_eq!(count(&v), 0, "{v}");

        let (_, v) = request(base().with_cache_breakpoints(vec![CacheTarget::Tools(Ephemeral), CacheTarget::System(Ephemeral)]), history.clone()).await;
        assert_eq!(count(&v), 2, "system and tail: {v}");
        assert_eq!(v["messages"][0]["content"], marked("sys"), "{v}");

        let five = (0..5).map(|i| CacheTarget::MessageIndex(i, Ephemeral)).collect();
        let (_, v) = request(base().with_cache_breakpoints(five), history.clone()).await;
        assert_eq!(count(&v), 4, "{v}");
        assert_eq!(v["messages"][4]["content"], marked("a3"), "the fourth breakpoint: {v}");
        assert_eq!(v["messages"][5]["content"], "u4", "past the cap: {v}");
        assert_eq!(v["messages"][6]["content"], "a5", "no tail past the cap: {v}");

        let (_, v) = request(base().with_cache_breakpoints(vec![CacheTarget::MessageIndex(99, Ephemeral)]), history).await;
        assert_eq!(count(&v), 1, "the tail only: {v}");
        assert_eq!(v["messages"][6]["content"], marked("a5"), "{v}");
    }

    /// glm drops array content on a request with no tools, so each message's
    /// text-only content goes as one string, its parts joined with a blank
    /// line. A message with an image keeps its parts. A string carries no
    /// marker, so such a request has none; with tools, arrays and markers
    /// stay.
    ///
    /// Falsified by array content on a tool-less glm request.
    #[tokio::test]
    async fn a_glm_request_with_no_tools_sends_text_as_one_string() {
        use crate::llm::ContentBlock;
        use crate::llm::stream::{CacheTarget, CacheTtl::Ephemeral};
        let text = |t: &str| ContentBlock::Text { text: t.into() };
        let image = ContentBlock::Image { hash: "h".into(), media_type: "image/png".into(), data_base64: Some("QUJD".into()) };
        let history = vec![Message::tool_results(vec![text("a"), text("b")]), Message::tool_results(vec![text("look"), image])];
        let o = || {
            opts("glm-5.2")
                .with_system("sys")
                .with_cache_breakpoints(vec![CacheTarget::System(Ephemeral), CacheTarget::MessageIndex(0, Ephemeral)])
        };
        let (_, v) = request(o(), history.clone()).await;
        let m = &v["messages"];
        assert_eq!(m[0]["content"], "sys", "{v}");
        assert_eq!(m[1]["content"], "a\n\nb", "{v}");
        assert!(m[2]["content"].is_array(), "{v}");
        assert!(!v.to_string().contains("cache_control"), "{v}");

        let (_, v) = request(o().with_tools(vec![ls_tool()]), history.clone()).await;
        assert_eq!(v["messages"][1]["content"][1], serde_json::json!({"type": "text", "text": "b", "cache_control": {"type": "ephemeral"}}), "{v}");

        let (_, v) = request(opts("qwen3.7-plus").with_system("sys"), history).await;
        assert!(v["messages"][1]["content"].is_array(), "only glm flattens: {v}");
    }

    /// A model the table does not know is treated as toggle-only, with one
    /// warning per model.
    ///
    /// Falsified by a warning on every request, or an unknown model that
    /// gets an effort field.
    #[tokio::test]
    async fn an_unknown_model_is_toggle_only_and_warns_once() {
        let buf = Buf::default();
        let sub = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();
        let _guard = tracing::subscriber::set_default(sub);
        for _ in 0..3 {
            let v = body(opts("qwen9-unknown-test"), vec![Message::user("hi")]).await;
            assert_eq!(thinking(&v), fields(None, Some(true), None, true));
        }
        let log = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert_eq!(log.matches("qwen9-unknown-test").count(), 1, "{log}");
    }

    #[derive(Clone, Default)]
    struct Buf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
        type Writer = Buf;
        fn make_writer(&'a self) -> Buf {
            self.clone()
        }
    }

    /// A one-shot turns thinking off where the model allows it, keeps it on
    /// for a model that always thinks, and leaves a slot's explicit effort
    /// alone.
    ///
    /// Falsified by a one-shot that thinks on a toggle model, or one refused
    /// on an always-on model.
    #[tokio::test]
    async fn a_one_shot_turns_thinking_off_where_the_model_allows_it() {
        use crate::llm::Provider;
        let cases = [
            (opts("qwen3.8-flash"), fields(Some("none"), None, None, false)),
            (opts("qwen3.7-plus"), fields(None, Some(false), None, false)),
            (opts("deepseek-v4-pro"), fields(None, Some(false), None, false)),
            (opts("glm-5.3"), fields(None, None, None, true)),
            (opts("qwen3.8-flash").with_effort("high"), fields(Some("high"), None, None, true)),
        ];
        for (o, want) in cases {
            let model = o.model.clone();
            let (base, seen) = serve(vec![sse(DONE)]).await;
            Provider::Alibaba(Client::new("alibaba", &base, "k"))
                .prompt_with_system(o, "hi")
                .await
                .unwrap_or_else(|e| panic!("{model}: {e}"));
            let v: serde_json::Value = serde_json::from_str(&seen.lock().unwrap()[0].body).unwrap();
            assert_eq!(thinking(&v), want, "{model}");
        }
    }
}
