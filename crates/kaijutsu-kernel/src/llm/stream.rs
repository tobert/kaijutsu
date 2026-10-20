//! Streaming primitives for LLM responses.
//!
//! Provider-agnostic types that flow from per-provider `Client::stream()`
//! into the model-turn writer in `kaijutsu-kernel`. Each per-provider
//! client (`super::claude`, `super::openai`, `super::deepseek`) owns
//! translation from kaijutsu's `Message` / `ContentBlock` into the
//! provider's native wire shape and emits the events below.
//!
//! ```text
//! ┌──────────────────┐   ┌──────────────────┐
//! │ claude::Client   │   │ openai::Client   │   …
//! │   .stream(opts)  │   │   .stream(opts)  │
//! └────────┬─────────┘   └────────┬─────────┘
//!          │                      │
//!          ▼                      ▼
//!          ┌──────────────────────────────────┐
//!          │       StreamEvent (this file)    │
//!          │   (kernel model-turn writer)      │
//!          └──────────────────────────────────┘
//! ```

use serde::{Deserialize, Serialize};

use super::ToolDefinition;
use super::config::SlotTunables;

/// Provider-agnostic streaming events from an LLM completion.
///
/// Lifecycle (within a single completion):
///
/// 1. `ThinkingStart` → `ThinkingDelta(_)*` → `ThinkingEnd` (extended thinking)
/// 2. `TextStart` → `TextDelta(_)*` → `TextEnd` (runs may alternate with thinking)
/// 3. `ToolUse { … }` (zero or more, atomic once emitted), or
///    `InlineToolUse { … }` followed by a provider callback result
/// 4. `Done { … }` or `Error(_)` — terminal
///
/// The kernel block writer relies on `*Start` / `*End` bracketing each
/// text/thinking run — provider implementations must close the current
/// block before opening another, emitting a tool call, or sending `Done`.
/// Deltas and ends must match the open content kind. The writer rejects
/// malformed framing and EOF before a terminal event; `Done` ends consumption
/// immediately. Hard cancellation retains the accepted prefix and may drain
/// terminal usage without accepting further content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StreamEvent {
    /// Start of an extended-thinking block (reasoning before responding).
    ThinkingStart,
    /// Incremental text delta for the current thinking block.
    ThinkingDelta(String),
    /// End of the current thinking block.
    ///
    /// `signature` is the provider's continuity token for the block, and
    /// only that provider can use it: Anthropic's `signature_delta` (base64),
    /// a UUID from DeepSeek's Messages endpoint, a sentinel the OpenAI-compatible client and `mk` write (reasoning
    /// they round-trip as text), or, for a `redacted_thinking` block, its
    /// opaque `data` behind the `anthropic:redacted_thinking:` prefix. The
    /// block writer stores it on the block and threads it into
    /// [`crate::llm::ContentBlock::Reasoning`] so a later request can send
    /// the block back; a provider drops a signature it did not produce
    /// (`llm/claude/build.rs`). The block can have no text when the API
    /// withheld or omitted it; its signature still goes back. `None` when
    /// the provider emitted none.
    ThinkingEnd {
        signature: Option<String>,
    },

    /// Start of a text response block.
    TextStart,
    /// Incremental text delta for the current text block.
    TextDelta(String),
    /// End of the current text block.
    TextEnd,

    /// Tool invocation request (immutable once emitted).
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },

    /// A tool call whose arguments did not parse as JSON, most often because
    /// the response stopped at the output ceiling part-way through them.
    ///
    /// The call is real and the model believes it made it, so the runtime
    /// records it and answers it with an error tool result instead of failing
    /// the turn. `arguments` is the raw text as it arrived; `error` is the
    /// parser's message, which names the position it stopped at.
    ToolUseInvalid {
        id: String,
        name: String,
        arguments: String,
        error: String,
    },

    /// A tool request that must be answered before the provider can continue
    /// this stream.
    ///
    /// Most completion APIs end a response at `ToolUse`; kaijutsu writes the
    /// tool result and starts a fresh completion.  Agent runtimes such as the
    /// Codex app-server instead send a bidirectional request while their turn
    /// is still live.  The server handles this event synchronously through
    /// the ordinary broker and calls [`crate::llm::ProviderStream::respond_inline_tool`]
    /// before it polls the next event.  That preserves the one broker/kaish
    /// ownership path while retaining the runtime's single live turn.
    ///
    /// Providers must close any open text/thinking run before emitting this
    /// event, just as they do for [`Self::ToolUse`].
    InlineToolUse {
        /// Provider callback identifier.  It is opaque to Kaijutsu and is
        /// returned unchanged to the provider in `InlineToolResult`.
        id: String,
        name: String,
        input: serde_json::Value,
    },

    /// Tool execution result (produced by the runtime, not the model).
    /// Reserved on the wire for symmetry with [`ToolUse`]; the server
    /// generates these locally and does not see them on the stream.
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },

    /// Generation completed.
    ///
    /// `stop_reason` is kept as `Option<String>` for wire-compat with the
    /// server-side log/cancel checks. `input_tokens` / `output_tokens`
    /// are the common counts; `extra` carries provider-specific usage
    /// accounting (Anthropic cache stats, DeepSeek cache hit/miss +
    /// reasoning tokens) so it reaches the telemetry layer instead of
    /// being dropped on the floor. `None` when the provider reported no
    /// extra (or on a cancel-confirm `Done`).
    Done {
        stop_reason: Option<String>,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        extra: Option<UsageExtra>,
    },

    /// Generation failed. The kind says whether a repeat can succeed.
    Error(StreamError),
}

