//! Anthropic Claude provider implementation.

use async_trait::async_trait;
use anthropic_api::{
    messages::{
        Message as ApiMessage, MessageContent as ApiMessageContent, MessageRole as ApiMessageRole,
        MessagesBuilder, ResponseContentBlock,
    },
    models::ModelList,
    Credentials,
};
use tokio::sync::RwLock;

use super::{CompletionRequest, CompletionResponse, LlmError, LlmProvider, LlmResult, Message, Role, Usage};

/// Default model to use when none specified.
pub const DEFAULT_MODEL: &str = "claude-sonnet-4-20250514";

/// Anthropic Claude provider.
pub struct AnthropicProvider {
    credentials: Credentials,
    default_model: String,
    /// Cached model list (fetched lazily from API).
    cached_models: RwLock<Option<Vec<String>>>,
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicProvider")
            .field("default_model", &self.default_model)
            .field("credentials", &"[REDACTED]")
            .finish()
    }
}

impl AnthropicProvider {
    /// Create a new Anthropic provider from environment variables.
    ///
    /// Reads `ANTHROPIC_API_KEY` from the environment.
    ///
    /// # Panics
    ///
    /// Panics if `ANTHROPIC_API_KEY` is not set.
    pub fn from_env() -> Self {
        Self {
            credentials: Credentials::from_env(),
            default_model: DEFAULT_MODEL.to_string(),
            cached_models: RwLock::new(None),
        }
    }

    /// Create a new Anthropic provider with an explicit API key.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            credentials: Credentials::new(api_key, ""),
            default_model: DEFAULT_MODEL.to_string(),
            cached_models: RwLock::new(None),
        }
    }

    /// Create a new Anthropic provider with API key and custom base URL.
    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            credentials: Credentials::new(api_key, base_url),
            default_model: DEFAULT_MODEL.to_string(),
            cached_models: RwLock::new(None),
        }
    }

    /// Fetch available models from the API and cache them.
    pub async fn fetch_models(&self) -> LlmResult<Vec<String>> {
        let model_list = ModelList::builder()
            .credentials(self.credentials.clone())
            .create()
            .await
            .map_err(|e| LlmError::ApiError(e.error.message))?;

        let models: Vec<String> = model_list.data.into_iter().map(|m| m.id).collect();

        // Cache the result
        *self.cached_models.write().await = Some(models.clone());

        Ok(models)
    }

    /// Get cached models, or fetch if not cached.
    pub async fn models(&self) -> LlmResult<Vec<String>> {
        // Check cache first
        if let Some(models) = self.cached_models.read().await.as_ref() {
            return Ok(models.clone());
        }

        // Fetch and cache
        self.fetch_models().await
    }

    /// Set the default model.
    pub fn set_default_model(&mut self, model: impl Into<String>) {
        self.default_model = model.into();
    }

    /// Get the default model.
    pub fn default_model(&self) -> &str {
        &self.default_model
    }

    /// Convert our Message to the API's Message type.
    fn convert_message(msg: &Message) -> ApiMessage {
        ApiMessage {
            role: match msg.role {
                Role::User => ApiMessageRole::User,
                Role::Assistant => ApiMessageRole::Assistant,
            },
            content: ApiMessageContent::Text(msg.content.clone()),
        }
    }

    /// Extract text content from response blocks.
    fn extract_text(content: &[ResponseContentBlock]) -> String {
        content
            .iter()
            .filter_map(|block| match block {
                ResponseContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn available_models(&self) -> Vec<&str> {
        // Return default model synchronously; use fetch_models() for full list
        vec![DEFAULT_MODEL]
    }

    async fn is_available(&self) -> bool {
        // Could do a lightweight API check here, but for now just return true
        // since we have credentials
        true
    }

    async fn complete(&self, request: CompletionRequest) -> LlmResult<CompletionResponse> {
        let messages: Vec<ApiMessage> = request.messages.iter().map(Self::convert_message).collect();

        let mut builder = MessagesBuilder::builder(
            &request.model,
            messages,
            request.max_tokens as u64,
        )
        .credentials(self.credentials.clone());

        // Add system prompt if provided
        if let Some(system) = &request.system {
            builder = builder.system(system.clone());
        }

        // Add temperature if provided
        if let Some(temp) = request.temperature {
            builder = builder.temperature(temp as f64);
        }

        let response = builder.create().await.map_err(|e| {
            let msg = e.error.message.clone();
            let error_type = e.error.error_type.as_str();

            match error_type {
                "authentication_error" => LlmError::AuthError(msg),
                "rate_limit_error" => LlmError::RateLimited(msg),
                "invalid_request_error" => LlmError::InvalidRequest(msg),
                _ => LlmError::ApiError(msg),
            }
        })?;

        let content = Self::extract_text(&response.content);

        Ok(CompletionResponse {
            content,
            model: response.model,
            stop_reason: response.stop_reason,
            usage: Usage {
                input_tokens: response.usage.input_tokens,
                output_tokens: response.usage.output_tokens,
            },
        })
    }

    async fn prompt(&self, model: &str, prompt: &str) -> LlmResult<String> {
        let request = CompletionRequest::new(model, vec![Message::user(prompt)]);
        let response = self.complete(request).await?;
        Ok(response.content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_message() {
        let msg = Message::user("hello");
        let api_msg = AnthropicProvider::convert_message(&msg);

        assert!(matches!(api_msg.role, ApiMessageRole::User));
        assert!(matches!(api_msg.content, ApiMessageContent::Text(ref t) if t == "hello"));
    }

    #[test]
    fn test_default_model() {
        assert!(DEFAULT_MODEL.contains("claude"));
    }
}
