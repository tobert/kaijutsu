//! LLM provider abstraction for kaijutsu kernels.
//!
//! Per-provider modules (`claude`, `openai`, `deepseek`) own their native wire shapes
//! and translate kaijutsu's `Message` / `ContentBlock` into provider
//! requests. Dispatch is an explicit `match Provider` over a closed-set
//! enum — adding a provider means a new variant plus a new arm at each
//! call site. See `docs/unrig.md` for the rationale.
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────┐
//! │                       kaijutsu-kernel                         │
//! │  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐   │
//! │  │  Provider   │  │ StreamEvent │  │ Message / Content   │   │
//! │  │  (enum)     │  │  (CRDT-out) │  │ Block (kaijutsu)    │   │
//! │  └──────┬──────┘  └──────┬──────┘  └──────────┬──────────┘   │
//! │         │                │                    │              │
//! └─────────┼────────────────┼────────────────────┼──────────────┘
//!           │                │                    │
//!           ▼                ▼                    ▼
//!     ┌──────────────────────────────────────────────────┐
//!     │  llm/claude/  llm/openai/  llm/deepseek/          │
//!     │   Client::stream(opts, messages) → StreamEvent…  │
//!     └──────────────────────────────────────────────────┘
//! ```
//!
//! Claude, the OpenAI-compatible core, and DeepSeek are live. Gemini is
//! not implemented — add a real provider (or point the OpenAI-compatible
//! core at Google's OpenAI-shaped endpoint) when it's needed.

pub mod claude;
pub mod codex;
pub mod config;
pub mod db_config;
pub mod deepseek;
mod hydrate;
pub mod image_cache;
pub mod mailbox;
pub mod openai;
mod splice;
pub mod stream;
pub mod system_prompt;

// Re-export key types
pub use config::{
    BackendConfig, BackendKind, EmbeddingModelConfig, ModelAlias, ModelInfo, ResolvedSlot,
    SUPPORTED_BACKEND_KINDS, SlotTunables, unknown_backend_kind_message,
};
pub use db_config::{build_llm_registry, load_embedding_config};
pub use mailbox::ConversationMailbox;
pub use stream::{
    BuildOpts, CacheTarget, CacheTtl, ClaudeUsageExtra, FinishReason, OpenAiCompatUsageExtra,
    InlineToolResult, StreamError, StreamEvent, UsageExtra, apply_slot_tunables,
};
pub use system_prompt::{SituationalContext, build_system_prompt, extract_system_prompt_sections};

use serde::{Deserialize, Serialize};
use futures::{SinkExt, StreamExt};
use std::io;
use std::collections::HashMap;
use std::sync::Arc;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Default model to use when none specified.

/// Mock LLM client for testing — returns a canned response.
#[cfg(any(test, feature = "test-mock"))]
#[derive(Clone, Debug)]
pub struct MockClient {
    pub canned_response: String,
    /// Artificial latency applied to `prompt`/`prompt_with_system`, so a test
    /// can model a slow provider (e.g. exercising the distill `patient` hold).
    /// Zero by default; the streaming path ignores it.
    pub delay: std::time::Duration,
    /// Optional scripted event sequence for the streaming path — lets a test
    /// drive a real multi-iteration agentic turn (e.g. tool call → tool
    /// result → final text, each `Done` carrying distinct usage numbers) to
    /// exercise the context-usage accumulation model end-to-end, instead of
    /// the default single-shot text+Done reply below. Each `Provider::stream`
    /// call pops the next `Vec<StreamEvent>`; see `with_scripted_stream`.
    scripted: Option<Arc<parking_lot::Mutex<std::collections::VecDeque<Vec<stream::StreamEvent>>>>>,
    /// When true, a stream's `next_event()` never resolves once its scripted
    /// events run out — instead of returning `None` (a clean close). Models
    /// an HTTP connection that hangs rather than closing: the provider-side
    /// stand-in for "hard-cancelled, and the confirming flush never arrives"
    /// (deepseek review, `docs/issues.md`: hard-cancel-plus-hung-provider).
    /// Combine with `tokio::test(start_paused = true)` so a caller's idle
    /// timeout fires on virtual-clock auto-advance instead of real wall time.
    hangs_when_exhausted: bool,
}

#[cfg(any(test, feature = "test-mock"))]
impl MockClient {
    pub fn new(response: impl Into<String>) -> Self {
        Self {
            canned_response: response.into(),
            delay: std::time::Duration::ZERO,
            scripted: None,
            hangs_when_exhausted: false,
        }
    }

    /// Builder: make `prompt`/`prompt_with_system` sleep `delay` before
    /// returning the canned response.
    pub fn with_delay(mut self, delay: std::time::Duration) -> Self {
        self.delay = delay;
        self
    }

    /// Builder: script the streaming path's per-call event sequence. Each
    /// `Provider::stream` call pops the next entry, in order. Popping past
    /// the end panics loudly — a script that's too short is a test bug, not
    /// something to paper over by silently falling back to the default
    /// canned response (that would mask exactly the "how many LLM
    /// round-trips did this turn make" question these tests exist to pin
    /// down).
    pub fn with_scripted_stream(mut self, calls: Vec<Vec<stream::StreamEvent>>) -> Self {
        self.scripted = Some(Arc::new(parking_lot::Mutex::new(
            std::collections::VecDeque::from(calls),
        )));
        self
    }

    /// Builder: make the stream hang instead of closing once its scripted
    /// events are exhausted — see the `hangs_when_exhausted` field doc.
    pub fn hangs_when_exhausted(mut self) -> Self {
        self.hangs_when_exhausted = true;
        self
    }
}

/// Role of a message in a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Human/user message.
    User,
    /// Assistant/model message.
    Assistant,
}

/// Content block for structured message content (agentic loops).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContentBlock {
    /// Plain text content.
    Text { text: String },
    /// Tool use request from the model.
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Tool result for returning execution results.
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    /// Image content referenced by CAS hash.
    ///
    /// `data_base64` is `None` immediately after hydration — the hydrator
    /// is a pure function of `BlockSnapshot` and has no CAS access. The
    /// server-side path resolves the hash and fills `data_base64` before
    /// the request hits the LLM provider. Provider `build()` falls back to a
    /// text marker when resolution failed.
    Image {
        hash: String,
        media_type: String,
        data_base64: Option<String>,
    },
    /// Assistant reasoning preserved across tool-use iterations within a
    /// single agentic-loop turn (A3). Signature is provider-specific
    /// (Anthropic extended-thinking requires it for cross-tool-use turns);
    /// `None` is fine when extended thinking is not enabled.
    Reasoning {
        text: String,
        signature: Option<String>,
    },
}

/// Message content - either simple text or structured blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MessageContent {
    /// Simple text content.
    Text(String),
    /// Structured content blocks (for tool use/result).
    Blocks(Vec<ContentBlock>),
}

/// A message in a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Who sent this message.
    pub role: Role,
    /// Message content (text or blocks).
    pub content: MessageContent,
}

impl Message {
    /// Create a user message with text content.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: MessageContent::Text(content.into()),
        }
    }

    /// Create an assistant message with text content.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: MessageContent::Text(content.into()),
        }
    }

    /// Create a user message with tool results.
    pub fn tool_results(results: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::User,
            content: MessageContent::Blocks(results),
        }
    }

    /// Create an assistant message with tool uses.
    pub fn with_tool_uses(text: Option<String>, tool_uses: Vec<ContentBlock>) -> Self {
        Self::with_reasoning_text_and_tool_uses(Vec::new(), text, tool_uses)
    }

    /// Create an assistant message with reasoning, text, and tool uses. Used by
    /// the agentic-loop driver to preserve thinking across tool-use iterations
    /// (A3).
    ///
    /// `reasoning` is a list of `(text, signature)` pairs, **one per thinking
    /// block** — they are emitted in order as separate `Reasoning` blocks ahead
    /// of the text and tool uses, *not* merged. Anthropic requires each thinking
    /// block echoed back unmodified with its own signature (the signature is a
    /// verifier over that block's exact text), so collapsing distinct blocks
    /// would produce a signature mismatch. Empty-text entries are skipped.
    pub fn with_reasoning_text_and_tool_uses(
        reasoning: Vec<(String, Option<String>)>,
        text: Option<String>,
        tool_uses: Vec<ContentBlock>,
    ) -> Self {
        let mut blocks = Vec::new();
        for (reasoning_text, signature) in reasoning {
            if reasoning_text.is_empty() {
                continue;
            }
            blocks.push(ContentBlock::Reasoning {
                text: reasoning_text,
                signature,
            });
        }
        if let Some(t) = text {
            blocks.push(ContentBlock::Text { text: t });
        }
        blocks.extend(tool_uses);
        Self {
            role: Role::Assistant,
            content: MessageContent::Blocks(blocks),
        }
    }

    /// Get text content if this is a simple text message.
    pub fn as_text(&self) -> Option<&str> {
        match &self.content {
            MessageContent::Text(t) => Some(t),
            _ => None,
        }
    }
}

/// Tool definition for LLM API requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Tool name (e.g., "block.create").
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema for input parameters.
    pub input_schema: serde_json::Value,
}

/// A block of content in an LLM response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ResponseBlock {
    /// Model's extended thinking (reasoning before responding).
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
    /// Main text response.
    Text { text: String },
    /// Tool invocation request.
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Result from a tool execution.
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

impl ResponseBlock {
    /// Extract text content if this is a Text block.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ResponseBlock::Text { text } => Some(text),
            _ => None,
        }
    }

    /// Check if this is a thinking block.
    pub fn is_thinking(&self) -> bool {
        matches!(self, ResponseBlock::Thinking { .. })
    }

    /// Check if this is a tool use block.
    pub fn is_tool_use(&self) -> bool {
        matches!(self, ResponseBlock::ToolUse { .. })
    }
}

/// Token usage information.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens consumed.
    pub input_tokens: u64,
    /// Output tokens generated.
    pub output_tokens: u64,
}

impl Usage {
    /// Total tokens (input + output).
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

/// Error type for LLM operations.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// Provider not configured or unavailable.
    #[error("provider not available: {0}")]
    Unavailable(String),

    /// Authentication failed.
    #[error("authentication failed: {0}")]
    AuthError(String),

    /// Rate limited.
    #[error("rate limited: {0}")]
    RateLimited(String),

    /// Invalid request.
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// API error.
    #[error("api error: {0}")]
    ApiError(String),

    /// Network error.
    #[error("network error: {0}")]
    NetworkError(String),

    /// Provider-side completion error not covered by the variants above.
    #[error("completion error: {0}")]
    CompletionError(String),
}

impl kaijutsu_types::IntoErrorPayload for LlmError {
    fn into_error_payload(self) -> kaijutsu_types::ErrorPayload {
        use kaijutsu_types::{ErrorCategory, ErrorPayload, ErrorSeverity};
        let severity = match &self {
            LlmError::Unavailable(_) => ErrorSeverity::Fatal,
            LlmError::RateLimited(_) => ErrorSeverity::Warning,
            _ => ErrorSeverity::Error,
        };
        ErrorPayload {
            category: ErrorCategory::Stream,
            severity,
            code: None,
            detail: Some(self.to_string()),
            span: None,
            source_kind: None,
        }
    }
}

/// Result type for LLM operations.
pub type LlmResult<T> = Result<T, LlmError>;

/// Closed-set provider enum.
///
/// Dispatch is an explicit `match` at call sites — adding a provider is a
/// new variant plus a new arm wherever the enum is matched. The mock
/// variant exists only under `cfg(test)` / `feature = "test-mock"`; it
/// returns canned responses on `prompt_with_system` and refuses streaming
/// (matching the rig-era behavior).
#[derive(Clone, Debug)]
pub enum Provider {
    /// Anthropic Claude (see `llm/claude/`).
    Claude(claude::Client),
    /// DeepSeek — a preset over the OpenAI-compatible client, with required
    /// auth + the V4 reasoning-echo quirk (see `llm/deepseek/`).
    DeepSeek(deepseek::Client),
    /// Any other OpenAI-compatible chat-completions server: a local
    /// lemonade / llama.cpp server, Ollama, or OpenAI itself (see
    /// `llm/openai/`). Auth is optional; the provider name is config-driven.
    OpenAi(openai::Client),
    /// Experimental Codex app-server daemon, reached over a configured
    /// `ws://`/`wss://` endpoint. The provider never launches Codex itself.
    CodexApp(CodexAppClient),
    /// Mock provider for tests.
    #[cfg(any(test, feature = "test-mock"))]
    Mock(MockClient),
}

/// Connect-only Codex app-server backend configuration.
#[derive(Clone, Debug)]
pub struct CodexAppClient {
    name: String,
    endpoint: String,
    timeout: std::time::Duration,
}

struct CodexWsTransport {
    socket: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    timeout: std::time::Duration,
}

impl CodexWsTransport {
    async fn connect(endpoint: &str, timeout: std::time::Duration) -> codex::Result<Self> {
        let connected = tokio::time::timeout(timeout, connect_async(endpoint)).await.map_err(|_| {
            codex::CodexError::Io(io::Error::new(io::ErrorKind::TimedOut, "timed out connecting to Codex app-server"))
        })?;
        let (socket, _) = connected.map_err(|error| {
            codex::CodexError::Io(io::Error::new(io::ErrorKind::ConnectionRefused, error.to_string()))
        })?;
        Ok(Self { socket, timeout })
    }
}

#[async_trait::async_trait]
impl codex::JsonlTransport for CodexWsTransport {
    async fn receive(&mut self) -> codex::Result<Option<serde_json::Value>> {
        loop {
            let message = tokio::time::timeout(self.timeout, self.socket.next())
                .await
                .map_err(|_| {
                    codex::CodexError::Io(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timed out waiting for Codex app-server",
                    ))
                })?;
            match message {
                Some(Ok(WsMessage::Text(text))) => {
                    return Ok(Some(serde_json::from_str(text.as_ref())?));
                }
                Some(Ok(WsMessage::Binary(bytes))) => {
                    return Ok(Some(serde_json::from_slice(&bytes)?));
                }
                Some(Ok(WsMessage::Ping(payload))) => {
                    tokio::time::timeout(self.timeout, self.socket.send(WsMessage::Pong(payload)))
                        .await
                        .map_err(|_| {
                            codex::CodexError::Io(io::Error::new(
                                io::ErrorKind::TimedOut,
                                "timed out replying to Codex app-server ping",
                            ))
                        })?
                        .map_err(|error| {
                            codex::CodexError::Io(io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                error.to_string(),
                            ))
                        })?;
                }
                Some(Ok(WsMessage::Close(_))) | None => return Ok(None),
                Some(Ok(_)) => continue,
                Some(Err(error)) => {
                    return Err(codex::CodexError::Io(io::Error::new(
                        io::ErrorKind::ConnectionReset,
                        error.to_string(),
                    )));
                }
            }
        }
    }

    async fn send(&mut self, value: serde_json::Value) -> codex::Result<()> {
        let encoded = serde_json::to_string(&value)?;
        tokio::time::timeout(self.timeout, self.socket.send(WsMessage::Text(encoded.into())))
            .await
            .map_err(|_| {
                codex::CodexError::Io(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out sending to Codex app-server",
                ))
            })?
            .map_err(|error| {
                codex::CodexError::Io(io::Error::new(io::ErrorKind::BrokenPipe, error.to_string()))
            })
    }

    async fn close(&mut self) -> codex::Result<()> {
        tokio::time::timeout(self.timeout, self.socket.close(None))
            .await
            .map_err(|_| {
                codex::CodexError::Io(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out closing Codex app-server connection",
                ))
            })?
            .map_err(|error| {
            codex::CodexError::Io(io::Error::new(io::ErrorKind::BrokenPipe, error.to_string()))
        })
    }
}

pub struct CodexStream {
    client: codex::Client<CodexWsTransport>,
}