/// Whether sending the same request again can succeed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDisposition {
    /// A provider-side hiccup: retry under the bounded backoff.
    Transient,
    /// The same request fails the same way: stop and report it.
    Permanent,
}

/// What kind of failure ended a stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamErrorKind {
    /// The provider has no capacity for the request (Anthropic 529).
    Overloaded,
    /// The account's rate limit answered the request.
    RateLimited,
    /// An internal failure on the provider's side.
    ServerError,
    /// The provider, or the runtime's own deadline, timed out.
    Timeout,
    /// The provider cut the reply for lack of capacity (`insufficient_system_resource`).
    CapacityCut,
    /// The provider's content filter ended the reply.
    ContentFilter,
    /// The model stopped before it finished: `pause_turn`,
    /// `model_context_window_exceeded`, or a stop reason this client does not
    /// know or did not receive. Holds the reason.
    Truncated(String),
    /// The credential was refused.
    Auth,
    /// The provider rejected the request.
    InvalidRequest,
    /// The account cannot pay for the request.
    Billing,
    /// The connection failed while the stream was open.
    Transport,
    /// The provider's events broke the wire contract: undecodable data, a
    /// repeated or unopened content block.
    Protocol,
    /// An error type this client does not know. Holds the provider's type name.
    Unknown(String),
}

impl StreamErrorKind {
    /// Whether the same request may succeed on another attempt.
    pub fn retry_disposition(&self) -> RetryDisposition {
        match self {
            Self::Overloaded
            | Self::RateLimited
            | Self::ServerError
            | Self::Timeout
            | Self::CapacityCut
            | Self::Transport => RetryDisposition::Transient,
            Self::ContentFilter
            | Self::Truncated(_)
            | Self::Auth
            | Self::InvalidRequest
            | Self::Billing
            | Self::Protocol
            | Self::Unknown(_) => RetryDisposition::Permanent,
        }
    }

    /// The name logs and error blocks lead with.
    pub fn label(&self) -> &str {
        match self {
            Self::Overloaded => "overloaded_error",
            Self::RateLimited => "rate_limit_error",
            Self::ServerError => "api_error",
            Self::Timeout => "timeout_error",
            Self::CapacityCut => "insufficient_system_resource",
            Self::ContentFilter => "content_filter",
            Self::Truncated(reason) | Self::Unknown(reason) => reason,
            Self::Auth => "authentication_error",
            Self::InvalidRequest => "invalid_request_error",
            Self::Billing => "billing_error",
            Self::Transport => "transport",
            Self::Protocol => "protocol",
        }
    }
}

