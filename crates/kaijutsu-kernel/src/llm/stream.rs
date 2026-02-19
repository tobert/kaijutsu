//! Provider-agnostic streaming events for LLM responses.
//!
//! This module provides a unified streaming interface that converts rig-core's
//! provider-specific streaming events into kaijutsu's [`StreamEvent`] enum,
//! which maps directly to CRDT block operations.
//!
//! ```text
//! ┌─────────────────┐   ┌─────────────────┐   ┌─────────────────┐
//! │ rig Anthropic   │   │ rig Gemini      │   │ rig Ollama      │
//! │ streaming       │   │ streaming       │   │ streaming       │
//! └────────┬────────┘   └────────┬────────┘   └────────┬────────┘
//!          │                     │                     │
//!          ▼                     ▼                     ▼
//!          ┌─────────────────────────────────────────────┐
//!          │       RigStreamAdapter                      │
//!          │  - Converts RawStreamingChoice → StreamEvent │
//!          │  - Handles provider differences internally  │
//!          └─────────────────────────────────────────────┘
//!                              │
//!                              ▼
//!          ┌─────────────────────────────────────────────┐
//!          │          StreamEvent (common enum)          │
//!          │   - Block handler consumes these events    │
//!          │   - Maps to CRDT operations                │
//!          └─────────────────────────────────────────────┘
//! ```

use std::future::Future;

use futures::StreamExt;
use rig::client::CompletionClient;
use rig::completion::{CompletionModel as RigCompletionModel, CompletionRequest};
use rig::providers::{anthropic, gemini, ollama, openai};
use rig::streaming::StreamingCompletionResponse as RigStreamingResponse;
use serde::{Deserialize, Serialize};

use super::{LlmResult, Message, RigProvider, ToolDefinition};

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
        input_tokens: Option<u64>,
        /// Output tokens generated.
        output_tokens: Option<u64>,
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
    pub messages: Vec<Message>,
    /// System prompt.
    pub system: Option<String>,
    /// Maximum tokens to generate.
    pub max_tokens: u64,
    /// Temperature (0.0 = deterministic, 1.0 = creative).
    pub temperature: Option<f64>,
    /// Whether to enable extended thinking.
    pub thinking_enabled: bool,
    /// Token budget for thinking (if enabled).
    pub thinking_budget: Option<u64>,
    /// Tools available for the model to use.
    pub tools: Option<Vec<ToolDefinition>>,
}

impl StreamRequest {
    /// Create a new streaming request.
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            messages,
            system: None,
            max_tokens: 64000,
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
    pub fn with_max_tokens(mut self, max_tokens: u64) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Set temperature.
    pub fn with_temperature(mut self, temperature: f64) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// Enable extended thinking with the given token budget.
    pub fn with_thinking(mut self, budget: u64) -> Self {
        self.thinking_enabled = true;
        self.thinking_budget = Some(budget);
        self
    }

    /// Set tools available for the model.
    pub fn with_tools(mut self, tools: Vec<ToolDefinition>) -> Self {
        self.tools = Some(tools);
        self
    }

