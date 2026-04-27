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
pub mod toml_config;
pub mod stream;

// Re-export key types
pub use config::ProviderConfig;
pub use toml_config::{
    EmbeddingModelConfig, LlmConfig, ModelAlias, ModelsConfig, initialize_llm_registry,
    load_llm_config_toml, load_models_config_toml,
};
pub use stream::{LlmStream, RigStreamAdapter, StreamEvent, StreamRequest, StreamingBlockType};

use rig::client::{CompletionClient, Nothing};
use rig::completion::{self as rig_completion};
use rig::providers::{anthropic, gemini, ollama, openai};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// Default model to use when none specified.
pub const DEFAULT_MODEL: &str = "claude-haiku-4-5-20251001";

/// Mock LLM client for testing — returns a canned response.
#[cfg(any(test, feature = "test-mock"))]
#[derive(Clone, Debug)]
pub struct MockClient {
    pub canned_response: String,
}

#[cfg(any(test, feature = "test-mock"))]
impl MockClient {
    pub fn new(response: impl Into<String>) -> Self {
        Self {
            canned_response: response.into(),
        }
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
    /// the request hits the LLM provider. Conversion to rig falls back to a
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
        Self::with_reasoning_text_and_tool_uses(None, text, tool_uses)
    }

    /// Create an assistant message with optional reasoning, text, and tool
    /// uses. Used by the agentic-loop driver to preserve thinking across
    /// tool-use iterations (A3). Reasoning blocks are emitted *before* text
    /// and tool uses so providers see the reasoning chain in order.
    pub fn with_reasoning_text_and_tool_uses(
        reasoning: Option<(String, Option<String>)>,
        text: Option<String>,
        tool_uses: Vec<ContentBlock>,
    ) -> Self {
        let mut blocks = Vec::new();
        if let Some((reasoning_text, signature)) = reasoning
            && !reasoning_text.is_empty()
        {
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
    /// Mock provider for testing — returns canned responses.
    #[cfg(any(test, feature = "test-mock"))]
    Mock(MockClient),
}

impl std::fmt::Debug for RigProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Anthropic(_) => f.debug_tuple("Anthropic").field(&"[client]").finish(),
            Self::Gemini(_) => f.debug_tuple("Gemini").field(&"[client]").finish(),
            Self::OpenAI(_) => f.debug_tuple("OpenAI").field(&"[client]").finish(),
            Self::Ollama(_) => f.debug_tuple("Ollama").field(&"[client]").finish(),
            #[cfg(any(test, feature = "test-mock"))]
            Self::Mock(_) => f.debug_tuple("Mock").field(&"[canned]").finish(),
        }
    }
}

impl RigProvider {
    /// Create a provider from configuration.
    // TODO(dedup): provider type strings "anthropic"/"gemini"/"openai" hardcoded here,
    // in config.rs, toml_config.rs — consider constants or an enum
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
            #[cfg(any(test, feature = "test-mock"))]
            "mock" => {
                let model = config
                    .default_model
                    .clone()
                    .unwrap_or_else(|| "mock-model".into());
                Ok(Self::Mock(MockClient::new(format!(
                    "Mock summary for testing (model: {model})."
                ))))
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
            #[cfg(any(test, feature = "test-mock"))]
            Self::Mock(_) => "mock",
        }
    }

    /// Simple prompt helper - sends a single user message.
    #[tracing::instrument(skip(self, prompt), fields(llm.model = %model, llm.provider = self.name()))]
    pub async fn prompt(&self, model: &str, prompt: &str) -> LlmResult<String> {
        self.prompt_with_system(model, None, prompt).await
    }

    /// Prompt with an optional system preamble.
    #[tracing::instrument(skip(self, system, prompt), fields(llm.model = %model, llm.provider = self.name()))]
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
            model: None,
            output_schema: None,
        };

        // Helper to extract text from OneOrMany<AssistantContent>
        fn extract_text(choice: rig::OneOrMany<AssistantContent>) -> String {
            let mut texts = Vec::new();
            for content in choice.iter() {
                match content {
                    AssistantContent::Text(text) => texts.push(text.text.clone()),
                    other => {
                        tracing::warn!(
                            kind = std::any::type_name_of_val(other),
                            "unexpected non-text content in prompt response (dropped)"
                        );
                    }
                }
            }
            texts.join("")
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
            #[cfg(any(test, feature = "test-mock"))]
            Self::Mock(mock) => mock.canned_response.clone(),
        };

        Ok(response_text)
    }

    /// Create a streaming request.
    ///
    /// Returns a [`RigStreamAdapter`] that converts rig's streaming events
    /// into provider-agnostic [`StreamEvent`]s.
    #[tracing::instrument(skip(self, request), fields(llm.provider = self.name()))]
    pub async fn stream(&self, request: StreamRequest) -> LlmResult<RigStreamAdapter> {
        RigStreamAdapter::new(self.clone(), request).await
    }
}

