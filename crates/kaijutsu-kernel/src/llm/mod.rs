//! LLM provider abstraction for kaijutsu kernels.
//!
//! This module provides a unified interface for interacting with various
//! LLM providers (Anthropic, local models, etc.).
//!
//! ## Streaming
//!
//! For real-time streaming responses, use the [`stream`] module which provides
//! a provider-agnostic [`StreamEvent`](stream::StreamEvent) enum and
//! [`LlmStream`](stream::LlmStream) trait.

mod anthropic;
pub mod stream;

// Re-export streaming types for public API
pub use anthropic::{AnthropicProvider, AnthropicStream};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Role of a message in a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Human/user message.
    User,
    /// Assistant/model message.
    Assistant,
}

/// A message in a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Who sent this message.
    pub role: Role,
    /// Message content.
    pub content: String,
}

impl Message {
    /// Create a user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }

    /// Create an assistant message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }
}

/// A block of content in an LLM response.
///
/// This mirrors Claude's API response format to support structured content
/// including extended thinking, tool use, and text blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ResponseBlock {
    /// Model's extended thinking (reasoning before responding).
    Thinking {
        /// The thinking text.
        thinking: String,
        /// Signature for thinking block verification (if provided).
        signature: Option<String>,
    },
    /// Main text response.
    Text {
        /// The text content.
        text: String,
    },
    /// Tool invocation request.
    ToolUse {
        /// Unique ID for this tool use.
        id: String,
        /// Tool name.
        name: String,
        /// Tool input as JSON.
        input: serde_json::Value,
    },
    /// Result from a tool execution.
    ToolResult {
        /// ID of the tool_use this is a result for.
        tool_use_id: String,
        /// Result content.
        content: String,
        /// Whether this result represents an error.
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
    pub input_tokens: u32,
    /// Output tokens generated.
    pub output_tokens: u32,
}

impl Usage {
    /// Total tokens (input + output).
    pub fn total(&self) -> u32 {
        self.input_tokens + self.output_tokens
    }
}

/// Response from an LLM completion request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionResponse {
    /// The generated text content (concatenated from text blocks).
    pub content: String,
    /// Structured content blocks from the response.
    pub blocks: Vec<ResponseBlock>,
    /// Model that generated the response.
    pub model: String,
    /// Reason the generation stopped.
    pub stop_reason: Option<String>,
    /// Token usage statistics.
    pub usage: Usage,
}

impl CompletionResponse {
    /// Get only text blocks from the response.
    pub fn text_blocks(&self) -> impl Iterator<Item = &str> {
        self.blocks.iter().filter_map(|b| b.as_text())
    }

    /// Get thinking blocks from the response.
    pub fn thinking_blocks(&self) -> impl Iterator<Item = &str> {
        self.blocks.iter().filter_map(|b| match b {
            ResponseBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
            _ => None,
        })
    }

    /// Check if the response contains any thinking blocks.
    pub fn has_thinking(&self) -> bool {
        self.blocks.iter().any(|b| b.is_thinking())
    }

    /// Check if the response contains any tool use blocks.
    pub fn has_tool_use(&self) -> bool {
        self.blocks.iter().any(|b| b.is_tool_use())
    }
}

/// Configuration for a completion request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionRequest {
    /// Conversation history.
    pub messages: Vec<Message>,
    /// System prompt (provider-specific handling).
    pub system: Option<String>,
    /// Model identifier.
    pub model: String,
    /// Maximum tokens to generate.
    pub max_tokens: u32,
    /// Temperature (0.0 = deterministic, 1.0 = creative).
    pub temperature: Option<f32>,
}

impl CompletionRequest {
    /// Create a new completion request.
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            messages,
            system: None,
            model: model.into(),
            max_tokens: 4096,
            temperature: None,
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
}

/// Result type for LLM operations.
pub type LlmResult<T> = Result<T, LlmError>;

/// Trait for LLM providers.
///
/// Implementations provide access to different LLM backends
/// (Anthropic Claude, local models via llama.cpp, etc.).
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Get the provider name (e.g., "anthropic", "local").
    fn name(&self) -> &str;

    /// List available models for this provider.
    fn available_models(&self) -> Vec<&str>;

    /// Check if the provider is ready (has credentials, connection, etc.).
    async fn is_available(&self) -> bool;

    /// Send a completion request.
    async fn complete(&self, request: CompletionRequest) -> LlmResult<CompletionResponse>;

    /// Simple prompt helper - sends a single user message.
    async fn prompt(&self, model: &str, prompt: &str) -> LlmResult<String> {
        let request = CompletionRequest::new(model, vec![Message::user(prompt)]);
        let response = self.complete(request).await?;
        Ok(response.content)
    }
}

/// Registry of LLM providers.
#[derive(Default)]
pub struct LlmRegistry {
    providers: std::collections::HashMap<String, Arc<dyn LlmProvider>>,
    default_provider: Option<String>,
}

impl std::fmt::Debug for LlmRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmRegistry")
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .field("default_provider", &self.default_provider)
            .finish()
    }
}

impl LlmRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a provider.
    pub fn register(&mut self, provider: Arc<dyn LlmProvider>) {
        let name = provider.name().to_string();
        self.providers.insert(name, provider);
    }

    /// Get a provider by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn LlmProvider>> {
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

    /// Get the default provider.
    pub fn default_provider(&self) -> Option<Arc<dyn LlmProvider>> {
        self.default_provider
            .as_ref()
            .and_then(|name| self.get(name))
    }

    /// List all registered providers.
    pub fn list(&self) -> Vec<&str> {
        self.providers.keys().map(|s| s.as_str()).collect()
    }

    /// Quick prompt using default provider and model.
    pub async fn prompt(&self, prompt: &str) -> LlmResult<String> {
        let provider = self
            .default_provider()
            .ok_or_else(|| LlmError::Unavailable("no default provider set".into()))?;

        let models = provider.available_models();
        let model = models
            .first()
            .ok_or_else(|| LlmError::Unavailable("provider has no models".into()))?;

        provider.prompt(model, prompt).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_constructors() {
        let user = Message::user("hello");
        assert_eq!(user.role, Role::User);
        assert_eq!(user.content, "hello");

        let assistant = Message::assistant("hi there");
        assert_eq!(assistant.role, Role::Assistant);
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
    fn test_completion_request_builder() {
        let request = CompletionRequest::new("claude-3-opus", vec![Message::user("test")])
            .with_system("You are helpful")
            .with_max_tokens(1000)
            .with_temperature(0.7);

        assert_eq!(request.model, "claude-3-opus");
        assert_eq!(request.system, Some("You are helpful".into()));
        assert_eq!(request.max_tokens, 1000);
        assert_eq!(request.temperature, Some(0.7));
    }
}
