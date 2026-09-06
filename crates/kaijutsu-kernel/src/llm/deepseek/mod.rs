//! DeepSeek provider — a thin preset over the generic OpenAI-compatible
//! [`crate::llm::openai`] client.
//!
//! DeepSeek speaks the OpenAI `/chat/completions` dialect, so all the wire
//! machinery (request building, SSE parsing, the streaming state machine)
//! lives in `openai`. This module only pins the DeepSeek-specific
//! configuration:
//!
//! - **Base URL** `https://api.deepseek.com`.
//! - **API key required** — bearer auth, unlike a keyless local server.
//! - **`reasoning_required`** — DeepSeek V4 thinks by default and *requires*
//!   the chain-of-thought echoed back on any assistant turn that performed
//!   tool calls (else HTTP 400). See [`crate::llm::openai::build`].
//! - **Model list** — the two tool-capable models (`deepseek-v4-flash`,
//!   `deepseek-v4-pro`). The pure reasoning model can't call tools, so it
//!   belongs in a forked, tool-less context rather than the agentic loop.

use super::openai;
use crate::llm::stream::BuildOpts;
use crate::llm::{LlmResult, Message};

const DEEPSEEK_DEFAULT_BASE_URL: &str = "https://api.deepseek.com";

/// DeepSeek chat-completions client — a configured [`openai::Client`].
#[derive(Clone, Debug)]
pub struct Client(openai::Client);

impl Client {
    /// Construct a DeepSeek client from an API key (bearer auth required).
    pub fn new(api_key: impl Into<String>) -> Self {
        Self(
            openai::Client::new("deepseek")
                .with_base_url(DEEPSEEK_DEFAULT_BASE_URL)
                .with_api_key(api_key)
                .with_reasoning_required(true),
        )
    }

    /// Override the API base URL (for the `/beta` endpoint or a proxy).
    pub fn with_base_url(self, base_url: impl Into<String>) -> Self {
        Self(self.0.with_base_url(base_url))
    }

    /// Set the per-request HTTP timeout (see `openai::Client::with_request_timeout`).
    pub fn with_request_timeout(self, timeout: std::time::Duration) -> Self {
        Self(self.0.with_request_timeout(timeout))
    }

    /// Tool-capable models surfaced by this provider.
    pub fn available_models(&self) -> Vec<&'static str> {
        vec!["deepseek-v4-flash", "deepseek-v4-pro"]
    }

    /// One-shot prompt with optional system preamble (non-streaming).
    pub async fn prompt(
        &self,
        model: &str,
        system: Option<&str>,
        prompt: &str,
    ) -> LlmResult<String> {
        self.0.prompt(model, system, prompt).await
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
    use crate::llm::StreamEvent;
    use crate::llm::http_user_agent;
    use crate::llm::stream::UsageExtra;

    /// DeepSeek is a thin preset over `openai::Client` — this proves the
    /// kaijutsu identity actually rides through that wrapper to the wire,
    /// rather than assuming inheritance from the shared builder. Mirrors
    /// `openai::tests::outbound_requests_carry_the_kaijutsu_user_agent`.
    #[tokio::test]
    async fn outbound_requests_carry_the_kaijutsu_user_agent() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback bind");
        let port = listener.local_addr().expect("local addr").port();

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept");
            use tokio::io::AsyncReadExt;
            let mut head = Vec::new();
            loop {
                let mut buf = [0u8; 512];
                let n = sock.read(&mut buf).await.expect("read request");
                if n == 0 {
                    break;
                }
                head.extend_from_slice(&buf[..n]);
                if head.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let body = r#"{"choices":[{"message":{"role":"assistant","content":"ok"}}]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            use tokio::io::AsyncWriteExt;
            let _ = sock.write_all(resp.as_bytes()).await;
            String::from_utf8_lossy(&head).into_owned()
        });

        let text = Client::new("fake-key")
            .with_base_url(format!("http://127.0.0.1:{port}"))
            .prompt("deepseek-v4-flash", None, "hi")
            .await
            .expect("prompt round-trip against loopback");
        assert_eq!(text, "ok", "response must parse through the real path");

        let head = server.await.expect("server task");
        let ua = head
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("user-agent:"))
            .unwrap_or_else(|| panic!("request head carried no User-Agent:\n{head}"));
        let (name, value) = ua.split_once(':').expect("matched on the colon");
        assert!(name.eq_ignore_ascii_case("user-agent"), "header name: {name}");
        assert_eq!(
            value.trim(),
            http_user_agent(),
            "UA must be exactly the kaijutsu identity, not reqwest/none"
        );
    }

    /// Live API smoke test against api.deepseek.com. Gated behind
    /// `DEEPSEEK_API_KEY` so CI / casual `cargo test` skip it.
    ///
    /// ```sh
    /// DEEPSEEK_API_KEY=$(< ~/.deepseek-key) \
    ///   cargo test -p kaijutsu-kernel --lib deepseek_live \
    ///   -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "requires DEEPSEEK_API_KEY; run with `cargo test --ignored deepseek_live`"]
    async fn deepseek_live_smoke_streams_real_response() {
        let api_key = match std::env::var("DEEPSEEK_API_KEY") {
            Ok(k) if !k.is_empty() => k,
            _ => return,
        };
        let client = Client::new(api_key);
        let opts = BuildOpts::new("deepseek-v4-flash")
            .with_max_tokens(128)
            .with_system("You are friendly. Respond briefly.");
        let mut stream = client
            .stream(opts, vec![Message::user("hi there")])
            .await
            .expect("stream open must succeed with valid key");
        let mut text = String::new();
        let mut saw_done = false;
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;
        let mut cache_hit = 0u64;
        while let Some(ev) = stream.next_event().await {
            match ev {
                StreamEvent::TextDelta(t) => text.push_str(&t),
                StreamEvent::Done {
                    input_tokens: it,
                    output_tokens: ot,
                    extra,
                    ..
                } => {
                    saw_done = true;
                    input_tokens = it.unwrap_or(0);
                    output_tokens = ot.unwrap_or(0);
                    if let Some(UsageExtra::OpenAiCompat(d)) = extra {
                        cache_hit = d.prompt_cache_hit_tokens;
                    }
                }
                StreamEvent::Error(e) => panic!("live stream error: {e}"),
                _ => {}
            }
        }
        println!("\n--- deepseek said ---\n{text}\n--- meta ---");
        println!("tokens: in={input_tokens} out={output_tokens} cache_hit={cache_hit}\n");
        assert!(!text.is_empty(), "live response must include some text");
        assert!(saw_done, "live response must terminate with Done");
        assert!(input_tokens > 0, "usage must be captured from trailing chunk");
    }
}