impl RigProvider {
    /// List available models for this provider.
    pub fn available_models(&self) -> Vec<&str> {
        match self {
            Self::Anthropic(_) => vec![
                anthropic::completion::CLAUDE_4_OPUS,
                anthropic::completion::CLAUDE_4_SONNET,
                anthropic::completion::CLAUDE_3_5_SONNET,
                anthropic::completion::CLAUDE_3_5_HAIKU,
            ],
            Self::Gemini(_) => vec!["gemini-2.0-flash", "gemini-2.0-pro", "gemini-1.5-pro"],
            Self::OpenAI(_) => vec!["gpt-4o", "gpt-4-turbo", "gpt-3.5-turbo"],
            Self::Ollama(_) => vec!["qwen3.5:9b-bf16", "qwen3.5:35b-a3b"],
            #[cfg(any(test, feature = "test-mock"))]
            Self::Mock(_) => vec!["mock-model"],
        }
    }
}

/// Registry of LLM providers.
#[derive(Default)]
pub struct LlmRegistry {
    providers: HashMap<String, Arc<RigProvider>>,
    default_provider: Option<String>,
    default_model: Option<String>,
    model_aliases: HashMap<String, toml_config::ModelAlias>,
    provider_configs: Option<Vec<ProviderConfig>>,
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
            .finish()
    }
}

