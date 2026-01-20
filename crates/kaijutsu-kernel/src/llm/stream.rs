//! Provider-agnostic streaming events for LLM responses.
//!
//! This module provides a unified streaming interface that works across
//! different LLM providers (Anthropic, llama.cpp, Ollama, etc.). Each provider
//! implements [`LlmStream`] to convert their native events into [`StreamEvent`].
//!
//! ```text
//! ┌─────────────────┐   ┌─────────────────┐   ┌─────────────────┐
//! │ Anthropic SDK   │   │ llama.cpp       │   │ Ollama / Other  │
//! │ AnthropicStream │   │ LlamaCppStream  │   │ OllamaStream    │
//! └────────┬────────┘   └────────┬────────┘   └────────┬────────┘
//!          │                     │                     │
//!          ▼                     ▼                     ▼
//!          ┌─────────────────────────────────────────────┐
//!          │          StreamEvent (common enum)          │
//!          │   - All providers produce these events      │
//!          │   - Block handler consumes them             │
//!          └─────────────────────────────────────────────┘
//! ```

use std::future::Future;

use serde::{Deserialize, Serialize};

/// Provider-agnostic streaming events for LLM responses.
///
/// These events are produced by all LLM providers and consumed by
/// the block handler to create/update CRDT blocks in real-time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StreamEvent {
    /// Start of an extended thinking block (reasoning before responding).
    ThinkingStart,

    /// Incremental text delta for the current thinking block.
    ThinkingDelta(String),

    /// End of the current thinking block.
    ThinkingEnd,

    /// Start of a text response block.
    TextStart,

    /// Incremental text delta for the current text block.
    TextDelta(String),

    /// End of the current text block.
    TextEnd,

    /// Tool invocation request (immutable, created all at once).
    ToolUse {
        /// Unique ID for this tool use (for correlation with result).
        id: String,
        /// Tool name (e.g., "cell.edit", "file.read").
        name: String,
        /// Tool input parameters as JSON.
        input: serde_json::Value,
    },

    /// Tool execution result (provided by the system, not the model).
    ToolResult {
        /// ID of the tool_use this is a result for.
        tool_use_id: String,
        /// Result content (typically text or JSON).
        content: String,
        /// Whether this result represents an error.
        is_error: bool,
    },

    /// Generation completed successfully.
    Done {
        /// Reason generation stopped (e.g., "end_turn", "tool_use", "max_tokens").
        stop_reason: Option<String>,
        /// Input tokens consumed.
        input_tokens: Option<u32>,
        /// Output tokens generated.
        output_tokens: Option<u32>,
    },

    /// Error during generation.
    Error(String),
}

impl StreamEvent {
    /// Check if this is a delta event (thinking or text).
    pub fn is_delta(&self) -> bool {
        matches!(self, Self::ThinkingDelta(_) | Self::TextDelta(_))
    }

    /// Check if this is a block start event.
    pub fn is_start(&self) -> bool {
        matches!(self, Self::ThinkingStart | Self::TextStart)
    }

    /// Check if this is a block end event.
    pub fn is_end(&self) -> bool {
        matches!(self, Self::ThinkingEnd | Self::TextEnd)
    }

    /// Check if this is a terminal event (Done or Error).
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done { .. } | Self::Error(_))
    }

    /// Extract delta text if this is a delta event.
    pub fn as_delta(&self) -> Option<&str> {
        match self {
            Self::ThinkingDelta(s) | Self::TextDelta(s) => Some(s),
            _ => None,
        }
    }
}

/// Type of content block currently being streamed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamingBlockType {
    /// Extended thinking block.
    Thinking,
    /// Main text response block.
    Text,
}

/// Trait for LLM providers that support streaming responses.
///
/// Implementations convert provider-specific streaming events into
/// the common [`StreamEvent`] format.
pub trait LlmStream: Send {
    /// Poll for the next streaming event.
    ///
    /// Returns `None` when the stream is exhausted (after `Done` or `Error`).
    fn next_event(&mut self) -> impl Future<Output = Option<StreamEvent>> + Send;

    /// Get the model name being used for this stream.
    fn model(&self) -> &str;
}

/// Builder for constructing streaming requests.
///
/// This provides a provider-agnostic way to configure streaming requests.
#[derive(Debug, Clone)]
pub struct StreamRequest {
    /// The model to use.
    pub model: String,
    /// Conversation messages.
    pub messages: Vec<super::Message>,
    /// System prompt.
    pub system: Option<String>,
    /// Maximum tokens to generate.
    pub max_tokens: u32,
    /// Temperature (0.0 = deterministic, 1.0 = creative).
    pub temperature: Option<f32>,
    /// Whether to enable extended thinking.
    pub thinking_enabled: bool,
    /// Token budget for thinking (if enabled).
    pub thinking_budget: Option<u32>,
    /// Tools available for the model to use.
    pub tools: Option<Vec<super::ToolDefinition>>,
}

impl StreamRequest {
    /// Create a new streaming request.
    pub fn new(model: impl Into<String>, messages: Vec<super::Message>) -> Self {
        Self {
            model: model.into(),
            messages,
            system: None,
            max_tokens: 4096,
            temperature: None,
            thinking_enabled: false,
            thinking_budget: None,
            tools: None,
        }
    }

    /// Set the system prompt.
    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    /// Set max tokens.
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Set temperature.
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// Enable extended thinking with the given token budget.
    pub fn with_thinking(mut self, budget: u32) -> Self {
        self.thinking_enabled = true;
        self.thinking_budget = Some(budget);
        self
    }

    /// Set tools available for the model.
    pub fn with_tools(mut self, tools: Vec<super::ToolDefinition>) -> Self {
        self.tools = Some(tools);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stream_event_is_delta() {
        assert!(StreamEvent::ThinkingDelta("test".into()).is_delta());
        assert!(StreamEvent::TextDelta("test".into()).is_delta());
        assert!(!StreamEvent::ThinkingStart.is_delta());
        assert!(!StreamEvent::TextStart.is_delta());
    }

    #[test]
    fn test_stream_event_is_terminal() {
        assert!(StreamEvent::Done {
            stop_reason: None,
            input_tokens: None,
            output_tokens: None
        }
        .is_terminal());
        assert!(StreamEvent::Error("oops".into()).is_terminal());
        assert!(!StreamEvent::TextStart.is_terminal());
    }

    #[test]
    fn test_stream_request_builder() {
        let request = StreamRequest::new("claude-3-opus", vec![])
            .with_system("Be helpful")
            .with_max_tokens(1000)
            .with_temperature(0.7)
            .with_thinking(2048);

        assert_eq!(request.model, "claude-3-opus");
        assert_eq!(request.system, Some("Be helpful".into()));
        assert_eq!(request.max_tokens, 1000);
        assert_eq!(request.temperature, Some(0.7));
        assert!(request.thinking_enabled);
        assert_eq!(request.thinking_budget, Some(2048));
    }
}