/// The failure that ended a stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamError {
    pub kind: StreamErrorKind,
    pub message: String,
    /// The provider's id for the failed request, when it sent one.
    pub request_id: Option<String>,
}

impl StreamError {
    pub fn new(kind: StreamErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into(), request_id: None }
    }

    pub fn with_request_id(mut self, request_id: Option<String>) -> Self {
        self.request_id = request_id;
        self
    }
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind.label(), self.message)?;
        if let Some(id) = &self.request_id {
            write!(f, " (request_id: {id})")?;
        }
        Ok(())
    }
}

/// Result returned to a provider for [`StreamEvent::InlineToolUse`].
///
/// This deliberately carries only the portable tool-result shape.  A
/// provider owns its wire-specific callback envelope (Codex uses
/// `contentItems`); kaijutsu owns execution and its durable kernel blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InlineToolResult {
    pub content: String,
    pub is_error: bool,
}

impl StreamEvent {
    pub fn is_delta(&self) -> bool {
        matches!(self, Self::ThinkingDelta(_) | Self::TextDelta(_))
    }
    pub fn is_start(&self) -> bool {
        matches!(self, Self::ThinkingStart | Self::TextStart)
    }
    pub fn is_end(&self) -> bool {
        matches!(self, Self::ThinkingEnd { .. } | Self::TextEnd)
    }
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done { .. } | Self::Error(_))
    }
    pub fn as_delta(&self) -> Option<&str> {
        match self {
            Self::ThinkingDelta(s) | Self::TextDelta(s) => Some(s),
            _ => None,
        }
    }
}

/// Shared knobs applied to every provider stream request.
///
/// Provider-specific features live as typed builder methods on each
/// provider's native request — Claude's extended thinking and per-block
/// `cache_control`, Gemini's `googleSearch` / `codeExecution` — populated
/// inside `Client::stream()` from configuration and context state. Those
/// knobs intentionally do *not* appear here.
///
/// `cache_breakpoints` is the one exception: it's a Claude-specific
/// policy carrier on the shared options because the *policy* of where to
/// cache straddles the conversation shape (system / tools / message
/// index). Gemini's `build()` ignores it. The doc's Phase 0 sketch
/// keyed the map by `BlockId`, but `LlmMessage` doesn't carry block
/// identity past hydration — Phase 2 keys by [`CacheTarget`] (symbolic +
/// index-based) instead. Phase 2 ships with the carrier empty by
/// design (user pick: "carrier only, no defaults").
#[derive(Debug, Clone)]
pub struct BuildOpts {
    pub model: String,
    pub system: Option<String>,
    pub max_tokens: u64,
    pub temperature: Option<f64>,
    /// Nucleus-sampling cutoff. `None` = provider default. Dropped alongside
    /// `temperature` on Anthropic requests where thinking is enabled (see
    /// `claude::build::apply_thinking`) — the Messages API 400s if either
    /// rides with thinking on.
    pub top_p: Option<f64>,
    /// Provider effort-ladder token, passthrough with NO allowlist (each
    /// provider's `build()` interprets or ignores it; unrecognized strings
    /// ride straight to the wire — the provider's own 400, not ours, is the
    /// validation). Anthropic: adaptive tier's `output_config.effort`.
    /// DeepSeek: `reasoning_effort`, with `"none"` special-cased to the
    /// structural `thinking: {"type": "disabled"}`. Hosted OpenAI
    /// (`api.openai.com`): `reasoning_effort`. Everywhere else: inert
    /// (warned if explicitly set — see each provider's `build`).
    pub effort: Option<String>,
    /// Anthropic budget-tier thinking token budget. Only meaningful when
    /// `thinking_style == Some("budget")` on Claude; inert (warned) on every
    /// other backend/style combination — there is no budget sink outside
    /// that one tier.
    pub thinking_budget: Option<u64>,
    /// `"auto"` (default/unset — current model-gated behavior) | `"adaptive"`
    /// (force the adaptive tier) | `"budget"` (force `Enabled { budget_tokens
    /// }`, which requires `thinking_budget` to be set — see
    /// `claude::build::resolve_thinking`). Validated on Claude; ignored
    /// elsewhere (no other provider has a thinking-style knob yet).
    pub thinking_style: Option<String>,
    pub tools: Vec<ToolDefinition>,
    /// Cache breakpoint policy for Claude prompt caching. Empty = no
    /// `cache_control` applied. See [`CacheTarget`].
    pub cache_breakpoints: Vec<CacheTarget>,
    /// Which inference this request is, for the request sensors
    /// ([`crate::llm::request_dump`]). `None` for requests outside a turn.
    /// Boxed behind an `Arc` so `BuildOpts` stays small: it sits by value in
    /// every provider stream future.
    pub trace: Option<std::sync::Arc<crate::llm::request_dump::RequestTrace>>,
}