    /// Convert to rig's CompletionRequest format.
    fn to_rig_request(&self) -> CompletionRequest {
        use rig::message::{AssistantContent, Message as RigMessage, UserContent};

        let mut chat_history = Vec::new();
        for msg in &self.messages {
            match msg.role {
                super::Role::User => {
                    let content = match &msg.content {
                        super::MessageContent::Text(t) => {
                            rig::OneOrMany::one(UserContent::text(t.clone()))
                        }
                        super::MessageContent::Blocks(blocks) => {
                            let user_content: Vec<UserContent> = blocks
                                .iter()
                                .filter_map(|b| match b {
                                    super::ContentBlock::Text { text } => {
                                        Some(UserContent::text(text.clone()))
                                    }
                                    super::ContentBlock::ToolResult {
                                        tool_use_id,
                                        content,
                                        is_error: _,
                                    } => Some(UserContent::tool_result(
                                        tool_use_id.clone(),
                                        rig::OneOrMany::one(
                                            rig::message::ToolResultContent::text(content.clone()),
                                        ),
                                    )),
                                    _ => None,
                                })
                                .collect();
                            rig::OneOrMany::many(user_content)
                                .unwrap_or_else(|_| rig::OneOrMany::one(UserContent::text("")))
                        }
                    };
                    chat_history.push(RigMessage::User { content });
                }
                super::Role::Assistant => {
                    let content = match &msg.content {
                        super::MessageContent::Text(t) => {
                            rig::OneOrMany::one(AssistantContent::text(t.clone()))
                        }
                        super::MessageContent::Blocks(blocks) => {
                            let assistant_content: Vec<AssistantContent> = blocks
                                .iter()
                                .filter_map(|b| match b {
                                    super::ContentBlock::Text { text } => {
                                        Some(AssistantContent::text(text.clone()))
                                    }
                                    super::ContentBlock::ToolUse { id, name, input } => {
                                        Some(AssistantContent::tool_call(
                                            id.clone(),
                                            name.clone(),
                                            input.clone(),
                                        ))
                                    }
                                    _ => None,
                                })
                                .collect();
                            rig::OneOrMany::many(assistant_content)
                                .unwrap_or_else(|_| rig::OneOrMany::one(AssistantContent::text("")))
                        }
                    };
                    chat_history.push(RigMessage::Assistant { id: None, content });
                }
            }
        }

        // Convert Vec<Message> to OneOrMany<Message>
        let chat_history = rig::OneOrMany::many(chat_history)
            .unwrap_or_else(|_| rig::OneOrMany::one(RigMessage::User {
                content: rig::OneOrMany::one(UserContent::text("")),
            }));

        let mut req = CompletionRequest {
            preamble: self.system.clone(),
            chat_history,
            tools: self
                .tools
                .as_ref()
                .map(|ts| ts.iter().cloned().map(Into::into).collect())
                .unwrap_or_default(),
            temperature: self.temperature,
            max_tokens: Some(self.max_tokens),
            additional_params: None,
            tool_choice: None,
            documents: vec![],
        };

        // Add thinking params if enabled (Anthropic-specific)
        if self.thinking_enabled {
            if let Some(budget) = self.thinking_budget {
                req.additional_params = Some(serde_json::json!({
                    "thinking": {
                        "type": "enabled",
                        "budget_tokens": budget
                    }
                }));
            }
        }

        req
    }
}

/// Adapter that converts rig's streaming responses into kaijutsu [`StreamEvent`]s.
///
/// This is a wrapper around rig's `StreamingCompletionResponse` that handles
/// provider-specific differences and emits a unified event stream.
///
/// ## State Machine
///
/// The adapter tracks the current block type (Text or Thinking) and ensures
/// proper End events are emitted:
/// - When switching from one block type to another (e.g., Thinking → Text)
/// - When the stream finishes (before Done/Error)
///
/// This is critical for CRDT systems that need well-formed block boundaries.
pub struct RigStreamAdapter {
    model: String,
    provider_kind: ProviderKind,
    inner: RigStreamInner,
    /// Current block type being streamed.
    current_block: Option<StreamingBlockType>,
    /// Whether we've emitted a start event for text.
    text_started: bool,
    /// Whether we've emitted a start event for thinking.
    thinking_started: bool,
    /// Whether the stream has finished.
    finished: bool,
    /// Accumulated usage stats.
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    /// Pending event to emit on next call (for two-event sequences like End+Start).
    pending_event: Option<StreamEvent>,
}

/// Provider kind for internal dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderKind {
    Anthropic,
    Gemini,
    OpenAI,
    Ollama,
}

/// Inner stream type - each provider has its own response type.
enum RigStreamInner {
    Anthropic(RigStreamingResponse<anthropic::streaming::StreamingCompletionResponse>),
    Gemini(RigStreamingResponse<gemini::streaming::StreamingCompletionResponse>),
    OpenAI(RigStreamingResponse<openai::responses_api::streaming::StreamingCompletionResponse>),
    Ollama(RigStreamingResponse<ollama::StreamingCompletionResponse>),
}