impl CodexAppClient {
    async fn stream(&self, opts: BuildOpts, messages: Vec<Message>) -> LlmResult<ProviderStream> {
        let transport = CodexWsTransport::connect(&self.endpoint, self.timeout)
            .await
            .map_err(|error| LlmError::NetworkError(error.to_string()))?;
        let mut client = codex::Client::new(transport);
        let dynamic_tools = opts
            .tools
            .iter()
            .map(|tool| {
                codex::DynamicToolSpec::Function(codex::DynamicToolFunction {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    input_schema: tool.input_schema.clone(),
                    defer_loading: false,
                })
            })
            .collect::<Vec<_>>();
        client
            .initialize_with_experimental_api(
                "kaijutsu",
                env!("CARGO_PKG_VERSION"),
                !dynamic_tools.is_empty(),
            )
            .await
            .map_err(|error| LlmError::ApiError(error.to_string()))?;
        let thread_id = client
            .start_thread(codex::ThreadStart {
                model: Some(opts.model.clone()),
                developer_instructions: opts.system.clone(),
                sandbox: Some("read-only".to_string()),
                approval_policy: Some("untrusted".to_string()),
                dynamic_tools,
                ..Default::default()
            })
            .await
            .map_err(|error| LlmError::ApiError(error.to_string()))?;
        client
            .start_turn(codex::TurnStart {
                thread_id,
                text: flatten_codex_messages(&messages),
                model: Some(opts.model),
                effort: opts.effort,
            })
            .await
            .map_err(|error| LlmError::ApiError(error.to_string()))?;
        Ok(ProviderStream::Codex(CodexStream { client }))
    }

    async fn prompt(&self, model: &str, system: Option<&str>, prompt: &str) -> LlmResult<String> {
        let mut stream = self
            .stream(
                BuildOpts {
                    model: model.to_string(),
                    system: system.map(str::to_owned),
                    ..BuildOpts::new(model)
                },
                vec![Message::user(prompt)],
            )
            .await?;
        let mut output = String::new();
        while let Some(event) = stream.next_event().await {
            match event {
                StreamEvent::TextDelta(delta) => output.push_str(&delta),
                StreamEvent::Done { .. } => break,
                StreamEvent::Error(error) => return Err(LlmError::ApiError(error)),
                _ => {}
            }
        }
        Ok(output)
    }
}

fn flatten_codex_messages(messages: &[Message]) -> String {
    messages
        .iter()
        .map(|message| {
            let role = match message.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            format!("[{role}] {}", flatten_codex_content(&message.content))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn flatten_codex_content(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(text) => text.clone(),
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text } => text.clone(),
                ContentBlock::ToolUse { name, input, .. } => {
                    format!("tool call {name}: {input}")
                }
                ContentBlock::ToolResult { content, is_error, .. } => {
                    format!("tool result (error={is_error}): {content}")
                }
                ContentBlock::Image { hash, media_type, .. } => {
                    format!("[image {media_type}, CAS {hash}]")
                }
                ContentBlock::Reasoning { text, .. } => format!("[reasoning] {text}"),
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Resolve a backend's API key, falling back to a placeholder when
/// `key_optional` is set (for a gateway where auth is network identity, not
/// the bearer token — the header still has to be present and non-empty,
/// but nothing downstream reads its value).
fn resolve_key_or_placeholder(config: &BackendConfig, label: &str) -> LlmResult<String> {
    match config.resolve_api_key() {
        Some(key) => Ok(key),
        None if config.key_optional => Ok("key-optional-placeholder".to_string()),
        None => Err(LlmError::AuthError(format!("No API key for {label}"))),
    }
}

/// Default per-request HTTP timeout applied when a backend's
/// `request_timeout_secs` is `NULL`. Generous enough that it almost never
/// fires before kaijutsu-server's own total-deadline
/// (`Timeouts::llm_request_timeout`, 300s / 5 minutes as of this writing —
/// `kaijutsu_types::timeout`) does, so the higher layer's cancel-and-warn
/// path is what normally governs a long-running generation. This is the
/// lower-level HTTP-client backstop for a connection that never gets a
/// response at all (a dead TCP peer, a proxy that swallows the request) —
/// 10 minutes comfortably outlasts the 5-minute default above it while
/// still bounding an indefinite hang, which is what an unset `reqwest`
/// timeout would otherwise allow.
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 600;

/// Resolve the per-request HTTP timeout for a backend: the configured
/// `request_timeout_secs` when set, else [`DEFAULT_REQUEST_TIMEOUT_SECS`].
/// `0` is refused at write time (`BackendConfig::validate`), so `Some(0)`
/// cannot reach here.
fn resolve_request_timeout(config: &BackendConfig) -> std::time::Duration {
    std::time::Duration::from_secs(
        config
            .request_timeout_secs
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS),
    )
}

impl Provider {
    /// Build a client from a resolved `backends` row.
    ///
    /// The [`BackendKind`] — not the name — picks the variant, which is the
    /// whole point of the name/kind split: a backend called `zorak` or
    /// `gateway-2` with `kind = openai` works, and two Anthropic endpoints can
    /// coexist. The backend NAME rides along on the OpenAI-compatible client
    /// (it labels the provider in wire logs), so a local server is finally
    /// nameable as itself.
    pub fn from_backend(config: &BackendConfig) -> LlmResult<Self> {
        match config.kind {
            BackendKind::Anthropic => {
                let api_key = resolve_key_or_placeholder(config, "Anthropic")?;
                let mut client = claude::Client::new(api_key)
                    .with_request_timeout(resolve_request_timeout(config));
                if let Some(ref url) = config.base_url {
                    client = client.with_base_url(url);
                }
                Ok(Self::Claude(client))
            }
            BackendKind::DeepSeek => {
                let api_key = resolve_key_or_placeholder(config, "DeepSeek")?;
                let mut client = deepseek::Client::new(api_key)
                    .with_request_timeout(resolve_request_timeout(config));
                // DeepSeek's endpoint is fixed; a base_url on this kind is
                // accepted and honored rather than silently discarded, but the
                // `kj backend set` help says not to bother.
                if let Some(ref url) = config.base_url {
                    client = client.with_base_url(url);
                }
                Ok(Self::DeepSeek(client))
            }
            // Any OpenAI-compatible `/chat/completions` server. Auth is
            // optional — a local server needs no key — so a missing key is
            // not an error here.
            BackendKind::OpenAi => {
                let mut client = openai::Client::new(config.name.clone())
                    .with_request_timeout(resolve_request_timeout(config));
                if let Some(ref url) = config.base_url {
                    client = client.with_base_url(url);
                }
                if let Some(key) = config.resolve_api_key() {
                    client = client.with_api_key(key);
                }
                Ok(Self::OpenAi(client))
            }
            BackendKind::CodexApp => {
                let endpoint = config.base_url.clone().ok_or_else(|| {
                    LlmError::InvalidRequest(format!(
                        "backend '{}' has kind 'codex-app', which requires --base-url \
                         (ws:// or wss:// Codex app-server endpoint)",
                        config.name
                    ))
                })?;
                if !(endpoint.starts_with("ws://") || endpoint.starts_with("wss://")) {
                    return Err(LlmError::InvalidRequest(format!(
                        "backend '{}' codex-app endpoint must use ws:// or wss://, got {}",
                        config.name, endpoint
                    )));
                }
                Ok(Self::CodexApp(CodexAppClient {
                    name: config.name.clone(),
                    endpoint,
                    timeout: resolve_request_timeout(config),
                }))
            }
            #[cfg(any(test, feature = "test-mock"))]
            BackendKind::Mock => Ok(Self::Mock(MockClient::new(format!(
                "Mock summary for testing (backend: {}).",
                config.name
            )))),
        }
    }

    /// Create a Claude provider from `ANTHROPIC_API_KEY`.
    pub fn anthropic_from_env() -> LlmResult<Self> {
        let config = BackendConfig::new("anthropic", BackendKind::Anthropic)
            .with_api_key_env("ANTHROPIC_API_KEY");
        Self::from_backend(&config)
    }

    /// Create a DeepSeek provider from `DEEPSEEK_API_KEY`.
    pub fn deepseek_from_env() -> LlmResult<Self> {
        let config = BackendConfig::new("deepseek", BackendKind::DeepSeek)
            .with_api_key_env("DEEPSEEK_API_KEY");
        Self::from_backend(&config)
    }

    /// Stable identifier for the *kind* of wire this client speaks.
    ///
    /// NOT the backend name — an `OpenAi` client borrows its config-supplied
    /// name (which for the OpenAI-compatible kind IS the backend name), while
    /// Claude/DeepSeek report their kind. Registry keys are backend names;
    /// use those when you need the handle.
    pub fn name(&self) -> &str {
        match self {
            Self::Claude(_) => "anthropic",
            Self::DeepSeek(_) => "deepseek",
            Self::OpenAi(c) => c.provider_name(),
            Self::CodexApp(c) => &c.name,
            #[cfg(any(test, feature = "test-mock"))]
            Self::Mock(_) => "mock",
        }
    }

    /// One-shot prompt — sends a single user message.
    #[tracing::instrument(skip(self, prompt), fields(llm.model = %model, llm.provider = self.name()))]
    pub async fn prompt(&self, model: &str, prompt: &str) -> LlmResult<String> {
        self.prompt_with_system(model, None, prompt).await
    }

    /// One-shot prompt with optional system preamble.
    ///
    /// Claude / DeepSeek / OpenAi hit their provider; Mock returns its
    /// canned response (used by `KjDispatcher::summarize` in tests).
    #[tracing::instrument(skip(self, system, prompt), fields(llm.model = %model, llm.provider = self.name()))]
    pub async fn prompt_with_system(
        &self,
        model: &str,
        system: Option<&str>,
        prompt: &str,
    ) -> LlmResult<String> {
        match self {
            Self::Claude(client) => client.prompt(model, system, prompt).await,
            Self::DeepSeek(client) => client.prompt(model, system, prompt).await,
            Self::OpenAi(client) => client.prompt(model, system, prompt).await,
            Self::CodexApp(client) => client.prompt(model, system, prompt).await,
            #[cfg(any(test, feature = "test-mock"))]
            Self::Mock(mock) => {
                if !mock.delay.is_zero() {
                    tokio::time::sleep(mock.delay).await;
                }
                Ok(mock.canned_response.clone())
            }
        }
    }

    /// Start a streaming completion.
    ///
    /// Claude / DeepSeek / OpenAi stream from their provider; Mock
    /// replays its canned response as a well-formed text stream
    /// (`TextStart → TextDelta → TextEnd → Done`) so tests can exercise the
    /// full streaming turn — including the autonomous fork-and-act path —
    /// without a live provider.
    #[tracing::instrument(skip(self, opts, messages), fields(llm.provider = self.name()))]
    pub async fn stream(
        &self,
        opts: BuildOpts,
        messages: Vec<Message>,
    ) -> LlmResult<ProviderStream> {
        match self {
            Self::Claude(client) => {
                let stream = client.stream(opts, messages).await?;
                Ok(ProviderStream::Claude(stream))
            }
            // DeepSeek delegates to the OpenAI-compatible client, so both
            // yield an `openai::Stream` under the one `ProviderStream::OpenAi`.
            Self::DeepSeek(client) => {
                let stream = client.stream(opts, messages).await?;
                Ok(ProviderStream::OpenAi(stream))
            }
            Self::OpenAi(client) => {
                let stream = client.stream(opts, messages).await?;
                Ok(ProviderStream::OpenAi(stream))
            }
            Self::CodexApp(client) => client.stream(opts, messages).await,
            #[cfg(any(test, feature = "test-mock"))]
            Self::Mock(mock) => {
                let events = if let Some(script) = &mock.scripted {
                    script.lock().pop_front().unwrap_or_else(|| {
                        panic!(
                            "MockClient scripted stream exhausted — the agentic loop called \
                             stream() more times than the test scripted; add another call's \
                             worth of events or fix the test's iteration expectations"
                        )
                    })
                } else {
                    vec![
                        StreamEvent::TextStart,
                        StreamEvent::TextDelta(mock.canned_response.clone()),
                        StreamEvent::TextEnd,
                        StreamEvent::Done {
                            stop_reason: Some("end_turn".into()),
                            input_tokens: Some(0),
                            output_tokens: Some(0),
                            extra: None,
                        },
                    ]
                };
                Ok(ProviderStream::Mock(MockStream {
                    events: std::collections::VecDeque::from(events),
                    hangs_when_exhausted: mock.hangs_when_exhausted,
                }))
            }
        }
    }

    /// Models this provider exposes by default.
    pub fn available_models(&self) -> Vec<&'static str> {
        match self {
            Self::Claude(c) => c.available_models(),
            Self::DeepSeek(c) => c.available_models(),
            Self::OpenAi(c) => c.available_models(),
            Self::CodexApp(_) => Vec::new(),
            #[cfg(any(test, feature = "test-mock"))]
            Self::Mock(_) => vec!["mock-model"],
        }
    }
}

/// Closed-set wrapper over per-provider stream types.
///
/// The CRDT block writer in `kaijutsu-server` consumes [`StreamEvent`]s
/// via [`Self::next_event`] and signals cancellation via [`Self::cancel`].
/// Each call dispatches with an explicit `match` — no trait object.
pub enum ProviderStream {
    Claude(claude::Stream),
    /// Every OpenAI-compatible provider (DeepSeek and the generic `OpenAi`
    /// variant) streams through the one `openai::Stream`.
    OpenAi(openai::Stream),
    /// Codex app-server's translated event stream.
    Codex(CodexStream),
    /// Replays a pre-built event queue. Lets tests drive a real streaming
    /// turn (e.g. the autonomous fork-and-act path) without a live provider.
    #[cfg(any(test, feature = "test-mock"))]
    Mock(MockStream),
}

/// Backing state for `ProviderStream::Mock` — see
/// `MockClient::hangs_when_exhausted` for why this is more than a bare
/// `VecDeque`.
#[cfg(any(test, feature = "test-mock"))]
pub struct MockStream {
    events: std::collections::VecDeque<StreamEvent>,
    hangs_when_exhausted: bool,
}

impl ProviderStream {
    /// Poll for the next stream event. Returns `None` once the stream is
    /// exhausted (after `Done` or `Error`).
    pub async fn next_event(&mut self) -> Option<StreamEvent> {
        match self {
            Self::Claude(s) => s.next_event().await,
            Self::OpenAi(s) => s.next_event().await,
            Self::Codex(s) => s.client.next_event().await,
            #[cfg(any(test, feature = "test-mock"))]
            Self::Mock(state) => match state.events.pop_front() {
                Some(ev) => Some(ev),
                None if state.hangs_when_exhausted => std::future::pending().await,
                None => None,
            },
        }
    }

    /// Return the result of a provider-owned, in-flight tool request.
    ///
    /// Ordinary `ToolUse` calls end an LLM completion and are driven by the
    /// server's existing agentic loop.  This narrow seam is for runtimes that
    /// keep one turn alive while asking their client to execute a dynamic
    /// tool.  Unsupported providers fail loudly rather than dropping a tool
    /// result and leaving their turn hung.
    pub async fn respond_inline_tool(
        &mut self,
        id: &str,
        result: InlineToolResult,
    ) -> Result<(), String> {
        match self {
            Self::Codex(s) => s
                .client
                .respond_inline_tool(id, result)
                .await
                .map_err(|error| error.to_string()),
            Self::Claude(_) | Self::OpenAi(_) => Err(format!(
                "provider does not support inline tool response for callback {id}"
            )),
            #[cfg(any(test, feature = "test-mock"))]
            Self::Mock(_) => Err(format!(
                "mock provider does not support inline tool response for callback {id}"
            )),
        }
    }

    /// Abort the underlying HTTP stream.
    pub fn cancel(&self) {
        match self {
            Self::Claude(s) => s.cancel(),
            Self::OpenAi(s) => s.cancel(),
            // Phase 0 has no synchronous interrupt hook in ProviderStream;
            // dropping the stream closes the daemon connection at turn end.
            Self::Codex(_) => {}
            #[cfg(any(test, feature = "test-mock"))]
            Self::Mock(_) => {}
        }
    }
}

/// Registry of live LLM backends, keyed by backend NAME.
///
/// A **snapshot** of the DB config, not a view onto it: the whole thing is
/// rebuilt from `kernel_db` and swapped in behind the kernel's
/// `RwLock<LlmRegistry>` whenever a `kj backend`/`kj cast`/`kj alias` write
/// lands (see `db_config::build_llm_registry` and
/// `KjDispatcher::reload_llm_registry`). Reads stay lock-cheap on the hot turn
/// path, and config changes take effect without a kernel restart.
#[derive(Default)]
pub struct LlmRegistry {
    providers: HashMap<String, Arc<Provider>>,
    default_provider: Option<String>,
    default_model: Option<String>,
    model_aliases: HashMap<String, config::ModelAlias>,
    backends: HashMap<String, BackendConfig>,
    /// The `llm_defaults` tunable floor a cast slot cascades onto.
    default_tunables: SlotTunables,
    /// Resolved cast slots keyed by `(cast_label_lowercase, role)`. Stored so
    /// Track B can ask "what does cast X say for role Y" without a DB hop on
    /// the turn path.
    cast_slots: HashMap<(String, String), ResolvedSlot>,
}

impl std::fmt::Debug for LlmRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmRegistry")
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .field("default_provider", &self.default_provider)
            .field("default_model", &self.default_model)
            .field(
                "model_aliases",
                &self.model_aliases.keys().collect::<Vec<_>>(),
            )
            .field("casts", &self.cast_labels())
            .finish()
    }
}

