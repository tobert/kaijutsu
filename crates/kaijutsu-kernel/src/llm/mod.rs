//! LLM provider abstraction for kaijutsu kernels.
//!
//! This module provides a unified interface for interacting with various
//! LLM providers via rig-core (Anthropic, Gemini, OpenAI, Ollama).
//!
//! ## Architecture
//!
//! Kaijutsu uses a thin adapter layer over rig-core:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    kaijutsu-kernel                          │
//! │  ┌───────────────┐  ┌───────────────┐  ┌───────────────┐   │
//! │  │ RigProvider   │  │ StreamEvent   │  │ ToolFilter    │   │
//! │  │ (unified API) │  │ (CRDT events) │  │ (per-kernel)  │   │
//! │  └───────┬───────┘  └───────┬───────┘  └───────────────┘   │
//! │          │                  │                               │
//! └──────────┼──────────────────┼───────────────────────────────┘
//!            │                  │
//!            ▼                  ▼
//! ┌──────────────────────────────────────────────────────────────┐
//! │                       rig-core                                │
//! │  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌──────────┐        │
//! │  │Anthropic │ │ Gemini   │ │ OpenAI   │ │ Ollama   │        │
//! │  └──────────┘ └──────────┘ └──────────┘ └──────────┘        │
//! └──────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Streaming
//!
//! For real-time streaming responses, use the [`stream`] module which provides
//! a provider-agnostic [`StreamEvent`](stream::StreamEvent) enum that maps to
//! CRDT block operations.

pub mod config;
pub mod stream;

// Re-export key types
pub use config::{ContextSegment, ProviderConfig, ToolConfig, ToolFilter};
pub use stream::{LlmStream, RigStreamAdapter, StreamEvent, StreamRequest, StreamingBlockType};

use async_trait::async_trait;
use rig::client::{CompletionClient, Nothing};
use rig::completion::{self as rig_completion};
use rig::providers::{anthropic, gemini, ollama, openai};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Default model to use when none specified.
pub const DEFAULT_MODEL: &str = "claude-haiku-4-5-20251001";

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
        let mut blocks = Vec::new();
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

impl From<ToolDefinition> for rig_completion::ToolDefinition {
    fn from(td: ToolDefinition) -> Self {
        Self {
            name: td.name,
            description: td.description,
            parameters: td.input_schema,
        }
    }
}

impl From<rig_completion::ToolDefinition> for ToolDefinition {
    fn from(td: rig_completion::ToolDefinition) -> Self {
        Self {
            name: td.name,
            description: td.description,
            input_schema: td.parameters,
        }
    }
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

impl From<rig_completion::Usage> for Usage {
    fn from(u: rig_completion::Usage) -> Self {
        Self {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
        }
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

    /// Rig completion error.
    #[error("completion error: {0}")]
    CompletionError(String),
}

impl From<rig_completion::CompletionError> for LlmError {
    fn from(e: rig_completion::CompletionError) -> Self {
        match e {
            rig_completion::CompletionError::HttpError(e) => LlmError::NetworkError(e.to_string()),
            rig_completion::CompletionError::JsonError(e) => {
                LlmError::InvalidRequest(e.to_string())
            }
            rig_completion::CompletionError::RequestError(e) => {
                LlmError::InvalidRequest(e.to_string())
            }
            rig_completion::CompletionError::ResponseError(s) => LlmError::ApiError(s),
            rig_completion::CompletionError::ProviderError(s) => LlmError::ApiError(s),
            rig_completion::CompletionError::UrlError(e) => LlmError::InvalidRequest(e.to_string()),
        }
    }
}

/// Result type for LLM operations.
pub type LlmResult<T> = Result<T, LlmError>;

/// Unified provider enum wrapping rig-core providers.
///
/// This enum provides a consistent interface across all supported providers,
/// handling provider-specific quirks internally.
#[derive(Clone)]
pub enum RigProvider {
    /// Anthropic Claude models.
    Anthropic(anthropic::Client),
    /// Google Gemini models.
    Gemini(gemini::Client),
    /// OpenAI models (GPT-4, etc.).
    OpenAI(openai::Client),
    /// Ollama local models.
    Ollama(ollama::Client),
}

impl std::fmt::Debug for RigProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Anthropic(_) => f.debug_tuple("Anthropic").field(&"[client]").finish(),
            Self::Gemini(_) => f.debug_tuple("Gemini").field(&"[client]").finish(),
            Self::OpenAI(_) => f.debug_tuple("OpenAI").field(&"[client]").finish(),
            Self::Ollama(_) => f.debug_tuple("Ollama").field(&"[client]").finish(),
        }
    }
}