impl RigStreamAdapter {
    /// Create a new streaming adapter from a provider and request.
    pub async fn new(provider: RigProvider, request: StreamRequest) -> LlmResult<Self> {
        let model = request.model.clone();
        let rig_request = request.to_rig_request();

        let (provider_kind, inner) = match provider {
            RigProvider::Anthropic(client) => {
                let completion_model = client.completion_model(&model);
                let stream = completion_model.stream(rig_request).await?;
                (ProviderKind::Anthropic, RigStreamInner::Anthropic(stream))
            }
            RigProvider::Gemini(client) => {
                let completion_model = client.completion_model(&model);
                let stream = completion_model.stream(rig_request).await?;
                (ProviderKind::Gemini, RigStreamInner::Gemini(stream))
            }
            RigProvider::OpenAI(client) => {
                let completion_model = client.completion_model(&model);
                let stream = completion_model.stream(rig_request).await?;
                (ProviderKind::OpenAI, RigStreamInner::OpenAI(stream))
            }
            RigProvider::Ollama(client) => {
                let completion_model = client.completion_model(&model);
                let stream = completion_model.stream(rig_request).await?;
                (ProviderKind::Ollama, RigStreamInner::Ollama(stream))
            }
        };

        Ok(Self {
            model,
            provider_kind,
            inner,
            current_block: None,
            text_started: false,
            thinking_started: false,
            finished: false,
            input_tokens: None,
            output_tokens: None,
            pending_event: None,
        })
    }

}

impl RigStreamAdapter {
    /// Close the current block and return an End event, queueing the next event.
    fn close_current_block(&mut self, next_event: StreamEvent) -> StreamEvent {
        let end_event = match self.current_block.take() {
            Some(StreamingBlockType::Text) => StreamEvent::TextEnd,
            Some(StreamingBlockType::Thinking) => StreamEvent::ThinkingEnd,
            None => {
                // No block to close, just return the next event directly
                return next_event;
            }
        };
        // Queue the next event and return the end event
        self.pending_event = Some(next_event);
        end_event
    }

    /// Create the Done event with current token counts.
    fn make_done_event(&self) -> StreamEvent {
        StreamEvent::Done {
            stop_reason: Some("end_turn".into()),
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
        }
    }
}