impl LlmRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a provider by name.
    pub fn register(&mut self, name: impl Into<String>, provider: Arc<Provider>) {
        self.providers.insert(name.into(), provider);
    }

    /// Get a provider by name.
    pub fn get(&self, name: &str) -> Option<Arc<Provider>> {
        self.providers.get(name).cloned()
    }

    /// Set the default provider.
    pub fn set_default(&mut self, name: &str) -> bool {
        if self.providers.contains_key(name) {
            self.default_provider = Some(name.to_string());
            true
        } else {
            false
        }
    }

    /// Get the default provider name.
    pub fn default_provider_name(&self) -> Option<&str> {
        self.default_provider.as_deref()
    }

    /// Set the default model.
    pub fn set_default_model(&mut self, model: impl Into<String>) {
        self.default_model = Some(model.into());
    }

    /// Get the default provider.
    pub fn default_provider(&self) -> Option<Arc<Provider>> {
        self.default_provider
            .as_ref()
            .and_then(|name| self.get(name))
    }

    /// Get the default model.
    pub fn default_model(&self) -> Option<&str> {
        self.default_model.as_deref()
    }

    /// Maximum RESPONSE tokens, from `llm_defaults.max_tokens`, falling back
    /// to 64000 when unset.
    ///
    /// Set generously — the API enforces per-model ceilings. This is the
    /// global floor; a cast slot's own `max_tokens` overrides it, which Track
    /// B consumes via [`Self::resolved_slot`].
    pub fn max_output_tokens(&self) -> u64 {
        self.default_tunables.max_tokens.unwrap_or(64000)
    }

    /// The `llm_defaults` tunable floor.
    pub fn default_tunables(&self) -> &SlotTunables {
        &self.default_tunables
    }

    /// Get a backend's config by name.
    pub fn backend_config(&self, name: &str) -> Option<&BackendConfig> {
        self.backends.get(name)
    }

    /// Every configured backend, ordered by name.
    pub fn backend_configs(&self) -> Vec<&BackendConfig> {
        let mut v: Vec<&BackendConfig> = self.backends.values().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// Install the backend configs (registry construction; see `db_config`).
    pub fn set_backends(&mut self, backends: Vec<BackendConfig>) {
        self.backends = backends.into_iter().map(|b| (b.name.clone(), b)).collect();
    }

    /// Install the `llm_defaults` tunable floor.
    pub fn set_default_tunables(&mut self, tunables: SlotTunables) {
        self.default_tunables = tunables;
    }

    /// Install the resolved cast slots (registry construction).
    pub fn set_cast_slots(&mut self, slots: Vec<(String, ResolvedSlot)>) {
        self.cast_slots = slots
            .into_iter()
            .map(|(cast, slot)| ((cast.to_lowercase(), slot.role.clone()), slot))
            .collect();
    }

    /// One cast's seat for a role, with the `llm_defaults` cascade already
    /// applied. Cast labels are case-insensitive (matching the UNIQUE
    /// collation); roles are exact.
    ///
    /// Nothing on the turn path reads this yet — **Track B** wires cast
    /// selection into context creation and the request builder.
    pub fn resolved_slot(&self, cast: &str, role: &str) -> Option<&ResolvedSlot> {
        self.cast_slots.get(&(cast.to_lowercase(), role.to_string()))
    }

    /// Every configured cast label, sorted.
    pub fn cast_labels(&self) -> Vec<String> {
        let mut labels: Vec<String> = self
            .cast_slots
            .keys()
            .map(|(cast, _)| cast.clone())
            .collect();
        labels.sort();
        labels.dedup();
        labels
    }

    /// Configured context window for a resolved `(provider, model)` pair,
    /// or `None` when unconfigured.
    ///
    /// This is the one resolution path for "how big is this model's
    /// context window" — it reads the same `provider_configs` that
    /// [`Self::provider_config`] and `kj model`/`kj models` already resolve
    /// through, so there is no second alias-aware lookup that could
    /// disagree with it. Callers pass an
    /// already-resolved provider + model (e.g. the context row's columns,
    /// or the registry default), not an alias name. `None` must be
    /// rendered as an explicit "unknown" by every caller — never a
    /// fabricated default.
    pub fn context_window_for(&self, provider_name: &str, model: &str) -> Option<u64> {
        self.backend_config(provider_name)?.context_window(model)
    }

    /// Live-lookup variant of [`Self::context_window_for`].
    ///
    /// Precedence: the config-declared `context_window` (backend_models) is
    /// checked FIRST and, when present, returned immediately — config is
    /// always the override, and no network is attempted if it hits. Only
    /// when config has no entry AND the resolved provider is Anthropic does
    /// this fall through to the provider's live `GET /v1/models/{id}`
    /// lookup (see `claude::Client::context_window`, which caches the
    /// result for the process lifetime). Every other provider — including
    /// an unregistered one — falls through to `None`, exactly as
    /// [`Self::context_window_for`] already did; non-Anthropic providers
    /// keep the config-only path (this is additive, not a replacement).
    ///
    /// Never fabricates: a live-lookup failure degrades to `None` (logged
    /// by the client at `warn`), never a guessed number. Callers must keep
    /// treating `None` as an honest "unknown" — never substitute a default.
    pub async fn context_window_for_live(&self, provider_name: &str, model: &str) -> Option<u64> {
        if let Some(window) = self.context_window_for(provider_name, model) {
            return Some(window);
        }
        match self.get(provider_name).as_deref() {
            Some(Provider::Claude(client)) => client.context_window(model).await,
            _ => None,
        }
    }

    /// Set model aliases. Keys are normalized to lowercase so lookups match
    /// the `COLLATE NOCASE` semantics the `model_aliases` table enforces.
    pub fn set_model_aliases(&mut self, aliases: HashMap<String, config::ModelAlias>) {
        self.model_aliases = aliases
            .into_iter()
            .map(|(k, v)| (k.to_lowercase(), v))
            .collect();
    }

    /// All configured model aliases (friendly name → backend/model).
    ///
    /// Exposed for discovery surfaces (`kj models`) that enumerate the
    /// `--model` specs a caller can name. Read-only borrow of the map.
    pub fn model_aliases(&self) -> &HashMap<String, config::ModelAlias> {
        &self.model_aliases
    }

    /// Resolve a model name through aliases.
    ///
    /// If the name matches an alias, returns the (backend, model) tuple.
    /// Otherwise returns None, meaning the name should be used as-is.
    ///
    /// Alias lookup is case-insensitive, matching the `model_aliases.alias`
    /// `COLLATE NOCASE` PRIMARY KEY — the DB will not let two aliases differ
    /// only by case, so a case-sensitive resolver here could only ever fail to
    /// find a row that exists.
    pub fn resolve_alias(&self, name: &str) -> Option<(&str, &str)> {
        let key = name.to_lowercase();
        self.model_aliases
            .get(&key)
            .map(|a| (a.backend.as_str(), a.model.as_str()))
    }

    // `resolve_model` lived here and is deliberately GONE (2026-08-12). It
    // resolved an alias, and on a miss silently pinned the bare name onto the
    // registry's *default provider* — the sharp edge behind the 2026-07-04
    // cross-provider distill bug, which was fixed by routing around it rather
    // than by changing it. Every caller has since migrated to
    // `kj::parse::resolve_model_choice`, which parses the `provider/model`
    // slash form and refuses to guess a provider. The old function was left
    // with zero callers, which made it a loaded name waiting to be reached
    // for. Use `resolve_model_choice`; if you need alias-only resolution,
    // `resolve_alias` is right here and does not invent a provider.

    /// List all registered providers.
    pub fn list(&self) -> Vec<&str> {
        self.providers.keys().map(|s| s.as_str()).collect()
    }

    /// List all known model IDs for a backend: every model with a
    /// `backend_models` row, plus every model any alias points at.
    ///
    /// This is a discovery listing, not a claim about what the endpoint
    /// serves — an OpenAI-compatible server will happily accept a model id
    /// nobody wrote down.
    pub fn models_for_provider(&self, provider_name: &str) -> Vec<String> {
        let mut models: Vec<String> = self
            .model_aliases
            .values()
            .filter(|a| a.backend == provider_name)
            .map(|a| a.model.clone())
            .collect();
        if let Some(backend) = self.backends.get(provider_name) {
            models.extend(backend.models.keys().cloned());
        }
        models.sort();
        models.dedup();
        models
    }

    /// Quick prompt using default provider and model.
    #[tracing::instrument(skip(self, prompt))]
    pub async fn prompt(&self, prompt: &str) -> LlmResult<String> {
        let provider = self
            .default_provider()
            .ok_or_else(|| LlmError::Unavailable("no default provider set".into()))?;

        let model = self
            .default_model
            .as_deref()
            .or_else(|| provider.available_models().first().copied())
            .ok_or_else(|| LlmError::Unavailable("no default model set".into()))?;

        provider.prompt(model, prompt).await
    }
}

// ============================================================================
// Hydration: BlockSnapshot[] → Message[]
// ============================================================================

/// Resolve `ContentBlock::Image` placeholders against a content store.
///
/// The hydrator emits image blocks with `data_base64: None` because it has no
/// CAS access. Callers (typically the LLM stream pipeline) invoke this
/// helper to fill the data before passing messages to the provider. Unknown
/// hashes and CAS errors are tolerated: the block stays unfilled, and
/// the provider's `build()` falls back to a text marker so the model knows
/// an image existed at that turn.
///
/// CAS reads use blocking `std::fs` (see `kaijutsu_cas::FileStore`), so each
/// image is read on a `spawn_blocking` worker — keeps the tokio runtime
/// responsive when a long-history conversation re-encodes many images per
/// prompt.
///
/// When `cache` is `Some`, already-resolved hashes skip the disk read and
/// base64 encode entirely. Hashes are content-addressed, so a global cache
/// across contexts is correct.
pub async fn resolve_image_blocks_from_cas(
    messages: &mut [Message],
    cas: std::sync::Arc<dyn kaijutsu_cas::ContentStore>,
    cache: Option<&image_cache::ImageBase64Cache>,
) {
    use base64::Engine;
    use image_cache::ResolvedImage;
    use kaijutsu_cas::ContentHash;

    for msg in messages.iter_mut() {
        let MessageContent::Blocks(blocks) = &mut msg.content else {
            continue;
        };
        for block in blocks.iter_mut() {
            let ContentBlock::Image {
                hash,
                media_type,
                data_base64,
            } = block
            else {
                continue;
            };
            if data_base64.is_some() {
                continue;
            }
            let parsed = match ContentHash::from_str_checked(hash) {
                Ok(h) => h,
                Err(_) => continue,
            };
            if let Some(cache) = cache
                && let Some(hit) = cache.get(&parsed)
            {
                *media_type = hit.mime_type;
                *data_base64 = Some(hit.data_base64);
                continue;
            }
            // Both inspect (sidecar mime) and retrieve (object bytes) are
            // blocking std::fs reads — bundle them into one spawn_blocking
            // so the runtime stays responsive even for stacks of images.
            let cas_for_task = cas.clone();
            let parsed_for_task = parsed.clone();
            let join = tokio::task::spawn_blocking(move || {
                let inspected = cas_for_task.inspect(&parsed_for_task).ok().flatten();
                let bytes = cas_for_task.retrieve(&parsed_for_task).ok().flatten();
                (inspected, bytes)
            })
            .await;
            let (inspected, bytes) = match join {
                Ok(pair) => pair,
                Err(_) => continue, // worker panicked; leave block unresolved
            };
            // Prefer CAS-recorded mime over the hydrator's defaulted one;
            // CAS sidecar metadata reflects what was actually stored.
            if let Some(reference) = inspected {
                *media_type = reference.mime_type;
            }
            if let Some(bytes) = bytes {
                let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
                if let Some(cache) = cache {
                    cache.insert(
                        parsed,
                        ResolvedImage {
                            mime_type: media_type.clone(),
                            data_base64: encoded.clone(),
                        },
                    );
                }
                *data_base64 = Some(encoded);
            }
            // Otherwise stay unresolved — provider build() emits a text marker.
        }
    }
}

/// Crate-external entry point for [`hydrate::estimate_tokens`] — the
/// pre-flight token-size estimate walked over an already-hydrated message
/// sequence. `hydrate` is a private submodule (`mod hydrate;` above), so this
/// thin `pub fn` is the seam that lets `kaijutsu-server`'s pre-send
/// context-window warning (`llm_stream.rs`) reach it without widening the
/// submodule's own visibility.
pub fn estimate_tokens(messages: &[Message]) -> u64 {
    hydrate::estimate_tokens(messages)
}