impl LlmRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a provider by name.
    pub fn register(&mut self, name: impl Into<String>, provider: Arc<RigProvider>) {
        self.providers.insert(name.into(), provider);
    }

    /// Get a provider by name.
    pub fn get(&self, name: &str) -> Option<Arc<RigProvider>> {
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
    pub fn default_provider(&self) -> Option<Arc<RigProvider>> {
        self.default_provider
            .as_ref()
            .and_then(|name| self.get(name))
    }

    /// Get the default model.
    pub fn default_model(&self) -> Option<&str> {
        self.default_model.as_deref()
    }

    /// Get max_output_tokens for the default provider, falling back to 64000.
    ///
    /// Set generously — the API enforces per-model ceilings.
    pub fn max_output_tokens(&self) -> u64 {
        self.provider_configs
            .as_ref()
            .and_then(|configs| {
                let default = self.default_provider.as_deref()?;
                configs.iter().find(|c| c.provider_type == default)
            })
            .and_then(|c| c.max_output_tokens)
            .unwrap_or(64000)
    }

    /// Get a provider's config by name.
    pub fn provider_config(&self, name: &str) -> Option<&ProviderConfig> {
        self.provider_configs
            .as_ref()
            .and_then(|configs| configs.iter().find(|c| c.provider_type == name))
    }

    /// Store provider configs for runtime queries (e.g. max_output_tokens).
    pub fn set_provider_configs(&mut self, configs: Vec<ProviderConfig>) {
        self.provider_configs = Some(configs);
    }

    /// Set model aliases.
    pub fn set_model_aliases(&mut self, aliases: HashMap<String, toml_config::ModelAlias>) {
        self.model_aliases = aliases;
    }

    /// Resolve a model name through aliases.
    ///
    /// If the name matches an alias, returns the (provider, model) tuple.
    /// Otherwise returns None, meaning the name should be used as-is.
    pub fn resolve_alias(&self, name: &str) -> Option<(&str, &str)> {
        self.model_aliases
            .get(name)
            .map(|a| (a.provider.as_str(), a.model.as_str()))
    }

    /// Resolve a model name, returning the provider and model to use.
    ///
    /// Checks aliases first. If no alias matches, uses the default provider
    /// with the given model name.
    pub fn resolve_model(&self, model_name: &str) -> Option<(Arc<RigProvider>, String)> {
        if let Some((provider_name, model)) = self.resolve_alias(model_name) {
            self.get(provider_name).map(|p| (p, model.to_string()))
        } else {
            self.default_provider().map(|p| (p, model_name.to_string()))
        }
    }

    /// List all registered providers.
    pub fn list(&self) -> Vec<&str> {
        self.providers.keys().map(|s| s.as_str()).collect()
    }

    /// List all available model IDs for a provider (from aliases + default).
    pub fn models_for_provider(&self, provider_name: &str) -> Vec<String> {
        let mut models: Vec<String> = self
            .model_aliases
            .values()
            .filter(|a| a.provider == provider_name)
            .map(|a| a.model.clone())
            .collect();
        // Include provider's default model if not already listed
        if let Some(configs) = &self.provider_configs {
            for config in configs {
                if config.provider_type == provider_name
                    && let Some(ref default) = config.default_model
                    && !models.contains(default)
                {
                    models.push(default.clone());
                }
            }
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
/// rig-conversion falls back to a text marker so the model knows an image
/// existed at that turn.
pub fn resolve_image_blocks_from_cas(
    messages: &mut [Message],
    cas: &dyn kaijutsu_cas::ContentStore,
) {
    use base64::Engine;
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
            // Prefer CAS-recorded mime over the hydrator's defaulted one;
            // CAS sidecar metadata reflects what was actually stored.
            if let Ok(Some(reference)) = cas.inspect(&parsed) {
                *media_type = reference.mime_type;
            }
            match cas.retrieve(&parsed) {
                Ok(Some(bytes)) => {
                    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    *data_base64 = Some(encoded);
                }
                _ => {
                    // Stay unresolved — rig-conversion emits a text marker.
                }
            }
        }
    }
}

/// Reconstruct LLM conversation history from stored blocks.
///
/// Walks blocks in order and produces the `Message` sequence expected by the
/// LLM API. Skips thinking, file, compacted, and empty blocks.
/// Drift blocks are included as User messages with a provenance prefix.
///
/// Preserves `tool_use_id` from blocks when available, falling back to
/// `BlockId::to_key()` for pre-migration blocks.
///
/// **Trailing-tool-use guard:** If the last message is an assistant with
/// tool_uses but no following tool_results, synthesizes error results so the
/// LLM API doesn't reject the request.
pub fn hydrate_from_blocks(blocks: &[kaijutsu_types::BlockSnapshot]) -> Vec<Message> {
    use kaijutsu_types::{BlockKind, Role as BlockRole};

    struct HydrationState {
        messages: Vec<Message>,
        assistant_text: Option<String>,
        tool_uses: Vec<ContentBlock>,
        tool_results: Vec<ContentBlock>,
        /// Pending user-initiated shell commands, keyed by ToolCall BlockId.
        /// Matched to ToolResults via `tool_call_id` for correct pairing
        /// even when blocks interleave with model tool calls.
        user_shell_pending: std::collections::HashMap<kaijutsu_types::BlockId, String>,
    }

    impl HydrationState {
        fn new() -> Self {
            Self {
                messages: Vec::new(),
                assistant_text: None,
                tool_uses: Vec::new(),
                tool_results: Vec::new(),
                user_shell_pending: std::collections::HashMap::new(),
            }
        }

        /// Flush any pending assistant text + tool_uses into a message.
        fn flush_assistant(&mut self) {
            if self.assistant_text.is_none() && self.tool_uses.is_empty() {
                return;
            }
            if self.tool_uses.is_empty() {
                // Plain text assistant message
                if let Some(text) = self.assistant_text.take() {
                    self.messages.push(Message::assistant(text));
                }
            } else {
                // Assistant message with tool uses (optionally preceded by text)
                let text = self.assistant_text.take();
                let tool_uses = std::mem::take(&mut self.tool_uses);
                self.messages.push(Message::with_tool_uses(text, tool_uses));
            }
        }

        /// Flush any pending tool results into a user message.
        fn flush_tool_results(&mut self) {
            if self.tool_results.is_empty() {
                return;
            }
            let results = std::mem::take(&mut self.tool_results);
            self.messages.push(Message::tool_results(results));
        }

        /// Flush everything pending (assistant then tool results).
        fn flush_all(&mut self) {
            self.flush_assistant();
            self.flush_tool_results();
        }

        /// Consume and return final messages, repairing tool_use/tool_result pairing.
        ///
        /// The LLM API requires that every assistant message containing
        /// `tool_use` blocks is immediately followed by a user message with
        /// matching `tool_result` blocks for **each** tool_use id, and
        /// conversely that tool_result blocks only appear after an assistant
        /// message containing the matching tool_use.
        ///
        /// Forks, interrupts, and out-of-order tool execution can break both
        /// directions:
        /// - **Orphaned tool_uses**: synthesize `is_error: true` results.
        /// - **Late tool_results**: drop results whose tool_use already has
        ///   a (synthetic or real) result earlier in the conversation.
        fn into_messages(mut self) -> Vec<Message> {
            self.flush_all();

            // ── Pass 1: Forward repair (orphaned tool_uses → synthetic results) ──
            let mut repaired: Vec<Message> = Vec::with_capacity(self.messages.len() + 4);
            let len = self.messages.len();
            let mut i = 0;

            while i < len {
                let msg = &self.messages[i];

                // Extract tool_use ids from this assistant message (if any).
                let tool_use_ids: Vec<String> = if msg.role == Role::Assistant {
                    if let MessageContent::Blocks(blocks) = &msg.content {
                        blocks
                            .iter()
                            .filter_map(|b| {
                                if let ContentBlock::ToolUse { id, .. } = b {
                                    Some(id.clone())
                                } else {
                                    None
                                }
                            })
                            .collect()
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                };

                repaired.push(msg.clone());

                if tool_use_ids.is_empty() {
                    i += 1;
                    continue;
                }

                // Collect tool_result ids already present in the next message.
                let covered: std::collections::HashSet<&str> = self
                    .messages
                    .get(i + 1)
                    .and_then(|next| {
                        if next.role != Role::User {
                            return None;
                        }
                        if let MessageContent::Blocks(blocks) = &next.content {
                            Some(
                                blocks
                                    .iter()
                                    .filter_map(|b| {
                                        if let ContentBlock::ToolResult { tool_use_id, .. } = b {
                                            Some(tool_use_id.as_str())
                                        } else {
                                            None
                                        }
                                    })
                                    .collect(),
                            )
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();

                let missing: Vec<String> = tool_use_ids
                    .into_iter()
                    .filter(|id| !covered.contains(id.as_str()))
                    .collect();

                if missing.is_empty() {
                    i += 1;
                    continue;
                }

                tracing::warn!(
                    msg_idx = i,
                    ?missing,
                    covered_count = covered.len(),
                    "hydration repair: synthesizing tool_results for orphaned tool_uses"
                );

                let error_results: Vec<ContentBlock> = missing
                    .into_iter()
                    .map(|id| ContentBlock::ToolResult {
                        tool_use_id: id,
                        content: "Tool execution was interrupted (context was forked or pruned)"
                            .into(),
                        is_error: true,
                    })
                    .collect();

                if covered.is_empty() {
                    // No tool_result message follows at all — insert one.
                    repaired.push(Message::tool_results(error_results));
                } else {
                    // Next message has *some* results — append the missing ones
                    // into it so all results stay in one user message.
                    i += 1;
                    let mut next = self.messages[i].clone();
                    if let MessageContent::Blocks(ref mut blocks) = next.content {
                        blocks.extend(error_results);
                    }
                    repaired.push(next);
                }

                i += 1;
            }

            // ── Pass 3: Reverse repair (orphaned tool_results → drop) ──
            // Late-arriving ToolResult blocks that already have a synthetic
            // error result produce User messages with tool_results that don't
            // match any tool_use in the preceding assistant message. The API
            // rejects these. Strip them out.
            let mut cleaned: Vec<Message> = Vec::with_capacity(repaired.len());
            for (idx, msg) in repaired.iter().enumerate() {
                if msg.role == Role::User
                    && let MessageContent::Blocks(blocks) = &msg.content
                {
                    // Get tool_use IDs from the preceding assistant message
                    let preceding_tool_uses: std::collections::HashSet<&str> = idx
                        .checked_sub(1)
                        .and_then(|prev_idx| cleaned.get(prev_idx))
                        .and_then(|prev| {
                            if prev.role != Role::Assistant {
                                return None;
                            }
                            if let MessageContent::Blocks(pblocks) = &prev.content {
                                Some(
                                    pblocks
                                        .iter()
                                        .filter_map(|b| {
                                            if let ContentBlock::ToolUse { id, .. } = b {
                                                Some(id.as_str())
                                            } else {
                                                None
                                            }
                                        })
                                        .collect(),
                                )
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();

                    // Filter: keep only tool_results that match a preceding tool_use,
                    // plus any non-tool-result blocks (text).
                    let filtered: Vec<ContentBlock> = blocks.iter().filter(|b| {
                            match b {
                                ContentBlock::ToolResult { tool_use_id, .. } => {
                                    if preceding_tool_uses.contains(tool_use_id.as_str()) {
                                        true
                                    } else {
                                        tracing::warn!(
                                            msg_idx = idx,
                                            tool_use_id,
                                            "hydration repair: dropping orphaned tool_result (late arrival)"
                                        );
                                        false
                                    }
                                }
                                _ => true,
                            }
                        }).cloned().collect();

                    if filtered.is_empty() {
                        // Entire message was orphaned tool_results — skip it
                        continue;
                    }
                    if filtered.len() < blocks.len() {
                        // Some blocks were dropped — push the filtered version
                        cleaned.push(Message {
                            role: Role::User,
                            content: MessageContent::Blocks(filtered),
                        });
                        continue;
                    }
                }
                cleaned.push(msg.clone());
            }

            cleaned
        }
    }

    // Build index for parent lookups (Error block fold-into-parent logic).
    let blocks_by_id: std::collections::HashMap<kaijutsu_types::BlockId, &kaijutsu_types::BlockSnapshot> =
        blocks.iter().map(|b| (b.id, b)).collect();

    let mut state = HydrationState::new();

    for block in blocks {
        // Skip blocks that shouldn't appear in LLM history
        if block.compacted {
            continue;
        }
        if block.ephemeral {
            continue;
        }
        if block.excluded {
            continue;
        }
        if matches!(block.kind, BlockKind::Thinking | BlockKind::File) {
            continue;
        }
        // Skip System blocks unless they're Drift, Error, Notification, or Resource (D-34)
        if block.role == BlockRole::System
            && block.kind != BlockKind::Drift
            && block.kind != BlockKind::Error
            && block.kind != BlockKind::Notification
            && block.kind != BlockKind::Resource
        {
            continue;
        }
        if block.content.is_empty()
            && block.kind != BlockKind::ToolCall
            && block.kind != BlockKind::ToolResult
            && block.kind != BlockKind::Error
            && block.kind != BlockKind::Notification
            && block.kind != BlockKind::Resource
        {
            continue;
        }

        match (block.role, block.kind) {
            (BlockRole::User, BlockKind::Text) => {
                state.flush_all();
                state.messages.push(Message::user(&block.content));
            }
            (BlockRole::User, BlockKind::ToolCall) => {
                // User-initiated shell command — extract the code and wait for
                // the paired ToolResult to emit a single user message.
                state.flush_all();
                let code = block
                    .tool_input
                    .as_ref()
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                    .and_then(|v| v.get("code").and_then(|c| c.as_str().map(String::from)))
                    .unwrap_or_else(|| block.content.clone());
                state.user_shell_pending.insert(block.id, code);
            }
            (BlockRole::Model, BlockKind::Text) => {
                // Flush pending tool results before accumulating assistant text
                state.flush_tool_results();
                match &mut state.assistant_text {
                    Some(text) => {
                        text.push('\n');
                        text.push_str(&block.content);
                    }
                    None => {
                        state.assistant_text = Some(block.content.clone());
                    }
                }
            }
            (BlockRole::Model, BlockKind::ToolCall) => {
                // Flush pending tool results before accumulating tool uses
                state.flush_tool_results();
                let tool_use_id = block.tool_use_id.clone()
                    .unwrap_or_else(|| {
                        tracing::warn!(block_id = %block.id.to_key(), "ToolCall block missing tool_use_id, falling back to block ID");
                        block.id.to_key()
                    });
                let name = block.tool_name.clone().unwrap_or_default();
                let input = block
                    .tool_input
                    .as_ref()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or(serde_json::Value::Null);
                state.tool_uses.push(ContentBlock::ToolUse {
                    id: tool_use_id,
                    name,
                    input,
                });
            }
            (BlockRole::Asset, BlockKind::Text) => {
                // img_block / img_block_from_path — Asset role, content_type
                // Image, content holds the CAS hash. Surface to vision-capable
                // models as an Image content block; the server-side path
                // resolves the hash to bytes before the request goes out.
                use kaijutsu_types::ContentType;
                if block.content_type == ContentType::Image {
                    state.flush_all();
                    state.messages.push(Message {
                        role: Role::User,
                        content: MessageContent::Blocks(vec![ContentBlock::Image {
                            hash: block.content.clone(),
                            media_type: ContentType::Image.as_mime().to_string(),
                            data_base64: None,
                        }]),
                    });
                }
                // Other Asset content types stay skipped (no current producer).
            }
            (BlockRole::Tool, BlockKind::Text) => {
                // Tool-authored rich content (svg_block / abc_block).
                // Surface as a user message envelope so the model can read
                // back its own output on the next turn (A1). Plain text from
                // tools stays skipped — only typed content (Svg/Abc) is
                // worth round-tripping.
                use kaijutsu_types::ContentType;
                match block.content_type {
                    ContentType::Svg | ContentType::Abc => {
                        let envelope = kaijutsu_types::format_tool_content_for_llm(block);
                        state.flush_all();
                        state.messages.push(Message::user(envelope));
                    }
                    _ => {
                        // Skip — no rich content to surface.
                    }
                }
            }
            (BlockRole::Tool, BlockKind::ToolResult) => {
                let user_code = block
                    .tool_call_id
                    .and_then(|id| state.user_shell_pending.remove(&id));
                if let Some(code) = user_code {
                    // User-initiated shell result → emit as a single user message
                    state.flush_all();
                    let output = block.content.trim();
                    if output.is_empty() {
                        state
                            .messages
                            .push(Message::user(format!("[User ran `{}`]", code)));
                    } else {
                        state
                            .messages
                            .push(Message::user(format!("[User ran `{}`]\n{}", code, output)));
                    }
                } else {
                    // Agent-initiated tool result — existing logic
                    state.flush_assistant();
                    let tool_use_id = block.tool_use_id.clone()
                        .or_else(|| {
                            tracing::warn!(block_id = %block.id.to_key(), "ToolResult block missing tool_use_id, falling back to tool_call_id");
                            block.tool_call_id.map(|id| id.to_key())
                        })
                        .unwrap_or_else(|| {
                            tracing::warn!(block_id = %block.id.to_key(), "ToolResult block missing both tool_use_id and tool_call_id, falling back to block ID");
                            block.id.to_key()
                        });
                    state.tool_results.push(ContentBlock::ToolResult {
                        tool_use_id,
                        content: block.content.clone(),
                        is_error: block.is_error,
                    });
                }
            }
            (_, BlockKind::Drift) => {
                // Drift blocks become user messages with provenance context
                let source_label = block
                    .source_context
                    .map(|id| id.short())
                    .unwrap_or_else(|| "unknown".to_string());
                let drift_kind = block.drift_kind.map(|k| k.as_str()).unwrap_or("drift");
                let prefixed = format!(
                    "[{} from context {}]\n\n{}",
                    drift_kind, source_label, block.content
                );
                state.flush_all();
                state.messages.push(Message::user(&prefixed));
            }
            (_, BlockKind::Error) => {
                // Error blocks: fold into parent ToolResult content if possible,
                // otherwise emit as standalone user message.
                let envelope = kaijutsu_types::format_error_for_llm(block);

                let parent_is_tool_result = block
                    .parent_id
                    .and_then(|pid| blocks_by_id.get(&pid))
                    .is_some_and(|parent| parent.kind == BlockKind::ToolResult);

                if parent_is_tool_result {
                    // Find the matching tool_result by parent's tool_use_id
                    let parent_tool_use_id = block
                        .parent_id
                        .and_then(|pid| blocks_by_id.get(&pid))
                        .and_then(|p| p.tool_use_id.as_deref());

                    let folded = if let Some(target_id) = parent_tool_use_id {
                        state
                            .tool_results
                            .iter_mut()
                            .find_map(|tr| {
                                if let ContentBlock::ToolResult {
                                    tool_use_id,
                                    content,
                                    ..
                                } = tr
                                {
                                    if tool_use_id == target_id {
                                        content.push_str("\n\n");
                                        content.push_str(&envelope);
                                        Some(())
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            })
                            .is_some()
                    } else {
                        false
                    };

                    if !folded {
                        // Parent's tool_result already flushed or not found — standalone
                        state.flush_all();
                        state.messages.push(Message::user(envelope));
                    }
                } else {
                    state.flush_all();
                    state.messages.push(Message::user(envelope));
                }
            }
            (_, BlockKind::Notification) => {
                // Notification blocks (D-34): surface broker tool/log events to the
                // LLM as a user message so the model sees tool-world changes.
                let envelope = kaijutsu_types::format_notification_for_llm(block);
                state.flush_all();
                state.messages.push(Message::user(envelope));
            }
            (_, BlockKind::Resource) => {
                // Resource blocks (D-34, D-43): surface MCP resource contents to
                // the LLM as a user message with an XML envelope so the model sees
                // the read-through body (truncated per
                // RESOURCE_CONTENT_HYDRATION_BUDGET).
                let envelope = kaijutsu_types::format_resource_for_llm(block);
                state.flush_all();
                state.messages.push(Message::user(envelope));
            }
            _ => {
                // Skip unexpected role/kind combinations
            }
        }
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
        assert_eq!(
            RigProvider::Anthropic(anthropic::Client::new("fake").unwrap()).name(),
            "anthropic"
        );
    }

    #[test]
    fn test_registry_concrete_type() {
        let mut registry = LlmRegistry::new();
        let provider = Arc::new(RigProvider::Anthropic(
            anthropic::Client::new("fake").unwrap(),
        ));
        registry.register("anthropic", provider);
        registry.set_default("anthropic");

        assert!(registry.default_provider().is_some());
        assert_eq!(registry.list(), vec!["anthropic"]);
    }

    #[test]
    fn test_model_alias_resolution() {
        let mut registry = LlmRegistry::new();
        let provider = Arc::new(RigProvider::Anthropic(
            anthropic::Client::new("fake").unwrap(),
        ));
        registry.register("anthropic", provider);
        registry.set_default("anthropic");

        let mut aliases = HashMap::new();
        aliases.insert(
            "fast".to_string(),
            toml_config::ModelAlias {
                provider: "anthropic".to_string(),
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
        fn skips_thinking_file_compacted_empty_but_includes_drift() {
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
                {
                    let mut b =
                        BlockSnapshot::text(BlockId::new(c, m, 2), None, BlockRole::Model, "old");
                    b.compacted = true;
                    b
                },
                BlockSnapshot::text(BlockId::new(c, m, 3), None, BlockRole::Model, ""),
            ];

            let msgs = hydrate_from_blocks(&blocks);
            // user + assistant + drift (as user) = 3; thinking/file/compacted/empty skipped
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
            // Thinking blocks are still skipped
            assert_eq!(msgs.len(), 2);
            assert_eq!(msgs[0].as_text(), Some("Hello"));
            assert_eq!(msgs[1].as_text(), Some("Hi!"));
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
                tool: Some("example_tool".into()),
                count: None,
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
            assert_eq!(msgs.len(), 1, "expected one user message for one notification");
            assert_eq!(msgs[0].role, Role::User);
            let text = msgs[0].as_text().expect("notification should hydrate as text");
            // Envelope produced by format_notification_for_llm.
            assert!(
                text.starts_with("<notification "),
                "expected XML envelope, got {text:?}"
            );
            assert!(text.contains("instance=\"gpal\""));
            assert!(text.contains("kind=\"tool_added\""));
            assert!(text.contains("tool=\"example_tool\""));
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
            assert!(msgs[2]
                .as_text()
                .expect("notification text")
                .contains("kind=\"tool_removed\""));
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
            assert_eq!(msgs.len(), 1, "System-role Notification must not be filtered out");
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

        #[test]
        fn resolve_image_blocks_fills_data_from_cas() {
            use kaijutsu_cas::{ContentStore, FileStore};
            let tmp = tempfile::tempdir().unwrap();
            let cas = FileStore::at_path(tmp.path());
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
            resolve_image_blocks_from_cas(&mut messages, &cas);
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

        #[test]
        fn resolve_image_blocks_tolerates_missing_hash() {
            use kaijutsu_cas::{ContentStore, FileStore};
            let tmp = tempfile::tempdir().unwrap();
            let cas = FileStore::at_path(tmp.path());
            let mut messages = vec![Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![ContentBlock::Image {
                    hash: "0".repeat(64),
                    media_type: "image/png".to_string(),
                    data_base64: None,
                }]),
            }];
            // Should not panic, should leave block unresolved.
            resolve_image_blocks_from_cas(&mut messages, &cas);
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
    }
}