impl RigProvider {
    /// Create a provider from configuration.
    pub fn from_config(config: &ProviderConfig) -> LlmResult<Self> {
        match config.provider_type.as_str() {
            "anthropic" => {
                let api_key = config
                    .resolve_api_key()
                    .ok_or_else(|| LlmError::AuthError("No API key for Anthropic".into()))?;
                let client = anthropic::Client::new(&api_key)
                    .map_err(|e| LlmError::Unavailable(e.to_string()))?;
                Ok(Self::Anthropic(client))
            }
            "gemini" => {
                let api_key = config
                    .resolve_api_key()
                    .ok_or_else(|| LlmError::AuthError("No API key for Gemini".into()))?;
                let client = gemini::Client::new(&api_key)
                    .map_err(|e| LlmError::Unavailable(e.to_string()))?;
                Ok(Self::Gemini(client))
            }
            "openai" => {
                let api_key = config
                    .resolve_api_key()
                    .ok_or_else(|| LlmError::AuthError("No API key for OpenAI".into()))?;
                let client = openai::Client::new(&api_key)
                    .map_err(|e| LlmError::Unavailable(e.to_string()))?;
                Ok(Self::OpenAI(client))
            }
            "ollama" => {
                let base_url = config
                    .base_url
                    .clone()
                    .unwrap_or_else(|| "http://localhost:11434".into());
                let client = ollama::Client::builder()
                    .api_key(Nothing)
                    .base_url(&base_url)
                    .build()
                    .map_err(|e| LlmError::Unavailable(e.to_string()))?;
                Ok(Self::Ollama(client))
            }
            other => Err(LlmError::Unavailable(format!(
                "Unknown provider type: {}",
                other
            ))),
        }
    }

    /// Create an Anthropic provider from environment.
    pub fn anthropic_from_env() -> LlmResult<Self> {
        let config = ProviderConfig::new("anthropic").with_api_key_env("ANTHROPIC_API_KEY");
        Self::from_config(&config)
    }

    /// Create a Gemini provider from environment.
    pub fn gemini_from_env() -> LlmResult<Self> {
        let config = ProviderConfig::new("gemini").with_api_key_env("GEMINI_API_KEY");
        Self::from_config(&config)
    }

    /// Create an OpenAI provider from environment.
    pub fn openai_from_env() -> LlmResult<Self> {
        let config = ProviderConfig::new("openai").with_api_key_env("OPENAI_API_KEY");
        Self::from_config(&config)
    }

    /// Create an Ollama provider (local, no auth needed).
    pub fn ollama_local() -> LlmResult<Self> {
        let config = ProviderConfig::new("ollama");
        Self::from_config(&config)
    }

    /// Get the provider name.
    pub fn name(&self) -> &str {
        match self {
            Self::Anthropic(_) => "anthropic",
            Self::Gemini(_) => "gemini",
            Self::OpenAI(_) => "openai",
            Self::Ollama(_) => "ollama",
        }
    }

    /// Simple prompt helper - sends a single user message.
    pub async fn prompt(&self, model: &str, prompt: &str) -> LlmResult<String> {
        self.prompt_with_system(model, None, prompt).await
    }