impl BuildOpts {
    /// A request for `model` that may produce `max_tokens` tokens. The caller
    /// supplies the limit: the binding's slot for a turn, or the slot a
    /// one-shot prompt runs on.
    pub fn new(model: impl Into<String>, max_tokens: u64) -> Self {
        Self {
            model: model.into(),
            system: None,
            max_tokens,
            temperature: None,
            top_p: None,
            effort: None,
            thinking_budget: None,
            thinking_style: None,
            tools: Vec::new(),
            cache_breakpoints: Vec::new(),
            trace: None,
        }
    }

    pub fn with_trace(mut self, trace: crate::llm::request_dump::RequestTrace) -> Self {
        self.trace = Some(std::sync::Arc::new(trace));
        self
    }

    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u64) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    pub fn with_temperature(mut self, temperature: f64) -> Self {
        self.temperature = Some(temperature);
        self
    }

    pub fn with_top_p(mut self, top_p: f64) -> Self {
        self.top_p = Some(top_p);
        self
    }

    pub fn with_effort(mut self, effort: impl Into<String>) -> Self {
        self.effort = Some(effort.into());
        self
    }

    pub fn with_thinking_budget(mut self, thinking_budget: u64) -> Self {
        self.thinking_budget = Some(thinking_budget);
        self
    }

    pub fn with_thinking_style(mut self, thinking_style: impl Into<String>) -> Self {
        self.thinking_style = Some(thinking_style.into());
        self
    }

    pub fn with_tools(mut self, tools: Vec<ToolDefinition>) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_cache_breakpoints(mut self, breakpoints: Vec<CacheTarget>) -> Self {
        self.cache_breakpoints = breakpoints;
        self
    }
}

/// Put a binding slot's optional parameters on a request. An unset
/// parameter stays unset, so the request omits it. `max_tokens` is not a
/// tunable here: [`BuildOpts::new`] takes it from the caller.
pub fn apply_tunables(mut opts: BuildOpts, tunables: &SlotTunables) -> BuildOpts {
    opts.temperature = tunables.temperature;
    opts.top_p = tunables.top_p;
    opts.effort = tunables.effort.clone();
    opts.thinking_budget = tunables.thinking_budget;
    opts.thinking_style = tunables.thinking_style.clone();
    opts
}

