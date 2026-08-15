//! Streaming primitives for LLM responses.
//!
//! Provider-agnostic types that flow from per-provider `Client::stream()`
//! into the CRDT block writer in `kaijutsu-server`. Each per-provider
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
//!          │   (CRDT block writer in server)  │
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
/// 2. `TextStart` → `TextDelta(_)*` → `TextEnd` (interleavable with thinking)
/// 3. `ToolUse { … }` (zero or more, atomic once emitted), or
///    `InlineToolUse { … }` followed by a provider callback result
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

    /// Error during generation. Carries a human-readable string; Phase 2
    /// will switch to a typed [`StreamError`] variant.
    Error(String),
}

/// Result returned to a provider for [`StreamEvent::InlineToolUse`].
///
/// This deliberately carries only the portable tool-result shape.  A
/// provider owns its wire-specific callback envelope (Codex uses
/// `contentItems`); kaijutsu owns execution and its durable CRDT blocks.
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
}

impl BuildOpts {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            system: None,
            max_tokens: 64_000,
            temperature: None,
            top_p: None,
            effort: None,
            thinking_budget: None,
            thinking_style: None,
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

/// Apply a cast's resolved tunables onto a `BuildOpts` in progress — the ONE
/// seam through which [`SlotTunables`] reach a provider request. `slot` is
/// the tunables of the caller's resolved cast seat for this context's role
/// (`resolve_context_model`'s `tunables`, ultimately
/// [`crate::llm::LlmRegistry::resolved_slot`] — already cascaded onto
/// `llm_defaults`); `None` when the context has no cast seat (no cast
/// assigned, or its cast has no slot for this context_type). When `slot` is
/// `None`, `floor` (the bare `llm_defaults` row,
/// [`crate::llm::LlmRegistry::default_tunables`]) supplies the tunables
/// directly, so a kernel with no casts configured at all still gets its
/// `llm_defaults`.
///
/// `max_tokens` carries different precedence than the other knobs: a
/// tunable-supplied value overrides whatever `opts.max_tokens` already held,
/// but when neither `slot` nor `floor` sets it, `opts.max_tokens` is left
/// exactly as the caller built it — never reset to a hardcoded number — so a
/// provider's own clamp-on-zero fallback (`ChatRequest::clamp_max_tokens` for
/// DeepSeek/generic OpenAI) still applies when nothing upstream configured
/// anything. Every other field is set unconditionally from the resolved
/// tunables (including back to `None` — a slot/floor `None` is the honest
/// "provider default" answer, not "leave whatever was there").
///
/// Grep `apply_slot_tunables` to find every call site — this is Track D's
/// seam for wiring a context's cast slot through.
pub fn apply_slot_tunables(
    mut opts: BuildOpts,
    slot: Option<&SlotTunables>,
    floor: &SlotTunables,
) -> BuildOpts {
    let tunables = slot.unwrap_or(floor);
    if let Some(max_tokens) = tunables.max_tokens {
        opts.max_tokens = max_tokens;
    }
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
/// wire), so each variant must round-trip — they're all plain `u64`
/// counts, so that's free.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsageExtra {
    Claude(ClaudeUsageExtra),
    /// Any OpenAI-compatible chat-completions provider (DeepSeek, a local
    /// lemonade/llama.cpp server, Ollama, OpenAI itself). DeepSeek populates
    /// the cache split + reasoning tokens; leaner servers leave them zero.
    OpenAiCompat(OpenAiCompatUsageExtra),
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
                extra: None,
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

    #[test]
    fn build_opts_new_defaults_new_tunables_to_none() {
        let opts = BuildOpts::new("m");
        assert_eq!(opts.top_p, None);
        assert_eq!(opts.effort, None);
        assert_eq!(opts.thinking_budget, None);
        assert_eq!(opts.thinking_style, None);
    }

    #[test]
    fn build_opts_tunable_builders_set_fields() {
        let opts = BuildOpts::new("m")
            .with_top_p(0.9)
            .with_effort("high")
            .with_thinking_budget(4096)
            .with_thinking_style("budget");
        assert_eq!(opts.top_p, Some(0.9));
        assert_eq!(opts.effort.as_deref(), Some("high"));
        assert_eq!(opts.thinking_budget, Some(4096));
        assert_eq!(opts.thinking_style.as_deref(), Some("budget"));
    }

    mod apply_slot_tunables_tests {
        use super::*;
        use crate::llm::config::{ResolvedSlot, SlotTunables};

        fn floor(max_tokens: Option<u64>) -> SlotTunables {
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
        fn no_slot_uses_floor_directly() {
            let floor = floor(Some(8000));
            let opts = apply_slot_tunables(BuildOpts::new("m"), None, &floor);
            assert_eq!(opts.max_tokens, 8000);
            assert_eq!(opts.temperature, Some(0.5));
            assert_eq!(opts.top_p, Some(0.8));
            assert_eq!(opts.effort.as_deref(), Some("low"));
            assert_eq!(opts.thinking_budget, Some(1000));
            assert_eq!(opts.thinking_style.as_deref(), Some("auto"));
        }

        #[test]
        fn slot_tunables_win_over_floor() {
            let floor = floor(Some(8000));
            let slot = ResolvedSlot {
                role: "coder".into(),
                backend: "anthropic".into(),
                model: "claude-x".into(),
                tunables: SlotTunables {
                    max_tokens: Some(4000),
                    temperature: Some(0.9),
                    top_p: None,
                    effort: None,
                    thinking_budget: None,
                    thinking_style: None,
                },
                loadout: None,
                extra: None,
            };
            let opts = apply_slot_tunables(BuildOpts::new("m"), Some(&slot.tunables), &floor);
            assert_eq!(opts.max_tokens, 4000, "slot's own max_tokens wins over floor");
            assert_eq!(opts.temperature, Some(0.9), "slot's own temperature wins over floor");
            assert_eq!(
                opts.top_p, None,
                "slot leaves top_p None, which is the honest resolved answer \
                 (the ResolvedSlot cascade already merged floor values in — \
                 this helper does not re-cascade)"
            );
        }

        #[test]
        fn unset_max_tokens_leaves_opts_value_untouched() {
            let floor = floor(None); // no configured max_tokens anywhere
            let opts = apply_slot_tunables(BuildOpts::new("m").with_max_tokens(12345), None, &floor);
            assert_eq!(
                opts.max_tokens, 12345,
                "no configured max_tokens anywhere must not reset an already-set value"
            );
        }

        #[test]
        fn set_max_tokens_overrides_existing_opts_value() {
            let floor = floor(Some(999));
            let opts = apply_slot_tunables(BuildOpts::new("m").with_max_tokens(12345), None, &floor);
            assert_eq!(opts.max_tokens, 999, "an explicitly configured max_tokens wins");
        }
    }
}