/// Reconstruct LLM conversation history from stored blocks.
///
/// Walks blocks in order and produces the `Message` sequence expected by the
/// LLM API. Skips thinking, file, and empty blocks.
/// Drift blocks are included as User messages with a provenance prefix.
///
/// Preserves `tool_use_id` from blocks when available, falling back to
/// `BlockId::to_key()` for pre-migration blocks.
///
/// **Trailing-tool-use guard:** If the last message is an assistant with
/// tool_uses but no following tool_results, synthesizes error results so the
/// LLM API doesn't reject the request.
pub fn hydrate_from_blocks(blocks: &[kaijutsu_types::BlockSnapshot]) -> Vec<Message> {
    // Index for parent lookups — only the Error branch consults it, and
    // only for `parent_id`. Incremental callers pass the parent
    // directly to `translate_block` and don't need an index.
    let blocks_by_id: HashMap<kaijutsu_types::BlockId, &kaijutsu_types::BlockSnapshot> =
        blocks.iter().map(|b| (b.id, b)).collect();

    let mut state = hydrate::HydrationState::new();
    for block in blocks {
        let parent = block
            .parent_id
            .and_then(|pid| blocks_by_id.get(&pid).copied());
        state.translate_block(block, parent);
    }
    state.into_messages()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_constructors() {
        let user = Message::user("hello");
        assert_eq!(user.role, Role::User);
        assert_eq!(user.as_text(), Some("hello"));

        let assistant = Message::assistant("hi there");
        assert_eq!(assistant.role, Role::Assistant);
        assert_eq!(assistant.as_text(), Some("hi there"));
    }

    #[test]
    fn test_message_tool_results() {
        let results = vec![ContentBlock::ToolResult {
            tool_use_id: "tool_123".to_string(),
            content: "result".to_string(),
            is_error: false,
        }];
        let msg = Message::tool_results(results);
        assert_eq!(msg.role, Role::User);
        assert!(msg.as_text().is_none());
        match &msg.content {
            MessageContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 1);
            }
            _ => panic!("Expected blocks"),
        }
    }

    #[test]
    fn content_block_reasoning_serde_roundtrip() {
        let block = ContentBlock::Reasoning {
            text: "let me work through this".to_string(),
            signature: Some("provider-sig-xyz".to_string()),
        };
        let json = serde_json::to_string(&block).unwrap();
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        match back {
            ContentBlock::Reasoning { text, signature } => {
                assert_eq!(text, "let me work through this");
                assert_eq!(signature.as_deref(), Some("provider-sig-xyz"));
            }
            other => panic!("expected Reasoning, got {other:?}"),
        }
    }

    #[test]
    fn test_usage() {
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 50,
        };
        assert_eq!(usage.total(), 150);
    }

    #[test]
    fn test_tool_definition_roundtrip_serde() {
        // Provider-specific translation now lives in each provider's `build()`;
        // here we just verify the canonical kaijutsu form round-trips through serde.
        let td = ToolDefinition {
            name: "test_tool".into(),
            description: "A test tool".into(),
            input_schema: serde_json::json!({"type": "object"}),
        };

        let json = serde_json::to_string(&td).unwrap();
        let back: ToolDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, td.name);
        assert_eq!(back.description, td.description);
    }

    #[test]
    fn test_provider_names() {
        assert_eq!(
            Provider::Claude(claude::Client::new("fake")).name(),
            "anthropic"
        );
        assert_eq!(
            Provider::DeepSeek(deepseek::Client::new("fake")).name(),
            "deepseek"
        );
    }

    /// An unknown backend KIND must name it precisely and list the closed
    /// set — and must NOT guess about API keys (that guess belongs to the
    /// actual `AuthError` case below). Regression for the 2026-06-30 config
    /// papercut: `local-e4b` was reported as "Unknown or unsupported provider
    /// type" with no indication that the fix is to name a real kind.
    ///
    /// Note the shape change: under models.toml the *table name* was the
    /// type, so `local-e4b` was an unknown TYPE. Now a backend named
    /// `local-e4b` is perfectly legal — what must be a known token is its
    /// `kind`, and `BackendKind::parse` is where that is enforced.
    #[test]
    fn unknown_backend_kind_names_it_precisely_no_key_guess() {
        let msg = BackendKind::parse("local-e4b").unwrap_err();
        assert!(msg.contains("unknown backend kind 'local-e4b'"), "msg: {msg}");
        assert!(msg.contains("supported: anthropic, deepseek, openai"), "msg: {msg}");
        assert!(
            !msg.to_lowercase().contains("key"),
            "unknown-kind error must not guess about API keys: {msg}"
        );
    }

    /// A genuine credential failure (no resolvable API key) is the ONLY case
    /// that should mention keys — the flip side of the assertion above. Uses
    /// an explicit bogus `api_key_env` (rather than relying on
    /// `ANTHROPIC_API_KEY` being unset) so the test is deterministic
    /// regardless of the host's real environment.
    #[test]
    fn from_backend_missing_key_is_an_auth_error_mentioning_key() {
        let config = BackendConfig::new("anthropic", BackendKind::Anthropic)
            .with_api_key_env("KAIJUTSU_TEST_DEFINITELY_UNSET_XYZ");
        let err = Provider::from_backend(&config).unwrap_err();
        assert!(matches!(err, LlmError::AuthError(_)), "got {err:?}");
        assert!(
            err.to_string().to_lowercase().contains("key"),
            "auth error should mention key: {err}"
        );
    }

    #[test]
    fn resolve_request_timeout_uses_configured_value_when_set() {
        let mut config =
            BackendConfig::new("anthropic", BackendKind::Anthropic).with_api_key_env("X");
        config.request_timeout_secs = Some(45);
        assert_eq!(
            resolve_request_timeout(&config),
            std::time::Duration::from_secs(45)
        );
    }

    #[test]
    fn resolve_request_timeout_falls_back_to_default_when_null() {
        let config = BackendConfig::new("anthropic", BackendKind::Anthropic);
        assert_eq!(config.request_timeout_secs, None);
        assert_eq!(
            resolve_request_timeout(&config),
            std::time::Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS)
        );
    }

    /// A `key_optional` backend registers with a placeholder rather than
    /// failing — the local-gateway path, unchanged by the renovation.
    #[test]
    fn key_optional_backend_registers_without_a_key() {
        let config = BackendConfig::new("ollama", BackendKind::OpenAi)
            .with_base_url("http://localhost:11434/v1")
            .with_key_optional(true);
        assert!(Provider::from_backend(&config).is_ok());
    }

    /// The name/kind split in one assertion: two differently-NAMED backends
    /// of the same KIND both build, and the OpenAI-compatible client carries
    /// the backend's own name. `models.toml` could not express this.
    #[test]
    fn two_openai_kind_backends_keep_their_own_names() {
        let gpt = BackendConfig::new("gpt", BackendKind::OpenAi)
            .with_base_url("https://api.openai.com/v1")
            .with_key_optional(true);
        let zorak = BackendConfig::new("zorak", BackendKind::OpenAi)
            .with_base_url("http://zorak:8080/v1")
            .with_key_optional(true);
        assert_eq!(Provider::from_backend(&gpt).unwrap().name(), "gpt");
        assert_eq!(Provider::from_backend(&zorak).unwrap().name(), "zorak");
    }

    #[test]
    fn test_registry_concrete_type() {
        let mut registry = LlmRegistry::new();
        let provider = Arc::new(Provider::Claude(claude::Client::new("fake")));
        registry.register("anthropic", provider);
        registry.set_default("anthropic");

        assert!(registry.default_provider().is_some());
        assert_eq!(registry.list(), vec!["anthropic"]);
    }

    #[test]
    fn test_model_alias_resolution() {
        let mut registry = LlmRegistry::new();
        let provider = Arc::new(Provider::Claude(claude::Client::new("fake")));
        registry.register("anthropic", provider);
        registry.set_default("anthropic");

        let mut aliases = HashMap::new();
        aliases.insert(
            "fast".to_string(),
            ModelAlias {
                backend: "anthropic".to_string(),
                model: "claude-haiku-4-5-20251001".to_string(),
            },
        );
        registry.set_model_aliases(aliases);

        assert!(registry.resolve_alias("fast").is_some());
        let (prov, model) = registry.resolve_alias("fast").unwrap();
        assert_eq!(prov, "anthropic");
        assert_eq!(model, "claude-haiku-4-5-20251001");
        assert!(registry.resolve_alias("nonexistent").is_none());
    }

    #[test]
    fn context_window_for_resolves_via_backend_config() {
        let mut registry = LlmRegistry::new();
        let mut anthropic = BackendConfig::new("anthropic", BackendKind::Anthropic);
        anthropic.models.insert(
            "claude-opus-4-8".to_string(),
            config::ModelInfo {
                context_window: Some(1_000_000),
                extra: None,
            },
        );
        registry.set_backends(vec![anthropic]);

        assert_eq!(
            registry.context_window_for("anthropic", "claude-opus-4-8"),
            Some(1_000_000)
        );
    }

    #[test]
    fn context_window_for_unconfigured_model_is_none_not_a_default() {
        let mut registry = LlmRegistry::new();
        let anthropic = BackendConfig::new("anthropic", BackendKind::Anthropic); // no models configured
        registry.set_backends(vec![anthropic]);

        assert_eq!(
            registry.context_window_for("anthropic", "claude-haiku-4-5-20251001"),
            None,
            "a model with no configured window must resolve to None, never a guessed default"
        );
    }

    #[test]
    fn context_window_for_unknown_provider_is_none() {
        let registry = LlmRegistry::new(); // no backends set at all
        assert_eq!(registry.context_window_for("anthropic", "anything"), None);
    }

    // ── context_window_for_live: config override / live fallback / seam ──

    mod context_window_for_live_tests {
        use super::*;
        use crate::llm::claude::models_api::test_support::FakeModelCapabilitySource;

        /// Build a registry with a `Provider::Claude` registered under
        /// "anthropic", backed by `fake` instead of the real HTTP source —
        /// the test seam. The backend map starts empty; tests add entries via
        /// `set_backends` when they need to exercise the override-wins path.
        fn registry_with_fake_claude(fake: FakeModelCapabilitySource) -> LlmRegistry {
            let mut registry = LlmRegistry::new();
            let client = claude::Client::new("fake-key").with_capability_source(Arc::new(fake));
            registry.register("anthropic", Arc::new(Provider::Claude(client)));
            registry
        }

        #[tokio::test]
        async fn config_override_wins_no_live_call_attempted() {
            // The fake would answer with a DIFFERENT window than config —
            // if the live path were consulted at all, this test would catch
            // it via the mismatched value, and `call_count` pins it down
            // directly: config must win WITHOUT ever touching the seam.
            let fake = FakeModelCapabilitySource::always_none()
                .with_response("claude-opus-4-8", Ok(Some(999)));
            let mut registry = registry_with_fake_claude(fake);
            let mut cfg = BackendConfig::new("anthropic", BackendKind::Anthropic);
            cfg.models.insert(
                "claude-opus-4-8".to_string(),
                config::ModelInfo {
                    context_window: Some(1_000_000),
                    extra: None,
                },
            );
            registry.set_backends(vec![cfg]);

            let window = registry.context_window_for_live("anthropic", "claude-opus-4-8").await;
            assert_eq!(
                window,
                Some(1_000_000),
                "config value must win over the live fake's Some(999)"
            );

            // A second call also stays on the config value — proving config
            // short-circuits every time, not just once before a cache warms.
            let window_again = registry.context_window_for_live("anthropic", "claude-opus-4-8").await;
            assert_eq!(window_again, Some(1_000_000));
        }

        #[tokio::test]
        async fn live_value_used_when_config_absent_and_cached_on_second_call() {
            let fake = FakeModelCapabilitySource::always_none()
                .with_response("claude-sonnet-4-20250514", Ok(Some(200_000)));
            let registry = registry_with_fake_claude(fake);
            // No provider_configs at all — the honest gap this closes.

            let window = registry
                .context_window_for_live("anthropic", "claude-sonnet-4-20250514")
                .await;
            assert_eq!(
                window,
                Some(200_000),
                "live lookup must supply the window when config has no entry"
            );

            // Second call must hit the client-side cache, not the fake again.
            // We can't reach into the trait object's call_count from here
            // (it's type-erased behind Arc<dyn ModelCapabilitySource>), so
            // this is covered directly at the `claude::Client` level in
            // `claude/mod.rs`'s own tests; here we just confirm the value is
            // stable across repeated calls.
            let window_again = registry
                .context_window_for_live("anthropic", "claude-sonnet-4-20250514")
                .await;
            assert_eq!(window_again, Some(200_000));
        }

        #[tokio::test]
        async fn unknown_model_stays_none_when_live_says_none() {
            // Fake's default is Ok(None) for anything unscripted — mirrors a
            // real 404 from the live API for a model id that doesn't exist.
            let fake = FakeModelCapabilitySource::always_none();
            let registry = registry_with_fake_claude(fake);

            let window = registry
                .context_window_for_live("anthropic", "definitely-not-a-real-model")
                .await;
            assert_eq!(window, None, "unknown model must resolve to None, never a guess");
        }

        #[tokio::test]
        async fn live_lookup_failure_degrades_to_none_never_panics() {
            let fake = FakeModelCapabilitySource::always_none().with_response(
                "claude-opus-4-8",
                Err(LlmError::NetworkError("connection refused".into())),
            );
            let registry = registry_with_fake_claude(fake);

            let window = registry.context_window_for_live("anthropic", "claude-opus-4-8").await;
            assert_eq!(
                window, None,
                "a network/auth failure must degrade to None, never fabricate a window"
            );
        }

        #[tokio::test]
        async fn non_claude_provider_falls_through_to_none_additive_not_replacement() {
            // Non-Anthropic providers keep the config-only path — the live
            // fallback is Anthropic-specific and must not silently apply to
            // (say) DeepSeek just because a Claude client happens to also be
            // registered under a different name.
            let mut registry = LlmRegistry::new();
            registry.register(
                "deepseek",
                Arc::new(Provider::DeepSeek(deepseek::Client::new("fake"))),
            );
            let window = registry
                .context_window_for_live("deepseek", "deepseek-v4-pro")
                .await;
            assert_eq!(
                window, None,
                "non-Claude provider with no config entry must stay None, not attempt a live lookup"
            );
        }

        #[tokio::test]
        async fn both_null_together_invariant_holds_through_live_path() {
            // Mirrors the kernel_db `context_used_pct` both-null-together
            // guard, but exercised through the live-fallback resolution
            // itself: when the live lookup resolves None, the percentage
            // computed from it must also be None, never a fabricated 0%/NaN.
            let fake = FakeModelCapabilitySource::always_none();
            let registry = registry_with_fake_claude(fake);
            let window = registry.context_window_for_live("anthropic", "unknown-model").await;
            assert_eq!(window, None);

            let usage = crate::kernel_db::ContextUsageRow {
                context_id: kaijutsu_types::ContextId::new(),
                provider: "anthropic".into(),
                model: "unknown-model".into(),
                input_tokens: 1000,
                output_tokens: 200,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
                updated_at: 1,
            };
            let pct = crate::kernel_db::context_used_pct(&usage, window);
            assert_eq!(
                pct, None,
                "window=None must yield pct=None — both null together, never a guessed pct"
            );
        }
    }

    // ── Hydration tests ───────────────────────────────────────────────

    mod hydration {
        use super::super::*;
        use kaijutsu_types::{
            BlockId, BlockSnapshot, ContextId, PrincipalId, Role as BlockRole, ToolKind,
        };

        fn ctx() -> ContextId {
            ContextId::new()
        }
        fn user() -> PrincipalId {
            PrincipalId::new()
        }
        fn model() -> PrincipalId {
            PrincipalId::new()
        }
        fn system() -> PrincipalId {
            PrincipalId::system()
        }

        #[test]
        fn empty_blocks_produce_empty_messages() {
            assert!(hydrate_from_blocks(&[]).is_empty());
        }

        #[test]
        fn simple_user_model_exchange() {
            let c = ctx();
            let u = user();
            let m = model();
            let blocks = vec![
                BlockSnapshot::text(BlockId::new(c, u, 0), None, BlockRole::User, "Hello"),
                BlockSnapshot::text(BlockId::new(c, m, 0), None, BlockRole::Model, "Hi there"),
            ];
            let msgs = hydrate_from_blocks(&blocks);
            assert_eq!(msgs.len(), 2);
            assert_eq!(msgs[0].role, Role::User);
            assert_eq!(msgs[0].as_text(), Some("Hello"));
            assert_eq!(msgs[1].role, Role::Assistant);
            assert_eq!(msgs[1].as_text(), Some("Hi there"));
        }

        #[test]
        fn tool_roundtrip_with_tool_use_id() {
            let c = ctx();
            let u = user();
            let m = model();
            let s = system();

            let user_block = BlockSnapshot::text(
                BlockId::new(c, u, 0),
                None,
                BlockRole::User,
                "Read /etc/hosts",
            );
            let call_id = BlockId::new(c, m, 0);
            let tool_call = BlockSnapshot::tool_call(
                call_id,
                None,
                ToolKind::Mcp,
                "read_file",
                serde_json::json!({"path": "/etc/hosts"}),
                BlockRole::Model,
                Some("toolu_01ABC".to_string()),
            );
            let tool_result = BlockSnapshot::tool_result(
                BlockId::new(c, s, 0),
                call_id,
                ToolKind::Mcp,
                "127.0.0.1 localhost",
                false,
                Some(0),
                Some("toolu_01ABC".to_string()),
            );

            let msgs = hydrate_from_blocks(&[user_block, tool_call, tool_result]);
            assert_eq!(msgs.len(), 3);

            // User message
            assert_eq!(msgs[0].as_text(), Some("Read /etc/hosts"));

            // Assistant with tool use
            assert_eq!(msgs[1].role, Role::Assistant);
            match &msgs[1].content {
                MessageContent::Blocks(blocks) => {
                    assert_eq!(blocks.len(), 1);
                    match &blocks[0] {
                        ContentBlock::ToolUse { id, name, .. } => {
                            assert_eq!(id, "toolu_01ABC");
                            assert_eq!(name, "read_file");
                        }
                        other => panic!("Expected ToolUse, got {:?}", other),
                    }
                }
                other => panic!("Expected Blocks, got {:?}", other),
            }

            // Tool results
            assert_eq!(msgs[2].role, Role::User);
            match &msgs[2].content {
                MessageContent::Blocks(blocks) => {
                    assert_eq!(blocks.len(), 1);
                    match &blocks[0] {
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            assert_eq!(tool_use_id, "toolu_01ABC");
                            assert_eq!(content, "127.0.0.1 localhost");
                            assert!(!is_error);
                        }
                        other => panic!("Expected ToolResult, got {:?}", other),
                    }
                }
                other => panic!("Expected Blocks, got {:?}", other),
            }
        }

        #[test]
        fn multiple_tool_calls_grouped() {
            let c = ctx();
            let u = user();
            let m = model();
            let s = system();

            let blocks = vec![
                BlockSnapshot::text(BlockId::new(c, u, 0), None, BlockRole::User, "Build it"),
                BlockSnapshot::tool_call(
                    BlockId::new(c, m, 0),
                    None,
                    ToolKind::Shell,
                    "shell",
                    serde_json::json!({"code": "cargo build"}),
                    BlockRole::Model,
                    Some("toolu_1".into()),
                ),
                BlockSnapshot::tool_call(
                    BlockId::new(c, m, 1),
                    None,
                    ToolKind::Shell,
                    "shell",
                    serde_json::json!({"code": "cargo test"}),
                    BlockRole::Model,
                    Some("toolu_2".into()),
                ),
                BlockSnapshot::tool_result(
                    BlockId::new(c, s, 0),
                    BlockId::new(c, m, 0),
                    ToolKind::Shell,
                    "ok",
                    false,
                    Some(0),
                    Some("toolu_1".into()),
                ),
                BlockSnapshot::tool_result(
                    BlockId::new(c, s, 1),
                    BlockId::new(c, m, 1),
                    ToolKind::Shell,
                    "ok",
                    false,
                    Some(0),
                    Some("toolu_2".into()),
                ),
            ];

            let msgs = hydrate_from_blocks(&blocks);
            assert_eq!(msgs.len(), 3); // user, assistant(2 tool_uses), user(2 tool_results)

            // Assistant should have 2 tool uses
            match &msgs[1].content {
                MessageContent::Blocks(blocks) => assert_eq!(blocks.len(), 2),
                _ => panic!("Expected blocks"),
            }

            // Tool results should have 2 results
            match &msgs[2].content {
                MessageContent::Blocks(blocks) => assert_eq!(blocks.len(), 2),
                _ => panic!("Expected blocks"),
            }
        }

        #[test]
        fn skips_thinking_file_empty_but_includes_drift() {
            let c = ctx();
            let u = user();
            let m = model();

            let blocks = vec![
                BlockSnapshot::text(BlockId::new(c, u, 0), None, BlockRole::User, "Hello"),
                BlockSnapshot::thinking(BlockId::new(c, m, 0), None, "Let me think..."),
                BlockSnapshot::text(BlockId::new(c, m, 1), None, BlockRole::Model, "Hi"),
                BlockSnapshot::drift(
                    BlockId::new(c, PrincipalId::system(), 0),
                    None,
                    "drift content",
                    ContextId::new(),
                    None,
                    kaijutsu_types::DriftKind::Push,
                ),
                BlockSnapshot::file(BlockId::new(c, u, 1), None, "/foo", "content"),
                BlockSnapshot::text(BlockId::new(c, m, 3), None, BlockRole::Model, ""),
            ];

            let msgs = hydrate_from_blocks(&blocks);
            // user + assistant + drift (as user) = 3; thinking/file/empty skipped
            assert_eq!(msgs.len(), 3);
            assert_eq!(msgs[0].as_text(), Some("Hello"));
            assert_eq!(msgs[1].as_text(), Some("Hi"));
            assert_eq!(msgs[2].role, Role::User); // drift becomes user message
            assert!(msgs[2].as_text().unwrap().contains("drift content"));
        }

        #[test]
        fn skips_excluded_blocks() {
            let c = ctx();
            let u = user();
            let m = model();

            let blocks = vec![
                BlockSnapshot::text(BlockId::new(c, u, 0), None, BlockRole::User, "Hello"),
                {
                    let mut b = BlockSnapshot::text(
                        BlockId::new(c, m, 0),
                        None,
                        BlockRole::Model,
                        "excluded reply",
                    );
                    b.excluded = true;
                    b
                },
                BlockSnapshot::text(BlockId::new(c, m, 1), None, BlockRole::Model, "kept reply"),
            ];

            let msgs = hydrate_from_blocks(&blocks);
            assert_eq!(msgs.len(), 2);
            assert_eq!(msgs[0].as_text(), Some("Hello"));
            assert_eq!(msgs[1].as_text(), Some("kept reply"));
        }

        #[test]
        fn consecutive_model_text_merged() {
            let c = ctx();
            let u = user();
            let m = model();

            let blocks = vec![
                BlockSnapshot::text(BlockId::new(c, u, 0), None, BlockRole::User, "Hello"),
                BlockSnapshot::text(BlockId::new(c, m, 0), None, BlockRole::Model, "Part 1"),
                BlockSnapshot::text(BlockId::new(c, m, 1), None, BlockRole::Model, "Part 2"),
            ];

            let msgs = hydrate_from_blocks(&blocks);
            assert_eq!(msgs.len(), 2);
            assert_eq!(msgs[1].as_text(), Some("Part 1\nPart 2"));
        }

        #[test]
        fn tool_use_id_fallback_to_block_key() {
            let c = ctx();
            let u = user();
            let m = model();
            let s = system();

            let call_id = BlockId::new(c, m, 0);
            let result_id = BlockId::new(c, s, 0);
            let blocks = vec![
                BlockSnapshot::text(BlockId::new(c, u, 0), None, BlockRole::User, "Do it"),
                BlockSnapshot::tool_call(
                    call_id,
                    None,
                    ToolKind::Shell,
                    "shell",
                    serde_json::json!({"code": "ls"}),
                    BlockRole::Model,
                    None, // no tool_use_id
                ),
                BlockSnapshot::tool_result(
                    result_id,
                    call_id,
                    ToolKind::Shell,
                    "files",
                    false,
                    Some(0),
                    None, // no tool_use_id
                ),
            ];

            let msgs = hydrate_from_blocks(&blocks);
            assert_eq!(msgs.len(), 3);

            // Tool use should fall back to block id key
            match &msgs[1].content {
                MessageContent::Blocks(blocks) => match &blocks[0] {
                    ContentBlock::ToolUse { id, .. } => {
                        assert_eq!(id, &call_id.to_key());
                    }
                    _ => panic!("Expected ToolUse"),
                },
                _ => panic!("Expected Blocks"),
            }

            // Tool result should fall back to tool_call_id key
            match &msgs[2].content {
                MessageContent::Blocks(blocks) => match &blocks[0] {
                    ContentBlock::ToolResult { tool_use_id, .. } => {
                        assert_eq!(tool_use_id, &call_id.to_key());
                    }
                    _ => panic!("Expected ToolResult"),
                },
                _ => panic!("Expected Blocks"),
            }
        }

        #[test]
        fn trailing_tool_use_guard() {
            let c = ctx();
            let u = user();
            let m = model();

            let blocks = vec![
                BlockSnapshot::text(BlockId::new(c, u, 0), None, BlockRole::User, "Do it"),
                BlockSnapshot::tool_call(
                    BlockId::new(c, m, 0),
                    None,
                    ToolKind::Shell,
                    "shell",
                    serde_json::json!({"code": "ls"}),
                    BlockRole::Model,
                    Some("toolu_orphan".into()),
                ),
                // No tool result follows!
            ];

            let msgs = hydrate_from_blocks(&blocks);
            assert_eq!(msgs.len(), 3); // user, assistant(tool_use), user(synthetic error)

            // Last message should be synthesized error results
            match &msgs[2].content {
                MessageContent::Blocks(blocks) => {
                    assert_eq!(blocks.len(), 1);
                    match &blocks[0] {
                        ContentBlock::ToolResult {
                            tool_use_id,
                            is_error,
                            content,
                        } => {
                            assert_eq!(tool_use_id, "toolu_orphan");
                            assert!(is_error);
                            assert!(content.contains("interrupted"));
                        }
                        _ => panic!("Expected ToolResult"),
                    }
                }
                _ => panic!("Expected Blocks"),
            }
        }

        #[test]
        fn full_agentic_loop_replay() {
            // Simulate: user → model text + tool_call → tool_result → model text + tool_call → tool_result → model text
            let c = ctx();
            let u = user();
            let m = model();
            let s = system();

            let blocks = vec![
                // Turn 1: user prompt
                BlockSnapshot::text(BlockId::new(c, u, 0), None, BlockRole::User, "Fix the bug"),
                // Turn 2: model thinks + calls tool
                BlockSnapshot::text(
                    BlockId::new(c, m, 0),
                    None,
                    BlockRole::Model,
                    "Let me check",
                ),
                BlockSnapshot::tool_call(
                    BlockId::new(c, m, 1),
                    None,
                    ToolKind::Mcp,
                    "read_file",
                    serde_json::json!({"path": "src/main.rs"}),
                    BlockRole::Model,
                    Some("toolu_read".into()),
                ),
                // Turn 3: tool result
                BlockSnapshot::tool_result(
                    BlockId::new(c, s, 0),
                    BlockId::new(c, m, 1),
                    ToolKind::Mcp,
                    "fn main() { panic!() }",
                    false,
                    Some(0),
                    Some("toolu_read".into()),
                ),
                // Turn 4: model edits
                BlockSnapshot::text(
                    BlockId::new(c, m, 2),
                    None,
                    BlockRole::Model,
                    "Found it, fixing",
                ),
                BlockSnapshot::tool_call(
                    BlockId::new(c, m, 3),
                    None,
                    ToolKind::Mcp,
                    "write_file",
                    serde_json::json!({"path": "src/main.rs", "content": "fn main() {}"}),
                    BlockRole::Model,
                    Some("toolu_write".into()),
                ),
                // Turn 5: tool result
                BlockSnapshot::tool_result(
                    BlockId::new(c, s, 1),
                    BlockId::new(c, m, 3),
                    ToolKind::Mcp,
                    "ok",
                    false,
                    Some(0),
                    Some("toolu_write".into()),
                ),
                // Turn 6: model done
                BlockSnapshot::text(BlockId::new(c, m, 4), None, BlockRole::Model, "Fixed!"),
            ];

            let msgs = hydrate_from_blocks(&blocks);

            // Expected: user, assistant(text+tool), user(result), assistant(text+tool), user(result), assistant
            assert_eq!(msgs.len(), 6);
            assert_eq!(msgs[0].role, Role::User);
            assert_eq!(msgs[0].as_text(), Some("Fix the bug"));

            assert_eq!(msgs[1].role, Role::Assistant);
            match &msgs[1].content {
                MessageContent::Blocks(blocks) => {
                    assert_eq!(blocks.len(), 2); // text + tool_use
                }
                _ => panic!("Expected blocks"),
            }

            assert_eq!(msgs[2].role, Role::User); // tool results

            assert_eq!(msgs[3].role, Role::Assistant);
            match &msgs[3].content {
                MessageContent::Blocks(blocks) => {
                    assert_eq!(blocks.len(), 2); // text + tool_use
                }
                _ => panic!("Expected blocks"),
            }

            assert_eq!(msgs[4].role, Role::User); // tool results
            assert_eq!(msgs[5].role, Role::Assistant);
            assert_eq!(msgs[5].as_text(), Some("Fixed!"));
        }

        #[test]
        fn drift_blocks_become_user_messages() {
            let c = ctx();
            let u = user();
            let m = model();
            let source_ctx = ctx(); // different context

            let blocks = vec![
                BlockSnapshot::text(
                    BlockId::new(c, u, 0),
                    None,
                    BlockRole::User,
                    "What's happening?",
                ),
                BlockSnapshot::text(
                    BlockId::new(c, m, 0),
                    None,
                    BlockRole::Model,
                    "Let me check.",
                ),
                BlockSnapshot::drift(
                    BlockId::new(c, PrincipalId::system(), 0),
                    None,
                    "Found a critical bug in auth module. JWT tokens expire early.",
                    source_ctx,
                    Some("claude-opus-4-6".to_string()),
                    kaijutsu_types::DriftKind::Pull,
                ),
                BlockSnapshot::text(
                    BlockId::new(c, m, 1),
                    None,
                    BlockRole::Model,
                    "Got it, investigating.",
                ),
            ];

            let msgs = hydrate_from_blocks(&blocks);
            assert_eq!(
                msgs.len(),
                4,
                "expected 4 messages, got {}: {:?}",
                msgs.len(),
                msgs.iter().map(|m| &m.role).collect::<Vec<_>>()
            );
            assert_eq!(msgs[0].role, Role::User);
            assert_eq!(msgs[1].role, Role::Assistant);
            // Drift block becomes a User message
            assert_eq!(msgs[2].role, Role::User);
            let drift_text = msgs[2].as_text().unwrap();
            assert!(
                drift_text.contains("pull"),
                "should contain drift kind: {drift_text}"
            );
            assert!(
                drift_text.contains(&source_ctx.short()),
                "should contain source ctx short: {drift_text}"
            );
            assert!(
                drift_text.contains("JWT tokens"),
                "should contain content: {drift_text}"
            );
            assert_eq!(msgs[3].role, Role::Assistant);
        }

        #[test]
        fn drift_blocks_with_unknown_source() {
            let c = ctx();
            // Drift block with no source_context (edge case)
            let mut drift = BlockSnapshot::drift(
                BlockId::new(c, PrincipalId::system(), 0),
                None,
                "some drifted content",
                ctx(), // will be overridden
                None,
                kaijutsu_types::DriftKind::Distill,
            );
            drift.source_context = None; // force no source

            let msgs = hydrate_from_blocks(&[drift]);
            assert_eq!(msgs.len(), 1);
            let text = msgs[0].as_text().unwrap();
            assert!(
                text.contains("unknown"),
                "should say 'unknown' for no source: {text}"
            );
            assert!(
                text.contains("distill"),
                "should contain drift kind: {text}"
            );
        }

        #[test]
        fn user_shell_command_with_output() {
            let c = ctx();
            let u = user();
            let s = system();

            let call_id = BlockId::new(c, u, 0);
            let tool_call = BlockSnapshot::tool_call(
                call_id,
                None,
                ToolKind::Shell,
                "shell",
                serde_json::json!({"code": "cargo check"}),
                BlockRole::User,
                None,
            );
            let tool_result = BlockSnapshot::tool_result(
                BlockId::new(c, s, 0),
                call_id,
                ToolKind::Shell,
                "Compiling kaijutsu v0.1.0\n    Finished",
                false,
                Some(0),
                None,
            );

            let msgs = hydrate_from_blocks(&[tool_call, tool_result]);
            assert_eq!(msgs.len(), 1);
            assert_eq!(msgs[0].role, Role::User);
            let text = msgs[0].as_text().unwrap();
            assert!(text.contains("[User ran `cargo check`]"), "got: {text}");
            assert!(text.contains("Compiling kaijutsu"), "got: {text}");
        }

        #[test]
        fn user_shell_command_empty_output() {
            let c = ctx();
            let u = user();
            let s = system();

            let call_id = BlockId::new(c, u, 0);
            let tool_call = BlockSnapshot::tool_call(
                call_id,
                None,
                ToolKind::Shell,
                "shell",
                serde_json::json!({"code": "true"}),
                BlockRole::User,
                None,
            );
            let tool_result = BlockSnapshot::tool_result(
                BlockId::new(c, s, 0),
                call_id,
                ToolKind::Shell,
                "",
                false,
                Some(0),
                None,
            );

            let msgs = hydrate_from_blocks(&[tool_call, tool_result]);
            assert_eq!(msgs.len(), 1);
            assert_eq!(msgs[0].as_text(), Some("[User ran `true`]"));
        }

        #[test]
        fn user_shell_interleaved_with_model_tool_call() {
            let c = ctx();
            let u = user();
            let m = model();
            let s = system();

            // User runs a shell command
            let user_call_id = BlockId::new(c, u, 0);
            let user_tc = BlockSnapshot::tool_call(
                user_call_id,
                None,
                ToolKind::Shell,
                "shell",
                serde_json::json!({"code": "ls"}),
                BlockRole::User,
                None,
            );
            let user_tr = BlockSnapshot::tool_result(
                BlockId::new(c, s, 0),
                user_call_id,
                ToolKind::Shell,
                "src\nCargo.toml",
                false,
                Some(0),
                None,
            );

            // Model text + tool call
            let model_text = BlockSnapshot::text(
                BlockId::new(c, m, 0),
                None,
                BlockRole::Model,
                "Let me check...",
            );
            let model_call_id = BlockId::new(c, m, 1);
            let model_tc = BlockSnapshot::tool_call(
                model_call_id,
                None,
                ToolKind::Mcp,
                "read_file",
                serde_json::json!({"path": "Cargo.toml"}),
                BlockRole::Model,
                Some("toolu_01XYZ".to_string()),
            );
            let model_tr = BlockSnapshot::tool_result(
                BlockId::new(c, s, 1),
                model_call_id,
                ToolKind::Mcp,
                "[package]\nname = \"kaijutsu\"",
                false,
                Some(0),
                Some("toolu_01XYZ".to_string()),
            );

            let msgs = hydrate_from_blocks(&[user_tc, user_tr, model_text, model_tc, model_tr]);
            assert_eq!(
                msgs.len(),
                3,
                "got: {:?}",
                msgs.iter().map(|m| &m.role).collect::<Vec<_>>()
            );
            // 1: user shell message
            assert_eq!(msgs[0].role, Role::User);
            assert!(msgs[0].as_text().unwrap().contains("[User ran `ls`]"));
            // 2: assistant with text + tool use (merged)
            assert_eq!(msgs[1].role, Role::Assistant);
            // 3: tool result (user role per API convention)
            assert_eq!(msgs[2].role, Role::User);
        }

        #[test]
        fn user_shell_interleaved_out_of_order() {
            // Gemini review catch: if blocks arrive out of order (model result
            // before user result), the HashMap keying prevents mispairing.
            let c = ctx();
            let u = user();
            let m = model();
            let s = system();

            let user_call_id = BlockId::new(c, u, 0);
            let user_tc = BlockSnapshot::tool_call(
                user_call_id,
                None,
                ToolKind::Shell,
                "shell",
                serde_json::json!({"code": "sleep 10"}),
                BlockRole::User,
                None,
            );

            let model_call_id = BlockId::new(c, m, 1);
            let model_tc = BlockSnapshot::tool_call(
                model_call_id,
                None,
                ToolKind::Mcp,
                "fast_tool",
                serde_json::json!({}),
                BlockRole::Model,
                Some("toolu_fast".to_string()),
            );

            let model_tr = BlockSnapshot::tool_result(
                BlockId::new(c, s, 1),
                model_call_id,
                ToolKind::Mcp,
                "fast result",
                false,
                Some(0),
                Some("toolu_fast".to_string()),
            );

            let user_tr = BlockSnapshot::tool_result(
                BlockId::new(c, s, 0),
                user_call_id,
                ToolKind::Shell,
                "done sleeping",
                false,
                Some(0),
                None,
            );

            // Order: User Call, Model Call, Model Result, User Result
            let msgs = hydrate_from_blocks(&[user_tc, model_tc, model_tr, user_tr]);

            assert_eq!(
                msgs.len(),
                3,
                "got: {:?}",
                msgs.iter().map(|m| &m.role).collect::<Vec<_>>()
            );

            // 1: Assistant with tool use (model call)
            assert_eq!(msgs[0].role, Role::Assistant);

            // 2: Tool result for model's fast_tool
            assert_eq!(msgs[1].role, Role::User);
            match &msgs[1].content {
                MessageContent::Blocks(blocks) => match &blocks[0] {
                    ContentBlock::ToolResult { tool_use_id, .. } => {
                        assert_eq!(tool_use_id, "toolu_fast");
                    }
                    _ => panic!("Expected ToolResult"),
                },
                _ => panic!("Expected Blocks"),
            }

            // 3: User shell result
            assert_eq!(msgs[2].role, Role::User);
            assert!(msgs[2].as_text().unwrap().contains("[User ran `sleep 10`]"));
            assert!(msgs[2].as_text().unwrap().contains("done sleeping"));
        }

        #[test]
        fn mid_conversation_orphaned_tool_use_gets_synthetic_result() {
            // Simulates a forked context: model requested a tool, no result came,
            // then the user typed more messages. The API requires every tool_use
            // to have a matching tool_result in the immediately following user
            // message — not just at the tail.
            let c = ctx();
            let u = user();
            let m = model();

            let blocks = vec![
                BlockSnapshot::text(BlockId::new(c, u, 0), None, BlockRole::User, "Do it"),
                BlockSnapshot::tool_call(
                    BlockId::new(c, m, 0),
                    None,
                    ToolKind::Shell,
                    "shell",
                    serde_json::json!({"code": "cargo build"}),
                    BlockRole::Model,
                    Some("toolu_orphan_mid".into()),
                ),
                // No tool result! Then user typed again in the forked context:
                BlockSnapshot::text(
                    BlockId::new(c, u, 1),
                    None,
                    BlockRole::User,
                    "how about now?",
                ),
                BlockSnapshot::text(
                    BlockId::new(c, m, 1),
                    None,
                    BlockRole::Model,
                    "Let me try again",
                ),
            ];

            let msgs = hydrate_from_blocks(&blocks);

            // Should be: user, assistant(tool_use), user(synthetic error result), user, assistant
            assert_eq!(
                msgs.len(),
                5,
                "expected 5 messages, got {}: {:?}",
                msgs.len(),
                msgs.iter()
                    .map(|m| format!("{:?}", m.role))
                    .collect::<Vec<_>>()
            );

            assert_eq!(msgs[0].role, Role::User);
            assert_eq!(msgs[0].as_text(), Some("Do it"));

            assert_eq!(msgs[1].role, Role::Assistant);
            match &msgs[1].content {
                MessageContent::Blocks(blocks) => {
                    assert!(blocks.iter().any(|b| matches!(b, ContentBlock::ToolUse { id, .. } if id == "toolu_orphan_mid")));
                }
                _ => panic!("Expected Blocks with ToolUse"),
            }

            // Synthetic error result inserted
            assert_eq!(msgs[2].role, Role::User);
            match &msgs[2].content {
                MessageContent::Blocks(blocks) => {
                    assert_eq!(blocks.len(), 1);
                    match &blocks[0] {
                        ContentBlock::ToolResult {
                            tool_use_id,
                            is_error,
                            content,
                        } => {
                            assert_eq!(tool_use_id, "toolu_orphan_mid");
                            assert!(is_error);
                            assert!(content.contains("interrupted"));
                        }
                        _ => panic!("Expected ToolResult"),
                    }
                }
                _ => panic!("Expected Blocks with synthetic ToolResult"),
            }

            assert_eq!(msgs[3].as_text(), Some("how about now?"));
            assert_eq!(msgs[4].as_text(), Some("Let me try again"));
        }

        #[test]
        fn mid_conversation_partial_tool_results_get_filled() {
            // Two tool_uses, only one result — the missing one gets synthesized.
            let c = ctx();
            let u = user();
            let m = model();
            let s = system();

            let blocks = vec![
                BlockSnapshot::text(
                    BlockId::new(c, u, 0),
                    None,
                    BlockRole::User,
                    "Build and test",
                ),
                BlockSnapshot::tool_call(
                    BlockId::new(c, m, 0),
                    None,
                    ToolKind::Shell,
                    "shell",
                    serde_json::json!({"code": "cargo build"}),
                    BlockRole::Model,
                    Some("toolu_build".into()),
                ),
                BlockSnapshot::tool_call(
                    BlockId::new(c, m, 1),
                    None,
                    ToolKind::Shell,
                    "shell",
                    serde_json::json!({"code": "cargo test"}),
                    BlockRole::Model,
                    Some("toolu_test".into()),
                ),
                // Only the first tool result arrived before fork/interrupt
                BlockSnapshot::tool_result(
                    BlockId::new(c, s, 0),
                    BlockId::new(c, m, 0),
                    ToolKind::Shell,
                    "ok",
                    false,
                    Some(0),
                    Some("toolu_build".into()),
                ),
                // User typed again
                BlockSnapshot::text(
                    BlockId::new(c, u, 1),
                    None,
                    BlockRole::User,
                    "what happened?",
                ),
                BlockSnapshot::text(
                    BlockId::new(c, m, 2),
                    None,
                    BlockRole::Model,
                    "Sorry about that",
                ),
            ];

            let msgs = hydrate_from_blocks(&blocks);

            // Find the tool_results message and verify both IDs are covered
            let tool_result_msg = &msgs[2];
            assert_eq!(tool_result_msg.role, Role::User);
            match &tool_result_msg.content {
                MessageContent::Blocks(blocks) => {
                    let result_ids: Vec<&str> = blocks
                        .iter()
                        .filter_map(|b| {
                            if let ContentBlock::ToolResult { tool_use_id, .. } = b {
                                Some(tool_use_id.as_str())
                            } else {
                                None
                            }
                        })
                        .collect();
                    assert!(
                        result_ids.contains(&"toolu_build"),
                        "missing toolu_build: {:?}",
                        result_ids
                    );
                    assert!(
                        result_ids.contains(&"toolu_test"),
                        "missing toolu_test: {:?}",
                        result_ids
                    );
                }
                _ => panic!("Expected Blocks with ToolResults"),
            }
        }

        #[test]
        fn late_arriving_tool_results_dropped() {
            // Reproduces the real bug: parallel tool calls where some results
            // arrive much later (after the model has moved on). The late results
            // become orphaned User messages with tool_results that don't match
            // the preceding assistant's tool_uses. The API rejects these.
            let c = ctx();
            let u = user();
            let m = model();
            let s = system();

            let blocks = vec![
                // Turn 1: user prompt
                BlockSnapshot::text(BlockId::new(c, u, 0), None, BlockRole::User, "Find configs"),
                // Turn 2: model requests two tools in parallel
                BlockSnapshot::text(BlockId::new(c, m, 0), None, BlockRole::Model, "Checking"),
                BlockSnapshot::tool_call(
                    BlockId::new(c, m, 1),
                    None,
                    ToolKind::Mcp,
                    "read",
                    serde_json::json!({"path": "/etc/config"}),
                    BlockRole::Model,
                    Some("toolu_read".into()),
                ),
                BlockSnapshot::tool_call(
                    BlockId::new(c, m, 2),
                    None,
                    ToolKind::Shell,
                    "shell",
                    serde_json::json!({"code": "ls"}),
                    BlockRole::Model,
                    Some("toolu_shell".into()),
                ),
                // Only the shell result arrives promptly
                BlockSnapshot::tool_result(
                    BlockId::new(c, s, 0),
                    BlockId::new(c, m, 2),
                    ToolKind::Shell,
                    "file1 file2",
                    false,
                    Some(0),
                    Some("toolu_shell".into()),
                ),
                // Model continues (toolu_read never got a result)
                BlockSnapshot::text(
                    BlockId::new(c, m, 3),
                    None,
                    BlockRole::Model,
                    "Based on the ls output",
                ),
                // More conversation...
                BlockSnapshot::text(BlockId::new(c, u, 1), None, BlockRole::User, "thanks"),
                BlockSnapshot::text(
                    BlockId::new(c, m, 4),
                    None,
                    BlockRole::Model,
                    "You're welcome",
                ),
                // NOW the late read result arrives (way out of order)
                BlockSnapshot::tool_result(
                    BlockId::new(c, s, 1),
                    BlockId::new(c, m, 1),
                    ToolKind::Mcp,
                    "config contents here",
                    false,
                    Some(0),
                    Some("toolu_read".into()),
                ),
                // User types again after the late result
                BlockSnapshot::text(
                    BlockId::new(c, u, 2),
                    None,
                    BlockRole::User,
                    "one more thing",
                ),
            ];

            let msgs = hydrate_from_blocks(&blocks);

            // The late tool_result for toolu_read should be dropped.
            // Expected messages:
            //   [0] User "Find configs"
            //   [1] Assistant [Text, ToolUse(toolu_read), ToolUse(toolu_shell)]
            //   [2] User [ToolResult(toolu_shell), ToolResult(toolu_read, err=true)]
            //   [3] Assistant "Based on the ls output"
            //   [4] User "thanks"
            //   [5] Assistant "You're welcome"
            //   [6] User "one more thing"   ← late result dropped, not msg[6]=Blocks[ToolResult]

            // Verify no tool_result-only user messages exist after msg[2]
            for (i, msg) in msgs.iter().enumerate() {
                if i <= 2 {
                    continue;
                }
                if let MessageContent::Blocks(blocks) = &msg.content {
                    let has_tool_result = blocks
                        .iter()
                        .any(|b| matches!(b, ContentBlock::ToolResult { .. }));
                    assert!(
                        !has_tool_result,
                        "msg[{}] has unexpected tool_result (late arrival should be dropped): {:?}",
                        i, blocks
                    );
                }
            }

            // Verify the synthetic error result is present for toolu_read
            match &msgs[2].content {
                MessageContent::Blocks(blocks) => {
                    let result_ids: Vec<(&str, bool)> = blocks
                        .iter()
                        .filter_map(|b| {
                            if let ContentBlock::ToolResult {
                                tool_use_id,
                                is_error,
                                ..
                            } = b
                            {
                                Some((tool_use_id.as_str(), *is_error))
                            } else {
                                None
                            }
                        })
                        .collect();
                    assert!(
                        result_ids.iter().any(|(id, _)| *id == "toolu_shell"),
                        "missing toolu_shell result: {:?}",
                        result_ids
                    );
                    assert!(
                        result_ids
                            .iter()
                            .any(|(id, err)| *id == "toolu_read" && *err),
                        "missing synthetic error for toolu_read: {:?}",
                        result_ids
                    );
                }
                _ => panic!("Expected Blocks at msg[2]"),
            }

            // Verify the late result was dropped (check message count)
            assert_eq!(msgs.last().unwrap().as_text(), Some("one more thing"));
        }

        #[test]
        fn existing_behavior_unchanged_with_drift_addition() {
            // Verify that Text/Thinking/ToolCall/ToolResult still work correctly
            let c = ctx();
            let u = user();
            let m = model();

            let blocks = vec![
                BlockSnapshot::text(BlockId::new(c, u, 0), None, BlockRole::User, "Hello"),
                BlockSnapshot::thinking(BlockId::new(c, m, 0), None, "thinking..."),
                BlockSnapshot::text(BlockId::new(c, m, 1), None, BlockRole::Model, "Hi!"),
            ];

            let msgs = hydrate_from_blocks(&blocks);
            // Signatureless thinking blocks are still skipped (not rehydratable).
            assert_eq!(msgs.len(), 2);
            assert_eq!(msgs[0].as_text(), Some("Hello"));
            assert_eq!(msgs[1].as_text(), Some("Hi!"));
        }

        /// A Thinking block carrying a signature is rehydratable: it becomes a
        /// `Reasoning` content block at the head of the assistant message,
        /// before the text, with its signature preserved verbatim (Anthropic
        /// rejects a tampered/absent signature on a tool-use cycle).
        #[test]
        fn signed_thinking_rehydrates_as_reasoning_before_text() {
            let c = ctx();
            let u = user();
            let m = model();

            let mut thinking =
                BlockSnapshot::thinking(BlockId::new(c, m, 0), None, "let me reason");
            thinking.signature = Some("sig_abc123".into());

            let blocks = vec![
                BlockSnapshot::text(BlockId::new(c, u, 0), None, BlockRole::User, "Hello"),
                thinking,
                BlockSnapshot::text(BlockId::new(c, m, 1), None, BlockRole::Model, "Hi!"),
            ];

            let msgs = hydrate_from_blocks(&blocks);
            assert_eq!(msgs.len(), 2, "user + one assistant turn");
            assert_eq!(msgs[1].role, Role::Assistant);
            match &msgs[1].content {
                MessageContent::Blocks(blocks) => {
                    assert!(
                        matches!(
                            &blocks[0],
                            ContentBlock::Reasoning { text, signature }
                                if text == "let me reason"
                                    && signature.as_deref() == Some("sig_abc123")
                        ),
                        "reasoning leads the assistant message: {:?}",
                        blocks[0]
                    );
                    assert!(
                        matches!(&blocks[1], ContentBlock::Text { text } if text == "Hi!"),
                        "text follows reasoning: {:?}",
                        blocks[1]
                    );
                }
                other => panic!("expected Blocks, got {other:?}"),
            }
        }

        /// Two signed thinking blocks in one turn are preserved as two separate
        /// `Reasoning` blocks, in order, each keeping its own signature — they
        /// are NOT merged (Anthropic verifies each signature against its own
        /// block's text, so concatenating would invalidate the verifier).
        #[test]
        fn consecutive_signed_thinking_kept_as_separate_reasoning() {
            let c = ctx();
            let m = model();

            let mut t1 = BlockSnapshot::thinking(BlockId::new(c, m, 0), None, "first");
            t1.signature = Some("sig1".into());
            let mut t2 = BlockSnapshot::thinking(BlockId::new(c, m, 1), None, "second");
            t2.signature = Some("sig2".into());

            let blocks = vec![
                t1,
                t2,
                BlockSnapshot::text(BlockId::new(c, m, 2), None, BlockRole::Model, "answer"),
            ];

            let msgs = hydrate_from_blocks(&blocks);
            assert_eq!(msgs.len(), 1);
            match &msgs[0].content {
                MessageContent::Blocks(blocks) => {
                    assert!(
                        matches!(
                            &blocks[0],
                            ContentBlock::Reasoning { text, signature }
                                if text == "first" && signature.as_deref() == Some("sig1")
                        ),
                        "first reasoning block intact: {:?}",
                        blocks[0]
                    );
                    assert!(
                        matches!(
                            &blocks[1],
                            ContentBlock::Reasoning { text, signature }
                                if text == "second" && signature.as_deref() == Some("sig2")
                        ),
                        "second reasoning block intact: {:?}",
                        blocks[1]
                    );
                    assert!(
                        matches!(&blocks[2], ContentBlock::Text { text } if text == "answer"),
                        "text follows both reasoning blocks: {:?}",
                        blocks[2]
                    );
                }
                other => panic!("expected Blocks, got {other:?}"),
            }
        }

        // ── Error block hydration ──────────────────────────────────────

        fn test_error_payload() -> kaijutsu_types::ErrorPayload {
            kaijutsu_types::ErrorPayload {
                category: kaijutsu_types::ErrorCategory::Tool,
                severity: kaijutsu_types::ErrorSeverity::Error,
                code: Some("tool.timeout".into()),
                detail: Some("Shell command timed out after 30s".into()),
                span: None,
                source_kind: Some(kaijutsu_types::BlockKind::ToolResult),
            }
        }

        #[test]
        fn test_hydrate_error_block_standalone_becomes_user_message() {
            let c = ctx();
            let u = user();
            let m = model();

            let blocks = vec![
                BlockSnapshot::text(BlockId::new(c, u, 0), None, BlockRole::User, "Hello"),
                BlockSnapshot::text(BlockId::new(c, m, 0), None, BlockRole::Model, "Hi"),
                BlockSnapshot::error_for(
                    BlockId::new(c, PrincipalId::system(), 0),
                    BlockId::new(c, m, 0), // parent is a Text block, not ToolResult
                    test_error_payload(),
                    "tool error: timeout",
                ),
            ];

            let msgs = hydrate_from_blocks(&blocks);
            assert_eq!(msgs.len(), 3);
            assert_eq!(msgs[0].role, Role::User);
            assert_eq!(msgs[1].role, Role::Assistant);
            // Error block becomes a user message with XML envelope
            assert_eq!(msgs[2].role, Role::User);
            let text = msgs[2].as_text().expect("should be text");
            assert!(text.contains("<error"));
            assert!(text.contains("category=\"tool\""));
            assert!(text.contains("Shell command timed out"));
        }

        #[test]
        fn test_hydrate_error_block_folds_into_tool_result() {
            let c = ctx();
            let u = user();
            let m = model();
            let s = system();

            let tool_call_id = BlockId::new(c, m, 1);
            let tool_result_id = BlockId::new(c, s, 0);

            let blocks = vec![
                BlockSnapshot::text(BlockId::new(c, u, 0), None, BlockRole::User, "Run it"),
                BlockSnapshot::tool_call(
                    tool_call_id,
                    None,
                    ToolKind::Shell,
                    "shell",
                    serde_json::json!({"code": "sleep 999"}),
                    BlockRole::Model,
                    Some("toolu_01".into()),
                ),
                BlockSnapshot::tool_result(
                    tool_result_id,
                    tool_call_id,
                    ToolKind::Shell,
                    "Error: timed out",
                    true,
                    Some(124),
                    Some("toolu_01".into()),
                ),
                BlockSnapshot::error_for(
                    BlockId::new(c, s, 1),
                    tool_result_id, // parent is the ToolResult
                    test_error_payload(),
                    "tool error: timeout",
                ),
            ];

            let msgs = hydrate_from_blocks(&blocks);
            // Should be: user, assistant+tool_use, user+tool_result (with error folded in)
            assert_eq!(msgs.len(), 3);

            // The tool result message should contain the error envelope
            let result_msg = &msgs[2];
            assert_eq!(result_msg.role, Role::User);
            if let MessageContent::Blocks(blocks) = &result_msg.content {
                let tool_result = blocks
                    .iter()
                    .find_map(|b| {
                        if let ContentBlock::ToolResult { content, .. } = b {
                            Some(content.as_str())
                        } else {
                            None
                        }
                    })
                    .expect("should have a tool result");
                assert!(
                    tool_result.contains("<error"),
                    "error should be folded into tool result content"
                );
                assert!(tool_result.contains("Error: timed out"));
            } else {
                panic!("expected blocks content");
            }
        }

        #[test]
        fn test_hydrate_error_block_ephemeral_excluded() {
            let c = ctx();
            let u = user();
            let m = model();

            let mut error_block = BlockSnapshot::error_for(
                BlockId::new(c, PrincipalId::system(), 0),
                BlockId::new(c, m, 0),
                test_error_payload(),
                "should not appear",
            );
            error_block.ephemeral = true;

            let blocks = vec![
                BlockSnapshot::text(BlockId::new(c, u, 0), None, BlockRole::User, "Hello"),
                BlockSnapshot::text(BlockId::new(c, m, 0), None, BlockRole::Model, "Hi"),
                error_block,
            ];

            let msgs = hydrate_from_blocks(&blocks);
            assert_eq!(msgs.len(), 2); // ephemeral error excluded
        }

        #[test]
        fn test_hydrate_error_block_detail_truncated() {
            let c = ctx();
            let u = user();
            let m = model();

            let long_detail = "x".repeat(5000);
            let payload = kaijutsu_types::ErrorPayload {
                category: kaijutsu_types::ErrorCategory::Kernel,
                severity: kaijutsu_types::ErrorSeverity::Fatal,
                code: None,
                detail: Some(long_detail),
                span: None,
                source_kind: None,
            };

            let blocks = vec![
                BlockSnapshot::text(BlockId::new(c, u, 0), None, BlockRole::User, "Hello"),
                BlockSnapshot::text(BlockId::new(c, m, 0), None, BlockRole::Model, "Hi"),
                BlockSnapshot::error_for(
                    BlockId::new(c, PrincipalId::system(), 0),
                    BlockId::new(c, m, 0),
                    payload,
                    "kernel error",
                ),
            ];

            let msgs = hydrate_from_blocks(&blocks);
            let error_text = msgs[2].as_text().expect("should be text");
            assert!(error_text.contains("...[truncated]"));
            assert!(error_text.len() < 3000);
        }

        // ── Phase 2: Notification block hydration (D-34) ──────────────────
        //
        // These tests verify the full arm for `BlockKind::Notification`, not
        // just the formatter. `format_notification_for_llm` is unit-tested in
        // `kaijutsu-types` — this layer locks that the hydrator (a) passes
        // Notification blocks through its System-role and empty-content
        // filters, (b) emits them as user messages so the LLM reads them
        // alongside normal conversation, and (c) flushes pending assistant
        // state before the notification so turn boundaries stay coherent.
        //
        // Without these tests, a future refactor of the filter cascade
        // (e.g. adding a new `ephemeral_notifications` flag) could silently
        // drop Notification blocks from LLM context and only the app UI
        // would know.

        fn notif_payload(
            instance: &str,
            kind: kaijutsu_types::NotificationKind,
        ) -> kaijutsu_types::NotificationPayload {
            kaijutsu_types::NotificationPayload {
                instance: instance.into(),
                kind,
                level: None,
                tools: vec!["example_tool".into()],
                count: Some(1),
                detail: None,
            }
        }

        #[test]
        fn notification_block_hydrates_as_user_message_with_xml_envelope() {
            let c = ctx();
            let s = system();
            let block = BlockSnapshot::notification_block(
                BlockId::new(c, s, 0),
                None,
                notif_payload("gpal", kaijutsu_types::NotificationKind::ToolAdded),
                "[gpal] tool added: example_tool",
            );
            let msgs = hydrate_from_blocks(&[block]);
            assert_eq!(
                msgs.len(),
                1,
                "expected one user message for one notification"
            );
            assert_eq!(msgs[0].role, Role::User);
            let text = msgs[0]
                .as_text()
                .expect("notification should hydrate as text");
            // Envelope produced by format_notification_for_llm.
            assert!(
                text.starts_with("<notification "),
                "expected XML envelope, got {text:?}"
            );
            assert!(text.contains("instance=\"gpal\""));
            assert!(text.contains("kind=\"tool_added\""));
            assert!(text.contains("tools=\"example_tool\""));
            assert!(text.ends_with("</notification>"));
        }

        #[test]
        fn notification_block_flushes_pending_assistant_text() {
            // Regression guard: a notification arriving mid-turn must not
            // be folded into a pending assistant reply. `flush_all()` inside
            // the Notification arm is what enforces this.
            let c = ctx();
            let u = user();
            let m = model();
            let s = system();
            let blocks = vec![
                BlockSnapshot::text(BlockId::new(c, u, 0), None, BlockRole::User, "hi"),
                BlockSnapshot::text(BlockId::new(c, m, 0), None, BlockRole::Model, "mid"),
                BlockSnapshot::notification_block(
                    BlockId::new(c, s, 0),
                    None,
                    notif_payload("svc", kaijutsu_types::NotificationKind::ToolRemoved),
                    "[svc] tool removed: example_tool",
                ),
                BlockSnapshot::text(BlockId::new(c, m, 1), None, BlockRole::Model, "after"),
            ];
            let msgs = hydrate_from_blocks(&blocks);
            assert_eq!(
                msgs.len(),
                4,
                "user → assistant(mid) → user(notification) → assistant(after)"
            );
            assert_eq!(msgs[0].role, Role::User);
            assert_eq!(msgs[1].role, Role::Assistant);
            assert_eq!(msgs[1].as_text(), Some("mid"));
            assert_eq!(msgs[2].role, Role::User);
            assert!(
                msgs[2]
                    .as_text()
                    .expect("notification text")
                    .contains("kind=\"tool_removed\"")
            );
            assert_eq!(msgs[3].role, Role::Assistant);
            assert_eq!(msgs[3].as_text(), Some("after"));
        }

        #[test]
        fn notification_block_survives_system_role_filter() {
            // Notification blocks are authored by System principal
            // (`BlockSnapshot::notification_block` forces Role::System).
            // The hydrator's System-role filter skips Role::System blocks
            // unless kind is Drift, Error, or Notification. This test locks
            // that carve-out so a Notification block reaches the match arm
            // instead of being silently dropped.
            let c = ctx();
            let s = system();
            let block = BlockSnapshot::notification_block(
                BlockId::new(c, s, 0),
                None,
                notif_payload("gpal", kaijutsu_types::NotificationKind::Log),
                "[gpal] info: heartbeat",
            );
            assert_eq!(block.role, BlockRole::System, "sanity: role is System");
            let msgs = hydrate_from_blocks(&[block]);
            assert_eq!(
                msgs.len(),
                1,
                "System-role Notification must not be filtered out"
            );
        }

        // ── (Tool, Text) content-typed blocks (svg_block / abc_block, A1) ──

        #[test]
        fn tool_text_svg_block_hydrates_with_envelope() {
            let c = ctx();
            let m = model();
            let svg = "<svg viewBox='0 0 10 10'><circle cx='5' cy='5' r='3'/></svg>";
            let block = kaijutsu_types::BlockSnapshotBuilder::new(
                BlockId::new(c, m, 0),
                kaijutsu_types::BlockKind::Text,
            )
            .role(BlockRole::Tool)
            .content(svg)
            .content_type(kaijutsu_types::ContentType::Svg)
            .build();

            let msgs = hydrate_from_blocks(&[block]);
            assert_eq!(msgs.len(), 1, "(Tool, Text, Svg) block must hydrate");
            assert_eq!(msgs[0].role, Role::User);
            let text = msgs[0].as_text().expect("envelope is text");
            assert!(text.contains("svg"), "envelope mentions svg, got: {text}");
            assert!(
                text.contains(svg),
                "envelope includes svg source, got: {text}"
            );
        }

        #[test]
        fn tool_text_abc_block_hydrates_with_envelope() {
            let c = ctx();
            let m = model();
            let abc = "X:1\nT:Test\nK:C\nCDEF GABc";
            let block = kaijutsu_types::BlockSnapshotBuilder::new(
                BlockId::new(c, m, 0),
                kaijutsu_types::BlockKind::Text,
            )
            .role(BlockRole::Tool)
            .content(abc)
            .content_type(kaijutsu_types::ContentType::Abc)
            .build();

            let msgs = hydrate_from_blocks(&[block]);
            assert_eq!(msgs.len(), 1, "(Tool, Text, Abc) block must hydrate");
            assert_eq!(msgs[0].role, Role::User);
            let text = msgs[0].as_text().expect("envelope is text");
            assert!(text.contains("abc"), "envelope mentions abc, got: {text}");
            assert!(
                text.contains("CDEF GABc"),
                "envelope includes abc source, got: {text}"
            );
        }

        #[test]
        fn tool_text_diff_block_hydrates_with_diffstat_and_hunk_bounded_body() {
            // Producer 1 of 2: `diff_block` writes a (Tool, Text) block, the
            // svg_block/abc_block rail. Slice 3 replaces slice 2's passthrough
            // with a projection — the diffstat leads and the body is canonical.
            let c = ctx();
            let m = model();
            let diff = "--- a/foo.txt\n+++ b/foo.txt\n@@ -1 +1 @@\n-old\n+new\n";
            let block = kaijutsu_types::BlockSnapshotBuilder::new(
                BlockId::new(c, m, 0),
                kaijutsu_types::BlockKind::Text,
            )
            .role(BlockRole::Tool)
            .content(diff)
            .content_type(kaijutsu_types::ContentType::Diff)
            .build();

            let msgs = hydrate_from_blocks(&[block]);
            assert_eq!(msgs.len(), 1, "(Tool, Text, Diff) block must hydrate");
            assert_eq!(msgs[0].role, Role::User);
            let text = msgs[0].as_text().expect("envelope is text");
            assert!(
                text.contains("x-diff"),
                "envelope mentions the diff content_type, got: {text}"
            );
            assert!(
                text.contains("1 file, +1 \u{2212}1"),
                "the diffstat leads the body, got: {text}"
            );
            assert!(text.contains("-old"), "got: {text}");
            assert!(text.contains("+new"), "got: {text}");
            assert!(
                !text.contains("...[truncated]"),
                "a small diff must not be truncated at all: {text}"
            );
        }

        /// A unified diff big enough to blow the hydration budget, plus the
        /// stat it should report. Built through the real engine so the text is
        /// canonical and the hunk structure is real.
        fn oversized_diff() -> (String, kaijutsu_diff::DiffStat) {
            let before: String = (1..=4000).map(|i| format!("line {i}\n")).collect();
            // Change every tenth line: many well-separated hunks, so a
            // whole-hunk cut has plenty of legal boundaries to land on.
            let after: String = (1..=4000)
                .map(|i| {
                    if i % 10 == 0 {
                        format!("CHANGED {i}\n")
                    } else {
                        format!("line {i}\n")
                    }
                })
                .collect();
            let file = kaijutsu_diff::diff_file(
                &kaijutsu_diff::FileSpec::modified("big.txt", &before, &after),
                &kaijutsu_diff::DiffOptions::default(),
            )
            .expect("diff");
            let model = kaijutsu_diff::DiffModel::new(vec![file]);
            let text = kaijutsu_diff::format(&model);
            assert!(
                text.len() > kaijutsu_diff::limits::MAX_HYDRATION_BYTES,
                "fixture must exceed the hydration budget"
            );
            (text, model.stat())
        }

        #[test]
        fn an_oversized_diff_hydrates_truncated_on_hunk_boundaries() {
            let c = ctx();
            let m = model();
            let (diff, stat) = oversized_diff();
            let block = kaijutsu_types::BlockSnapshotBuilder::new(
                BlockId::new(c, m, 0),
                kaijutsu_types::BlockKind::Text,
            )
            .role(BlockRole::Tool)
            .content(&diff)
            .content_type(kaijutsu_types::ContentType::Diff)
            .build();

            let msgs = hydrate_from_blocks(&[block]);
            let text = msgs[0].as_text().expect("envelope is text");

            // The stat describes the WHOLE diff, not the surviving fragment —
            // that is how the model learns the true size of the change.
            assert!(
                text.contains(&stat.to_string()),
                "full diffstat must lead, got: {}",
                &text[..text.len().min(200)]
            );
            // Incompleteness is explicit, and it leads the diff body.
            let body = text
                .split_once(&format!("{}\n", stat))
                .expect("stat line then body")
                .1
                .strip_suffix("\n</tool_output>")
                .expect("envelope closes");
            assert!(
                body.starts_with(kaijutsu_diff::TRUNCATION_MARKER_PREFIX),
                "the truncation marker must lead the diff text, got: {}",
                &body[..body.len().min(120)]
            );
            // Never the char-count path: this is the marker that would appear
            // if the generic truncation had run.
            assert!(
                !text.contains("...[truncated]"),
                "a diff must never ride the char-count truncation"
            );
            // And the projection is still a parseable diff — every surviving
            // `@@` count matches its body, which a mid-hunk cut would break.
            let reparsed = kaijutsu_diff::parse(body).expect("projection must parse");
            assert!(
                !reparsed.is_complete(),
                "the reparsed projection must know it is incomplete"
            );
            assert!(
                body.len() <= kaijutsu_diff::limits::MAX_HYDRATION_BYTES,
                "projection must fit the budget"
            );
        }

        #[test]
        fn a_declared_diff_that_does_not_parse_hydrates_as_plain_text() {
            // Content and content_type are separate LWW registers, so this is a
            // legitimate state, not corruption. Dropping the block would lose
            // the model's own output; crashing would take down the turn.
            let c = ctx();
            let m = model();
            let block = kaijutsu_types::BlockSnapshotBuilder::new(
                BlockId::new(c, m, 0),
                kaijutsu_types::BlockKind::Text,
            )
            .role(BlockRole::Tool)
            .content("this is definitely not a unified diff")
            .content_type(kaijutsu_types::ContentType::Diff)
            .build();

            let msgs = hydrate_from_blocks(&[block]);
            assert_eq!(msgs.len(), 1, "a malformed diff block must still hydrate");
            let text = msgs[0].as_text().expect("envelope is text");
            assert!(
                text.contains("does not parse"),
                "the model must be told what happened, got: {text}"
            );
            assert!(
                text.contains("this is definitely not a unified diff"),
                "the content itself must survive, got: {text}"
            );
        }

        /// Producer 2 of 2: `kj diff` output rides a **ToolResult** block with
        /// `content_type = text/x-diff`, a different hydration branch entirely.
        /// The projection has to be applied on both roads or one producer
        /// silently ships an unbounded diff into the context window.
        #[test]
        fn kj_diff_tool_result_hydrates_through_the_projection() {
            let c = ctx();
            let m = model();
            let s = system();
            let (diff, stat) = oversized_diff();

            let call_id = BlockId::new(c, m, 0);
            let tool_call = BlockSnapshot::tool_call(
                call_id,
                None,
                ToolKind::Shell,
                "shell",
                serde_json::json!({"code": "kj diff /mnt/p/big.txt"}),
                BlockRole::Model,
                Some("toolu_diff".to_string()),
            );
            let mut tool_result = BlockSnapshot::tool_result(
                BlockId::new(c, s, 0),
                call_id,
                ToolKind::Shell,
                &diff,
                false,
                Some(0),
                Some("toolu_diff".to_string()),
            );
            tool_result.content_type = kaijutsu_types::ContentType::Diff;

            let msgs = hydrate_from_blocks(&[tool_call, tool_result]);
            let MessageContent::Blocks(blocks) = &msgs[1].content else {
                panic!("expected tool_result blocks, got {:?}", msgs[1].content);
            };
            let ContentBlock::ToolResult { content, .. } = &blocks[0] else {
                panic!("expected a ToolResult block");
            };
            assert!(
                content.starts_with(&stat.to_string()),
                "kj diff output must hydrate with its diffstat, got: {}",
                &content[..content.len().min(120)]
            );
            assert!(
                content.contains(kaijutsu_diff::TRUNCATION_MARKER_PREFIX),
                "and with the explicit truncation marker"
            );
            assert!(
                content.len() < diff.len(),
                "the projection must be smaller than the canonical block"
            );
        }

        #[test]
        fn user_run_kj_diff_hydrates_through_the_projection() {
            // The same ToolResult arm, other sub-branch: a human typed it at
            // the kaish prompt, so it lands as `[User ran ...]`.
            let c = ctx();
            let u = user();
            let s = system();
            let diff = "--- a/foo.txt\n+++ b/foo.txt\n@@ -1 +1 @@\n-old\n+new\n";

            let call_id = BlockId::new(c, u, 0);
            let tool_call = BlockSnapshot::tool_call(
                call_id,
                None,
                ToolKind::Shell,
                "shell",
                serde_json::json!({"code": "kj diff /mnt/p/foo.txt"}),
                BlockRole::User,
                None,
            );
            let mut tool_result = BlockSnapshot::tool_result(
                BlockId::new(c, s, 0),
                call_id,
                ToolKind::Shell,
                diff,
                false,
                Some(0),
                None,
            );
            tool_result.content_type = kaijutsu_types::ContentType::Diff;

            let msgs = hydrate_from_blocks(&[tool_call, tool_result]);
            assert_eq!(msgs.len(), 1);
            let text = msgs[0].as_text().unwrap();
            assert!(text.contains("[User ran `kj diff /mnt/p/foo.txt`]"), "got: {text}");
            assert!(
                text.contains("1 file, +1 \u{2212}1"),
                "the diffstat must reach the model here too, got: {text}"
            );
        }

        #[test]
        fn tool_text_plain_still_skipped() {
            // Tool-role text blocks without a rich content_type are noise (not
            // produced by any current engine); skip them so we don't surface
            // arbitrary tool-authored prose to the model on every turn.
            let c = ctx();
            let m = model();
            let block = kaijutsu_types::BlockSnapshotBuilder::new(
                BlockId::new(c, m, 0),
                kaijutsu_types::BlockKind::Text,
            )
            .role(BlockRole::Tool)
            .content("internal noise")
            .build();

            let msgs = hydrate_from_blocks(&[block]);
            assert!(
                msgs.is_empty(),
                "(Tool, Text, Plain) must remain skipped, got {msgs:?}"
            );
        }

        // ── Role::Asset image hydration (A2) ──

        #[test]
        fn asset_image_block_hydrates_as_image_content_block() {
            let c = ctx();
            let m = model();
            // img_block / img_block_from_path produce (Asset, Text, Image)
            // blocks where `content` holds the CAS hash.
            let block = kaijutsu_types::BlockSnapshotBuilder::new(
                BlockId::new(c, m, 0),
                kaijutsu_types::BlockKind::Text,
            )
            .role(BlockRole::Asset)
            .content("abcdef0123456789")
            .content_type(kaijutsu_types::ContentType::Image)
            .build();

            let msgs = hydrate_from_blocks(&[block]);
            assert_eq!(msgs.len(), 1, "Asset image block must hydrate");
            assert_eq!(msgs[0].role, Role::User);
            match &msgs[0].content {
                MessageContent::Blocks(blocks) => {
                    assert_eq!(blocks.len(), 1);
                    match &blocks[0] {
                        ContentBlock::Image {
                            hash,
                            media_type,
                            data_base64,
                        } => {
                            assert_eq!(hash, "abcdef0123456789");
                            assert!(
                                media_type.starts_with("image/"),
                                "media_type should look like a MIME image type, got: {media_type}"
                            );
                            assert!(
                                data_base64.is_none(),
                                "hydrator emits hash only; CAS resolution happens later"
                            );
                        }
                        other => panic!("Expected ContentBlock::Image, got {other:?}"),
                    }
                }
                other => panic!("Expected Blocks, got {other:?}"),
            }
        }

        #[tokio::test]
        async fn resolve_image_blocks_fills_data_from_cas() {
            use kaijutsu_cas::{ContentStore, FileStore};
            let tmp = tempfile::tempdir().unwrap();
            let cas: std::sync::Arc<dyn ContentStore> =
                std::sync::Arc::new(FileStore::at_path(tmp.path()));
            // 1x1 transparent PNG to keep the fixture small but legitimate.
            let png_bytes: &[u8] = &[
                0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
                0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
                0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78,
                0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
                0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
            ];
            let hash = cas.store(png_bytes, "image/png").unwrap();
            let mut messages = vec![Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![ContentBlock::Image {
                    hash: hash.to_string(),
                    media_type: "image/png".to_string(),
                    data_base64: None,
                }]),
            }];
            resolve_image_blocks_from_cas(&mut messages, cas, None).await;
            match &messages[0].content {
                MessageContent::Blocks(blocks) => match &blocks[0] {
                    ContentBlock::Image { data_base64, .. } => {
                        assert!(
                            data_base64.is_some(),
                            "data_base64 must be filled after resolve"
                        );
                    }
                    _ => panic!("expected Image"),
                },
                _ => panic!("expected Blocks"),
            }
        }

        #[tokio::test]
        async fn resolve_image_blocks_tolerates_missing_hash() {
            use kaijutsu_cas::{ContentStore, FileStore};
            let tmp = tempfile::tempdir().unwrap();
            let cas: std::sync::Arc<dyn ContentStore> =
                std::sync::Arc::new(FileStore::at_path(tmp.path()));
            let mut messages = vec![Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![ContentBlock::Image {
                    hash: "0".repeat(64),
                    media_type: "image/png".to_string(),
                    data_base64: None,
                }]),
            }];
            // Should not panic, should leave block unresolved.
            resolve_image_blocks_from_cas(&mut messages, cas, None).await;
            match &messages[0].content {
                MessageContent::Blocks(blocks) => match &blocks[0] {
                    ContentBlock::Image { data_base64, .. } => {
                        assert!(data_base64.is_none(), "missing hash stays unresolved");
                    }
                    _ => panic!(),
                },
                _ => panic!(),
            }
        }

        /// `ContentStore` wrapper that counts `retrieve` calls so tests can
        /// assert the cache short-circuits disk reads.
        struct CountingStore {
            inner: std::sync::Arc<dyn kaijutsu_cas::ContentStore>,
            retrieves: std::sync::atomic::AtomicUsize,
            inspects: std::sync::atomic::AtomicUsize,
        }
        impl CountingStore {
            fn new(inner: std::sync::Arc<dyn kaijutsu_cas::ContentStore>) -> Self {
                Self {
                    inner,
                    retrieves: std::sync::atomic::AtomicUsize::new(0),
                    inspects: std::sync::atomic::AtomicUsize::new(0),
                }
            }
            fn retrieves(&self) -> usize {
                self.retrieves.load(std::sync::atomic::Ordering::Relaxed)
            }
            fn inspects(&self) -> usize {
                self.inspects.load(std::sync::atomic::Ordering::Relaxed)
            }
        }
        impl kaijutsu_cas::ContentStore for CountingStore {
            fn store(
                &self,
                data: &[u8],
                mime_type: &str,
            ) -> Result<kaijutsu_cas::ContentHash, kaijutsu_cas::StoreError> {
                self.inner.store(data, mime_type)
            }
            fn retrieve(
                &self,
                hash: &kaijutsu_cas::ContentHash,
            ) -> Result<Option<Vec<u8>>, kaijutsu_cas::StoreError> {
                self.retrieves
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.inner.retrieve(hash)
            }
            fn exists(&self, hash: &kaijutsu_cas::ContentHash) -> bool {
                self.inner.exists(hash)
            }
            fn path(&self, hash: &kaijutsu_cas::ContentHash) -> Option<std::path::PathBuf> {
                self.inner.path(hash)
            }
            fn inspect(
                &self,
                hash: &kaijutsu_cas::ContentHash,
            ) -> Result<Option<kaijutsu_cas::CasReference>, kaijutsu_cas::StoreError> {
                self.inspects
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.inner.inspect(hash)
            }
            fn remove(
                &self,
                hash: &kaijutsu_cas::ContentHash,
            ) -> Result<bool, kaijutsu_cas::StoreError> {
                self.inner.remove(hash)
            }
        }

        #[tokio::test]
        async fn resolve_image_blocks_uses_cache_on_second_call() {
            use kaijutsu_cas::{ContentStore, FileStore};
            let tmp = tempfile::tempdir().unwrap();
            let backing: std::sync::Arc<dyn ContentStore> =
                std::sync::Arc::new(FileStore::at_path(tmp.path()));
            let png_bytes: &[u8] = &[
                0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
                0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
                0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78,
                0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
                0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
            ];
            let hash = backing.store(png_bytes, "image/png").unwrap();
            let counting = std::sync::Arc::new(CountingStore::new(backing));
            let cas: std::sync::Arc<dyn ContentStore> = counting.clone();
            let cache = image_cache::ImageBase64Cache::new(8);

            let make_msgs = || {
                vec![Message {
                    role: Role::User,
                    content: MessageContent::Blocks(vec![ContentBlock::Image {
                        hash: hash.to_string(),
                        media_type: "image/png".to_string(),
                        data_base64: None,
                    }]),
                }]
            };

            let mut first = make_msgs();
            resolve_image_blocks_from_cas(&mut first, cas.clone(), Some(&cache)).await;
            assert_eq!(counting.retrieves(), 1, "first resolve must read CAS once");
            assert_eq!(counting.inspects(), 1, "first resolve must inspect once");

            let mut second = make_msgs();
            resolve_image_blocks_from_cas(&mut second, cas.clone(), Some(&cache)).await;
            assert_eq!(counting.retrieves(), 1, "cached resolve must skip CAS");
            assert_eq!(counting.inspects(), 1, "cached resolve must skip inspect");

            match &second[0].content {
                MessageContent::Blocks(blocks) => match &blocks[0] {
                    ContentBlock::Image { data_base64, .. } => assert!(
                        data_base64.is_some(),
                        "cached resolve must still fill data_base64"
                    ),
                    _ => panic!("expected Image"),
                },
                _ => panic!("expected Blocks"),
            }
        }

        #[test]
        fn asset_text_plain_still_skipped() {
            // Asset role with non-Image content_type is not produced by any
            // current engine; skip to avoid surfacing arbitrary asset prose.
            let c = ctx();
            let m = model();
            let block = kaijutsu_types::BlockSnapshotBuilder::new(
                BlockId::new(c, m, 0),
                kaijutsu_types::BlockKind::Text,
            )
            .role(BlockRole::Asset)
            .content("plain asset text")
            .build();

            let msgs = hydrate_from_blocks(&[block]);
            assert!(
                msgs.is_empty(),
                "(Asset, Text, Plain) must remain skipped, got {msgs:?}"
            );
        }

        #[test]
        fn tool_text_long_svg_truncates() {
            let c = ctx();
            let m = model();
            let huge: String = "x".repeat(kaijutsu_types::TOOL_CONTENT_HYDRATION_BUDGET + 100);
            let block = kaijutsu_types::BlockSnapshotBuilder::new(
                BlockId::new(c, m, 0),
                kaijutsu_types::BlockKind::Text,
            )
            .role(BlockRole::Tool)
            .content(&huge)
            .content_type(kaijutsu_types::ContentType::Svg)
            .build();

            let msgs = hydrate_from_blocks(&[block]);
            assert_eq!(msgs.len(), 1);
            let text = msgs[0].as_text().unwrap();
            assert!(
                text.contains("[truncated]"),
                "long body must show truncation marker"
            );
        }

        /// Regression for the deepseek post-merge review (docs/issues.md,
        /// "⛔ Interrupted marker leaks into model context"): the hard-cancel
        /// marker used to land as `(Role::Model, BlockKind::Text)`, which
        /// hydration folds straight into `assistant_text` — the model's next
        /// turn would read its own prior turn as having said "⛔ Interrupted"
        /// verbatim. `llm_stream.rs` now inserts it as `(Role::System,
        /// BlockKind::Text)` *and* ephemeral; either alone is enough to be
        /// hydration-skipped, but both apply in practice, so this test pins
        /// each independently plus the real shape together.
        #[test]
        fn interrupted_marker_shapes_never_reach_hydrated_messages() {
            let c = ctx();
            let m = model();
            let s = system();

            let role_system_and_ephemeral = kaijutsu_types::BlockSnapshotBuilder::new(
                BlockId::new(c, s, 0),
                kaijutsu_types::BlockKind::Text,
            )
            .role(BlockRole::System)
            .content("⛔ Interrupted")
            .ephemeral(true)
            .build();

            let role_system_only = kaijutsu_types::BlockSnapshotBuilder::new(
                BlockId::new(c, s, 1),
                kaijutsu_types::BlockKind::Text,
            )
            .role(BlockRole::System)
            .content("⛔ Interrupted")
            .build();

            let ephemeral_only = kaijutsu_types::BlockSnapshotBuilder::new(
                BlockId::new(c, m, 1),
                kaijutsu_types::BlockKind::Text,
            )
            .role(BlockRole::Model)
            .content("⛔ Interrupted")
            .ephemeral(true)
            .build();

            for (name, block) in [
                ("role=System + ephemeral (the real shape)", role_system_and_ephemeral),
                ("role=System alone", role_system_only),
                ("ephemeral alone", ephemeral_only),
            ] {
                let msgs = hydrate_from_blocks(&[block]);
                assert!(
                    msgs.is_empty(),
                    "{name}: marker must not reach hydrated messages, got {msgs:?}"
                );
            }

            // Sanity check the regression is real: the OLD shape — plain
            // (Role::Model, BlockKind::Text) with no ephemeral flag — DOES
            // fold into assistant_text. If this assertion ever fails, the
            // three cases above stopped testing anything meaningful.
            let old_shape = BlockSnapshot::text(
                BlockId::new(c, m, 2),
                None,
                BlockRole::Model,
                "⛔ Interrupted",
            );
            let msgs = hydrate_from_blocks(&[old_shape]);
            assert_eq!(
                msgs.len(),
                1,
                "the OLD leaking shape must still fold into assistant_text \
                 (proves the fixed shapes above are actually being exercised)"
            );
            assert_eq!(msgs[0].as_text(), Some("⛔ Interrupted"));
        }
    }
}