/// Where to place a Claude `cache_control` breakpoint within a request.
///
/// Each variant carries a [`CacheTtl`] so the populator (rc scripts on
/// create / fork / drift; see [`docs/unrig.md`] and the
/// `project_cache_breakpoint_policy` memory) can pick ephemeral vs
/// extended per breakpoint — stable per-session targets (tools, fork
/// points) want `Extended`; targets that drift with the conversation
/// want `Ephemeral`.
///
/// Anthropic allows up to 4 breakpoints per request; the Claude `build()`
/// honors them in declaration order, dedupes (`Tools` and `System` each
/// land at most once; `MessageIndex` dedupes by index), and logs drops
/// for debuggability. Gemini's `build()` ignores all variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheTarget {
    /// Cache the tools array (stable across a session — the biggest
    /// single win for agent loops with a fixed toolset).
    Tools(CacheTtl),
    /// Cache the system prompt block (stable across a session).
    System(CacheTtl),
    /// Cache through the assistant/user message at this 0-based index
    /// in the messages array. The natural target after a fork (the
    /// last shared message with the parent) or after a long pasted
    /// document.
    MessageIndex(usize, CacheTtl),
}

impl CacheTarget {
    /// Extract the TTL associated with this breakpoint.
    pub fn ttl(&self) -> CacheTtl {
        match self {
            Self::Tools(ttl) | Self::System(ttl) | Self::MessageIndex(_, ttl) => *ttl,
        }
    }
}

/// Cache TTL hint. Anthropic offers a default 5-minute ephemeral cache
/// and a 1-hour `extended` variant; choose based on how often the same
/// prefix recurs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheTtl {
    /// 5-minute TTL (Anthropic's default `ephemeral`).
    #[default]
    Ephemeral,
    /// 1-hour TTL (Anthropic's `extended`).
    Extended,
}

impl CacheTtl {
    /// Duration in seconds — the value recorded on `ContextUsageRow::cache_ttl_secs`.
    pub fn as_secs(&self) -> i64 {
        match self {
            Self::Ephemeral => 300,
            Self::Extended => 3600,
        }
    }
}

/// The TTL actually chosen for a request's cache breakpoints, in seconds —
/// the longest of the set when breakpoints carry mixed TTLs, 0 when there
/// are none. This is what a provider path that HONORS `cache_breakpoints`
/// (Claude's `build()`; see `llm/claude/build.rs`) actually applied; a path
/// that ignores the carrier (DeepSeek, Gemini) must not call this — the
/// breakpoints existing in `BuildOpts` there is not the same as a TTL being
/// live on the wire.
pub fn longest_cache_ttl_secs(breakpoints: &[CacheTarget]) -> i64 {
    breakpoints.iter().map(|b| b.ttl().as_secs()).max().unwrap_or(0)
}

/// Token usage from a completed stream.
///
/// `extra` carries provider-specific richness so we don't lose cache /
/// grounding accounting through a lowest-common-denominator shape.
/// Phase 1 defines the carriers; Phase 2 wires Claude cache stats,
/// Phase 3 wires Gemini.
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub extra: Option<UsageExtra>,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

/// Typed provider-specific usage extension.
///
/// Rides on [`StreamEvent::Done`] (which is serde-serialized over the
/// wire), so each variant must round-trip; they hold integers and strings
/// only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsageExtra {
    Claude(ClaudeUsageExtra),
    /// Any OpenAI-compatible chat-completions provider (DeepSeek, a local
    /// lemonade/llama.cpp server, Ollama, OpenAI itself). DeepSeek populates
    /// the cache split + reasoning tokens; leaner servers leave them zero.
    OpenAiCompat(OpenAiCompatUsageExtra),
    /// The megakernel (`llm/mk/`).
    Mk(MkUsageExtra),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeUsageExtra {
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

/// Usage extras for OpenAI-compatible chat-completions providers.
///
/// DeepSeek caches the prompt prefix automatically (no `cache_control`
/// knob), reporting the split in `usage`; `reasoning_tokens` counts the
/// chain-of-thought tokens billed as output on thinking-mode turns. Local
/// servers (lemonade/llama.cpp, Ollama) that don't report a cache split
/// leave these zero — the field carrier is shared, not the guarantee.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenAiCompatUsageExtra {
    pub prompt_cache_hit_tokens: u64,
    pub prompt_cache_miss_tokens: u64,
    pub reasoning_tokens: u64,
}