impl LlmStream for RigStreamAdapter {
    async fn next_event(&mut self) -> Option<StreamEvent> {
        use rig::streaming::StreamedAssistantContent;

        // First, drain any pending event from a previous transition
        if let Some(event) = self.pending_event.take() {
            return Some(event);
        }

        if self.finished {
            return None;
        }

        loop {
            // Each provider has a different concrete type. We use a macro to avoid
            // code duplication while handling each provider's stream type.
            macro_rules! process_stream {
                ($stream:expr) => {{
                    match $stream.next().await {
                        Some(Ok(content)) => {
                            // Pattern match on the StreamedAssistantContent enum
                            match content {
                                StreamedAssistantContent::Text(text) => {
                                    if !text.text.is_empty() {
                                        // Check for block transition: was Thinking, now Text
                                        if self.current_block == Some(StreamingBlockType::Thinking) {
                                            self.text_started = true;
                                            self.current_block = Some(StreamingBlockType::Text);
                                            // Queue: TextStart, then TextDelta
                                            self.pending_event = Some(StreamEvent::TextDelta(text.text));
                                            return Some(self.close_current_block(StreamEvent::TextStart));
                                        }

                                        // Normal case: starting or continuing text
                                        if !self.text_started {
                                            self.text_started = true;
                                            self.current_block = Some(StreamingBlockType::Text);
                                            // Queue the delta, emit start first
                                            self.pending_event = Some(StreamEvent::TextDelta(text.text));
                                            return Some(StreamEvent::TextStart);
                                        }
                                        return Some(StreamEvent::TextDelta(text.text));
                                    }
                                }
                                StreamedAssistantContent::ToolCall { tool_call, .. } => {
                                    let tool_event = StreamEvent::ToolUse {
                                        id: tool_call.id.clone(),
                                        name: tool_call.function.name.clone(),
                                        input: tool_call.function.arguments.clone(),
                                    };
                                    // Close any open block before tool use
                                    if self.current_block.is_some() {
                                        return Some(self.close_current_block(tool_event));
                                    }
                                    return Some(tool_event);
                                }
                                StreamedAssistantContent::ToolCallDelta { .. } => {
                                    // Tool call deltas are partial updates, we handle complete tool calls
                                }
                                StreamedAssistantContent::Reasoning(reasoning) => {
                                    if !reasoning.reasoning.is_empty() {
                                        let text = reasoning.reasoning.join("");
                                        if !text.is_empty() {
                                            // Check for block transition: was Text, now Thinking
                                            if self.current_block == Some(StreamingBlockType::Text) {
                                                self.thinking_started = true;
                                                self.current_block = Some(StreamingBlockType::Thinking);
                                                self.pending_event = Some(StreamEvent::ThinkingDelta(text));
                                                return Some(self.close_current_block(StreamEvent::ThinkingStart));
                                            }

                                            if !self.thinking_started {
                                                self.thinking_started = true;
                                                self.current_block = Some(StreamingBlockType::Thinking);
                                                self.pending_event = Some(StreamEvent::ThinkingDelta(text));
                                                return Some(StreamEvent::ThinkingStart);
                                            }
                                            return Some(StreamEvent::ThinkingDelta(text));
                                        }
                                    }
                                }
                                StreamedAssistantContent::ReasoningDelta { reasoning, .. } => {
                                    if !reasoning.is_empty() {
                                        // Check for block transition: was Text, now Thinking
                                        if self.current_block == Some(StreamingBlockType::Text) {
                                            self.thinking_started = true;
                                            self.current_block = Some(StreamingBlockType::Thinking);
                                            self.pending_event = Some(StreamEvent::ThinkingDelta(reasoning));
                                            return Some(self.close_current_block(StreamEvent::ThinkingStart));
                                        }

                                        if !self.thinking_started {
                                            self.thinking_started = true;
                                            self.current_block = Some(StreamingBlockType::Thinking);
                                            self.pending_event = Some(StreamEvent::ThinkingDelta(reasoning));
                                            return Some(StreamEvent::ThinkingStart);
                                        }
                                        return Some(StreamEvent::ThinkingDelta(reasoning));
                                    }
                                }
                                StreamedAssistantContent::Final(_) => {
                                    // Final response - close any open block, then Done
                                    self.finished = true;
                                    let done = self.make_done_event();
                                    if self.current_block.is_some() {
                                        return Some(self.close_current_block(done));
                                    }
                                    return Some(done);
                                }
                            }
                            // No event to emit for this chunk, continue
                            continue;
                        }
                        Some(Err(e)) => {
                            // Close any open block before error
                            self.finished = true;
                            let error = StreamEvent::Error(e.to_string());
                            if self.current_block.is_some() {
                                return Some(self.close_current_block(error));
                            }
                            return Some(error);
                        }
                        None => {
                            // Stream ended - close any open block, then Done
                            self.finished = true;
                            let done = self.make_done_event();
                            if self.current_block.is_some() {
                                return Some(self.close_current_block(done));
                            }
                            return Some(done);
                        }
                    }
                }};
            }

            match &mut self.inner {
                RigStreamInner::Anthropic(stream) => process_stream!(stream),
                RigStreamInner::Gemini(stream) => process_stream!(stream),
                RigStreamInner::OpenAI(stream) => process_stream!(stream),
                RigStreamInner::Ollama(stream) => process_stream!(stream),
            }
        }
    }

    fn model(&self) -> &str {
        &self.model
    }
}

impl std::fmt::Debug for RigStreamAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RigStreamAdapter")
            .field("model", &self.model)
            .field("provider_kind", &self.provider_kind)
            .field("current_block", &self.current_block)
            .field("finished", &self.finished)
            .finish()
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
        let request = StreamRequest::new("claude-sonnet-4", vec![])
            .with_system("Be helpful")
            .with_max_tokens(1000)
            .with_temperature(0.7)
            .with_thinking(2048);

        assert_eq!(request.model, "claude-sonnet-4");
        assert_eq!(request.system, Some("Be helpful".into()));
        assert_eq!(request.max_tokens, 1000);
        assert_eq!(request.temperature, Some(0.7));
        assert!(request.thinking_enabled);
        assert_eq!(request.thinking_budget, Some(2048));
    }
}