    /// Prompt with an optional system preamble.
    pub async fn prompt_with_system(
        &self,
        model: &str,
        system: Option<&str>,
        prompt: &str,
    ) -> LlmResult<String> {
        use rig::completion::{AssistantContent, CompletionModel};
        use rig::message::Message as RigMessage;

        let message = RigMessage::user(prompt);
        let request = rig_completion::CompletionRequest {
            preamble: system.map(|s| s.to_string()),
            chat_history: rig::OneOrMany::one(message),
            tools: vec![],
            temperature: None,
            max_tokens: None,
            additional_params: None,
            tool_choice: None,
            documents: vec![],
        };

        // Helper to extract text from OneOrMany<AssistantContent>
        fn extract_text(choice: rig::OneOrMany<AssistantContent>) -> String {
            choice
                .iter()
                .filter_map(|content| match content {
                    AssistantContent::Text(text) => Some(text.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        }

        let response_text = match self {
            Self::Anthropic(client) => {
                let model = client.completion_model(model);
                let response = model.completion(request).await?;
                extract_text(response.choice)
            }
            Self::Gemini(client) => {
                let model = client.completion_model(model);
                let response = model.completion(request).await?;
                extract_text(response.choice)
            }
            Self::OpenAI(client) => {
                let model = client.completion_model(model);
                let response = model.completion(request).await?;
                extract_text(response.choice)
            }
            Self::Ollama(client) => {
                let model = client.completion_model(model);
                let response = model.completion(request).await?;
                extract_text(response.choice)
            }
        };

        Ok(response_text)
    }

    /// Create a streaming request.
    ///
    /// Returns a [`RigStreamAdapter`] that converts rig's streaming events
    /// into provider-agnostic [`StreamEvent`]s.
    pub async fn stream(&self, request: StreamRequest) -> LlmResult<RigStreamAdapter> {
        RigStreamAdapter::new(self.clone(), request).await
    }
}

/// Trait for LLM providers (kept for compatibility).
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Get the provider name (e.g., "anthropic", "gemini").
    fn name(&self) -> &str;

    /// List available models for this provider.
    fn available_models(&self) -> Vec<&str>;

    /// Check if the provider is ready (has credentials, connection, etc.).
    async fn is_available(&self) -> bool;

    /// Simple prompt helper - sends a single user message.
    async fn prompt(&self, model: &str, prompt: &str) -> LlmResult<String>;
}

#[async_trait]
impl LlmProvider for RigProvider {
    fn name(&self) -> &str {
        self.name()
    }

    fn available_models(&self) -> Vec<&str> {
        match self {
            Self::Anthropic(_) => vec![
                anthropic::completion::CLAUDE_4_OPUS,
                anthropic::completion::CLAUDE_4_SONNET,
                anthropic::completion::CLAUDE_3_5_SONNET,
                anthropic::completion::CLAUDE_3_5_HAIKU,
            ],
            Self::Gemini(_) => vec!["gemini-2.0-flash", "gemini-2.0-pro", "gemini-1.5-pro"],
            Self::OpenAI(_) => vec!["gpt-4o", "gpt-4-turbo", "gpt-3.5-turbo"],
            Self::Ollama(_) => vec!["qwen2.5-coder:7b", "llama3.2", "codellama"],
        }
    }

    async fn is_available(&self) -> bool {
        // Could do a lightweight API check here
        true
    }

    async fn prompt(&self, model: &str, prompt: &str) -> LlmResult<String> {
        self.prompt(model, prompt).await
    }
}

/// Registry of LLM providers.
#[derive(Default)]
pub struct LlmRegistry {
    providers: std::collections::HashMap<String, Arc<dyn LlmProvider>>,
    default_provider: Option<String>,
    default_model: Option<String>,
}

impl std::fmt::Debug for LlmRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmRegistry")
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .field("default_provider", &self.default_provider)
            .field("default_model", &self.default_model)
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

    /// Set the default model.
    pub fn set_default_model(&mut self, model: impl Into<String>) {
        self.default_model = Some(model.into());
    }

    /// Get the default provider.
    pub fn default_provider(&self) -> Option<Arc<dyn LlmProvider>> {
        self.default_provider
            .as_ref()
            .and_then(|name| self.get(name))
    }

    /// Get the default model.
    pub fn default_model(&self) -> Option<&str> {
        self.default_model.as_deref()
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

        let model = self
            .default_model
            .as_deref()
            .or_else(|| provider.available_models().first().copied())
            .ok_or_else(|| LlmError::Unavailable("no default model set".into()))?;

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
    fn test_usage() {
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 50,
        };
        assert_eq!(usage.total(), 150);
    }

    #[test]
    fn test_tool_definition_conversion() {
        let td = ToolDefinition {
            name: "test_tool".into(),
            description: "A test tool".into(),
            input_schema: serde_json::json!({"type": "object"}),
        };

        let rig_td: rig_completion::ToolDefinition = td.clone().into();
        assert_eq!(rig_td.name, "test_tool");
        assert_eq!(rig_td.description, "A test tool");

        let back: ToolDefinition = rig_td.into();
        assert_eq!(back.name, td.name);
    }

    #[test]
    fn test_provider_names() {
        // Just verify the names are correct
        assert_eq!(
            RigProvider::Anthropic(anthropic::Client::new("fake").unwrap()).name(),
            "anthropic"
        );
    }
}