/// Usage extras for the megakernel. `kept` prompt tokens came from a held
/// prefix and `fed` were prefilled for this call; both are inside
/// `input_tokens`. The times are whole milliseconds: `prefill_ms` and
/// `decode_ms` as the service measured them, `first_token_ms` and `wall_ms`
/// as the provider saw them, queueing behind other GPU work included. `seed`
/// replays the reply under the same weights (`model`, `weight_hash`). A
/// council v2 generation reports no service times and no seed, so those are
/// `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MkUsageExtra {
    pub kept: u64,
    pub fed: u64,
    pub prefill_ms: Option<u64>,
    pub decode_ms: Option<u64>,
    pub first_token_ms: u64,
    pub wall_ms: u64,
    pub seed: Option<u64>,
    pub model: String,
    pub weight_hash: String,
}

/// Common finish reasons plus a typed provider escape hatch.
///
/// Defined in Phase 1 alongside [`StreamEvent::Done`]; Phase 2 wires the
/// real values through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    /// Provider-specific reason that doesn't map cleanly onto the common
    /// set (e.g. Gemini's `SAFETY` or `RECITATION`).
    Provider(String),
}

impl FinishReason {
    pub fn as_str(&self) -> &str {
        match self {
            Self::EndTurn => "end_turn",
            Self::ToolUse => "tool_use",
            Self::MaxTokens => "max_tokens",
            Self::StopSequence => "stop_sequence",
            Self::Provider(s) => s.as_str(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_load_and_connection_failures_are_transient() {
        use StreamErrorKind::*;
        for kind in [Overloaded, RateLimited, ServerError, Timeout, CapacityCut, Transport] {
            assert_eq!(kind.retry_disposition(), RetryDisposition::Transient, "{kind:?}");
        }
        for kind in [
            ContentFilter, Truncated("pause_turn".into()), Auth, InvalidRequest, Billing, Protocol,
            Unknown("x".into()),
        ] {
            assert_eq!(kind.retry_disposition(), RetryDisposition::Permanent, "{kind:?}");
        }
    }

    #[test]
    fn a_stream_error_displays_its_kind_and_message() {
        let error = StreamError::new(StreamErrorKind::Overloaded, "servers are busy");
        assert_eq!(error.to_string(), "overloaded_error: servers are busy");
        let error = StreamError::new(StreamErrorKind::Truncated("pause_turn".into()), "paused")
            .with_request_id(Some("req_1".into()));
        assert_eq!(error.to_string(), "pause_turn: paused (request_id: req_1)");
    }

    #[test]
    fn stream_event_is_delta() {
        assert!(StreamEvent::ThinkingDelta("x".into()).is_delta());
        assert!(StreamEvent::TextDelta("x".into()).is_delta());
        assert!(!StreamEvent::ThinkingStart.is_delta());
        assert!(!StreamEvent::TextStart.is_delta());
    }

    #[test]
    fn stream_event_is_terminal() {
        assert!(
            StreamEvent::Done {
                stop_reason: None,
                input_tokens: None,
                output_tokens: None,
                extra: None,
            }
            .is_terminal()
        );
        assert!(StreamEvent::Error(StreamError::new(StreamErrorKind::Unknown("x".into()), "oops")).is_terminal());
        assert!(!StreamEvent::TextStart.is_terminal());
    }

    #[test]
    fn cache_ttl_as_secs_matches_anthropic_durations() {
        assert_eq!(CacheTtl::Ephemeral.as_secs(), 300);
        assert_eq!(CacheTtl::Extended.as_secs(), 3600);
    }

    #[test]
    fn longest_cache_ttl_secs_is_zero_with_no_breakpoints() {
        assert_eq!(longest_cache_ttl_secs(&[]), 0);
    }

    #[test]
    fn longest_cache_ttl_secs_picks_the_longest_of_mixed_ttls() {
        let breakpoints = vec![
            CacheTarget::Tools(CacheTtl::Ephemeral),
            CacheTarget::System(CacheTtl::Extended),
            CacheTarget::MessageIndex(3, CacheTtl::Ephemeral),
        ];
        assert_eq!(longest_cache_ttl_secs(&breakpoints), 3600);
    }

    #[test]
    fn longest_cache_ttl_secs_single_ephemeral_breakpoint() {
        let breakpoints = vec![CacheTarget::Tools(CacheTtl::Ephemeral)];
        assert_eq!(longest_cache_ttl_secs(&breakpoints), 300);
    }

    #[test]
    fn build_opts_builder() {
        let opts = BuildOpts::new("claude-haiku-4-5", 4096)
            .with_system("be helpful")
            .with_max_tokens(1024)
            .with_temperature(0.7);
        assert_eq!(opts.model, "claude-haiku-4-5");
        assert_eq!(opts.system.as_deref(), Some("be helpful"));
        assert_eq!(opts.max_tokens, 1024);
        assert_eq!(opts.temperature, Some(0.7));
        assert!(opts.tools.is_empty());
    }

    #[test]
    fn finish_reason_as_str() {
        assert_eq!(FinishReason::EndTurn.as_str(), "end_turn");
        assert_eq!(FinishReason::ToolUse.as_str(), "tool_use");
        assert_eq!(FinishReason::Provider("safety".into()).as_str(), "safety");
    }

    #[test]
    fn usage_total_sums_io() {
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 50,
            extra: None,
        };
        assert_eq!(usage.total(), 150);
    }

    #[test]
    fn build_opts_new_defaults_new_tunables_to_none() {
        let opts = BuildOpts::new("m", 4096);
        assert_eq!(opts.top_p, None);
        assert_eq!(opts.effort, None);
        assert_eq!(opts.thinking_budget, None);
        assert_eq!(opts.thinking_style, None);
    }

    #[test]
    fn build_opts_tunable_builders_set_fields() {
        let opts = BuildOpts::new("m", 4096)
            .with_top_p(0.9)
            .with_effort("high")
            .with_thinking_budget(4096)
            .with_thinking_style("budget");
        assert_eq!(opts.top_p, Some(0.9));
        assert_eq!(opts.effort.as_deref(), Some("high"));
        assert_eq!(opts.thinking_budget, Some(4096));
        assert_eq!(opts.thinking_style.as_deref(), Some("budget"));
    }

    mod apply_tunables_tests {
        use super::*;
        use crate::llm::config::SlotTunables;

        fn tunables(max_tokens: Option<u64>) -> SlotTunables {
            SlotTunables {
                max_tokens,
                temperature: Some(0.5),
                top_p: Some(0.8),
                effort: Some("low".into()),
                thinking_budget: Some(1000),
                thinking_style: Some("auto".into()),
            }
        }

        #[test]
        fn every_optional_parameter_reaches_the_request() {
            let opts = apply_tunables(BuildOpts::new("m", 777), &tunables(Some(8000)));
            assert_eq!(opts.max_tokens, 777, "max_tokens comes from the caller, not the tunables");
            assert_eq!(opts.temperature, Some(0.5));
            assert_eq!(opts.top_p, Some(0.8));
            assert_eq!(opts.effort.as_deref(), Some("low"));
            assert_eq!(opts.thinking_budget, Some(1000));
            assert_eq!(opts.thinking_style.as_deref(), Some("auto"));
        }

        #[test]
        fn an_unset_parameter_is_omitted() {
            let opts = apply_tunables(
                BuildOpts::new("m", 100).with_effort("high"),
                &SlotTunables::default(),
            );
            assert_eq!(opts.effort, None, "an unset effort clears what was there");
            assert_eq!(opts.temperature, None);
            assert_eq!(opts.top_p, None);
            assert_eq!(opts.thinking_budget, None);
            assert_eq!(opts.thinking_style, None);
        }
    }
}
