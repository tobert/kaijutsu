//! Streaming primitives for LLM responses.
//!
//! Provider-agnostic types that flow from per-provider `Client::stream()`
//! into the CRDT block writer in `kaijutsu-server`. Each per-provider
//! client (currently `super::claude`, `super::gemini`) owns translation
//! from kaijutsu's `Message` / `ContentBlock` into the provider's native
//! wire shape and emits the events below.
//!
//! ```text
//! ┌──────────────────┐   ┌──────────────────┐
//! │ claude::Client   │   │ gemini::Client   │   …
//! │   .stream(opts)  │   │   .stream(opts)  │
//! └────────┬─────────┘   └────────┬─────────┘
//!          │                      │
//!          ▼                      ▼
//!          ┌──────────────────────────────────┐
//!          │       StreamEvent (this file)    │
//!          │   (CRDT block writer in server)  │
//!          └──────────────────────────────────┘
//! ```
//!
//! Phase 1: shapes are defined; provider `stream()` methods return a loud
//! "not yet implemented" error (see `crates/kaijutsu-kernel/src/llm/claude/mod.rs`
//! and `…/gemini/mod.rs`). Phase 2 lands the Claude wire implementation;
//! Phase 3 lands Gemini.

use serde::{Deserialize, Serialize};

use super::ToolDefinition;

/// Provider-agnostic streaming events from an LLM completion.
///
/// Lifecycle (within a single completion):
///
/// 1. `ThinkingStart` → `ThinkingDelta(_)*` → `ThinkingEnd` (extended thinking)
/// 2. `TextStart` → `TextDelta(_)*` → `TextEnd` (interleavable with thinking)
/// 3. `ToolUse { … }` (zero or more, atomic once emitted)
/// 4. `Done { … }` or `Error(_)` — terminal
///
/// The CRDT block writer relies on `*Start` / `*End` bracketing each
/// text/thinking run — provider implementations must close the current
/// block before opening another or before emitting a tool call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StreamEvent {
    /// Start of an extended-thinking block (reasoning before responding).
    ThinkingStart,
    /// Incremental text delta for the current thinking block.
    ThinkingDelta(String),
    /// End of the current thinking block.
    ///
    /// `signature` carries the provider-specific verification token
    /// (Anthropic's `signature_delta`) when extended thinking is
    /// enabled. The server-side block writer captures it and threads
    /// it into [`crate::llm::ContentBlock::Reasoning`] on the
    /// assistant message so subsequent tool-use turns can echo the
    /// reasoning chain back with its verifying signature. `None` when
    /// the provider didn't emit one (e.g. extended thinking disabled,
    /// or non-Anthropic providers that don't have the concept).
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
    /// `stop_reason` is kept as `Option<String>` for Phase 1 wire-compat
    /// with the existing server-side log/cancel checks. Phase 2 will
    /// re-shape this to carry [`FinishReason`] alongside [`Usage`].
    Done {
        stop_reason: Option<String>,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    },

    /// Error during generation. Carries a human-readable string; Phase 2
    /// will switch to a typed [`StreamError`] variant.
    Error(String),
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
    pub tools: Vec<ToolDefinition>,
    /// Cache breakpoint policy for Claude prompt caching. Empty = no
    /// `cache_control` applied. See [`CacheTarget`].
    pub cache_breakpoints: Vec<CacheTarget>,
}

impl BuildOpts {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            system: None,
            max_tokens: 64_000,
            temperature: None,
            tools: Vec::new(),
            cache_breakpoints: Vec::new(),
        }
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

    pub fn with_tools(mut self, tools: Vec<ToolDefinition>) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_cache_breakpoints(mut self, breakpoints: Vec<CacheTarget>) -> Self {
        self.cache_breakpoints = breakpoints;
        self
    }
}

/// Where to place a Claude `cache_control` breakpoint within a request.
///
/// Anthropic allows up to 4 breakpoints per request; providers honor up
/// to that cap and silently drop the rest. Gemini's `build()` ignores
/// all variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheTarget {
    /// Cache the tools array (stable across a session — the biggest
    /// single win for agent loops with a fixed toolset).
    Tools,
    /// Cache the system prompt block (stable across a session).
    System,
    /// Cache through the assistant/user message at this 0-based index
    /// in the messages array. Useful after a long pasted document.
    MessageIndex(usize),
}

/// Cache TTL hint. Anthropic offers a default 5-minute ephemeral cache
/// and a 1-hour `extended` variant; choose based on how often the same
/// prefix recurs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheTtl {
    /// 5-minute TTL (Anthropic's default `ephemeral`).
    Ephemeral,
    /// 1-hour TTL (Anthropic's `extended`).
    Extended,
}

impl Default for CacheTtl {
    fn default() -> Self {
        Self::Ephemeral
    }
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
#[derive(Debug, Clone)]
pub enum UsageExtra {
    Claude(ClaudeUsageExtra),
    Gemini(GeminiUsageExtra),
}

#[derive(Debug, Clone, Default)]
pub struct ClaudeUsageExtra {
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

#[derive(Debug, Clone, Default)]
pub struct GeminiUsageExtra {
    pub cached_content_tokens: u64,
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

/// Common stream errors plus a typed provider escape hatch.
///
/// Phase 1 defines the variants; Phase 2 will surface these from the
/// Claude wire layer (replacing the current opaque `Error(String)` event).
#[derive(Debug, Clone, thiserror::Error)]
pub enum StreamError {
    #[error("rate limited: {0}")]
    RateLimit(String),
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("server error: {0}")]
    Server(String),
    #[error("overloaded: {0}")]
    Overloaded(String),
    /// Provider-specific error payload that doesn't fit the common shape.
    /// Kaijutsu surfaces errors as JSON to users — homogenization isn't
    /// load-bearing, but the typed variant keeps the carrier honest.
    #[error("provider error: {0}")]
    Provider(String),
}

#[cfg(test)]
mod tests {
    use super::*;

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
            }
            .is_terminal()
        );
        assert!(StreamEvent::Error("oops".into()).is_terminal());
        assert!(!StreamEvent::TextStart.is_terminal());
    }

    #[test]
    fn build_opts_builder() {
        let opts = BuildOpts::new("claude-haiku-4-5")
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
}
