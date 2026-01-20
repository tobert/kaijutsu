//! Anthropic Claude provider implementation.

use async_trait::async_trait;
use anthropic_api::{
    messages::{
        ContentBlockDelta, ContentBlockStart, Message as ApiMessage,
        MessageContent as ApiMessageContent, MessageRole as ApiMessageRole, MessagesBuilder,
        RequestContentBlock, ResponseContentBlock, StreamEvent as ApiStreamEvent,
        Thinking, ThinkingType, Tool as ApiTool,
    },
    models::ModelList,
    Credentials,
};
use tokio::sync::mpsc::Receiver;
use tokio::sync::RwLock;

use super::stream::{LlmStream, StreamEvent, StreamRequest, StreamingBlockType};
use super::{
    CompletionRequest, CompletionResponse, ContentBlock, LlmError, LlmProvider, LlmResult,
    Message, MessageContent, ResponseBlock, Role, Usage,
};

/// Default model to use when none specified.
pub const DEFAULT_MODEL: &str = "claude-haiku-4-5-20251001";

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
        let role = match msg.role {
            Role::User => ApiMessageRole::User,
            Role::Assistant => ApiMessageRole::Assistant,
        };

        let content = match &msg.content {
            MessageContent::Text(text) => ApiMessageContent::Text(text.clone()),
            MessageContent::Blocks(blocks) => {
                let api_blocks: Vec<RequestContentBlock> = blocks
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => {
                            Some(RequestContentBlock::Text { text: text.clone() })
                        }
                        ContentBlock::ToolUse { .. } => {
                            // Tool use blocks from assistant are handled differently
                            // (they're in ResponseContentBlock, not RequestContentBlock)
                            None
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => Some(RequestContentBlock::ToolResult {
                            tool_use_id: tool_use_id.clone(),
                            content: content.clone(),
                            is_error: if *is_error { Some(true) } else { None },
                        }),
                    })
                    .collect();
                ApiMessageContent::ContentBlocks(api_blocks)
            }
        };

        ApiMessage { role, content }
    }

    /// Convert API response blocks to our ResponseBlock type.
    fn convert_blocks(content: &[ResponseContentBlock]) -> Vec<ResponseBlock> {
        content
            .iter()
            .filter_map(|block| match block {
                ResponseContentBlock::Text { text } => Some(ResponseBlock::Text {
                    text: text.clone(),
                }),
                ResponseContentBlock::Thinking { thinking, signature } => {
                    Some(ResponseBlock::Thinking {
                        thinking: thinking.clone(),
                        signature: Some(signature.clone()),
                    })
                }
                ResponseContentBlock::ToolUse { id, name, input } => {
                    Some(ResponseBlock::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    })
                }
                // Skip other block types for now (redacted thinking, etc.)
                _ => None,
            })
            .collect()
    }

    /// Extract text content from response blocks (for backward compatibility).
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
        let blocks = Self::convert_blocks(&response.content);

        Ok(CompletionResponse {
            content,
            blocks,
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

impl AnthropicProvider {
    /// Create a streaming request.
    ///
    /// Returns an [`AnthropicStream`] that can be polled for events.
    pub async fn stream(&self, request: StreamRequest) -> LlmResult<AnthropicStream> {
        let messages: Vec<ApiMessage> = request
            .messages
            .iter()
            .map(Self::convert_message)
            .collect();

        let mut builder = MessagesBuilder::builder(&request.model, messages, request.max_tokens as u64)
            .credentials(self.credentials.clone());

        // Add system prompt if provided
        if let Some(system) = &request.system {
            builder = builder.system(system.clone());
        }

        // Add temperature if provided
        if let Some(temp) = request.temperature {
            builder = builder.temperature(temp as f64);
        }

        // Enable thinking if requested
        if request.thinking_enabled {
            let budget = request.thinking_budget.unwrap_or(4096);
            builder = builder.thinking(Thinking {
                thinking_type: ThinkingType::Enabled,
                budget_tokens: budget as u64,
            });
        }

        // Add tools if provided
        if let Some(ref tools) = request.tools {
            let api_tools: Vec<ApiTool> = tools
                .iter()
                .map(|t| ApiTool {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    input_schema: t.input_schema.clone(),
                })
                .collect();
            builder = builder.tools(api_tools);
        }

        let receiver = builder.create_stream().await.map_err(|e| {
            LlmError::NetworkError(format!("Failed to create stream: {}", e))
        })?;

        Ok(AnthropicStream {
            model: request.model.clone(),
            receiver,
            current_block: None,
            current_block_index: None,
            finished: false,
            input_tokens: None,
            output_tokens: None,
            tool_use_buffer: None,
            stop_reason: None,
        })
    }
}

/// Streaming response adapter for Anthropic Claude.
///
/// Converts Anthropic's native streaming events into provider-agnostic
/// [`StreamEvent`]s for consumption by the block handler.
pub struct AnthropicStream {
    /// The model name.
    model: String,
    /// Receiver for API streaming events.
    receiver: Receiver<ApiStreamEvent>,
    /// Current block type being streamed.
    current_block: Option<StreamingBlockType>,
    /// Current block index (for tracking).
    current_block_index: Option<u32>,
    /// Whether the stream has finished.
    finished: bool,
    /// Input tokens (updated at end).
    input_tokens: Option<u32>,
    /// Output tokens (updated at end).
    output_tokens: Option<u32>,
    /// Buffer for tool use JSON (built incrementally).
    tool_use_buffer: Option<ToolUseBuffer>,
    /// Stop reason captured from MessageDelta.
    stop_reason: Option<String>,
}

/// Buffer for building tool use input incrementally.
struct ToolUseBuffer {
    id: String,
    name: String,
    input_json: String,
}

impl LlmStream for AnthropicStream {
    async fn next_event(&mut self) -> Option<StreamEvent> {
        if self.finished {
            return None;
        }

        loop {
            let api_event = self.receiver.recv().await?;

            match api_event {
                ApiStreamEvent::MessageStart { .. } => {
                    // Ignore message start, we'll emit block events
                    continue;
                }

                ApiStreamEvent::ContentBlockStart { index, content_block } => {
                    self.current_block_index = Some(index);

                    match content_block {
                        ContentBlockStart::Text { text: _ } => {
                            self.current_block = Some(StreamingBlockType::Text);
                            return Some(StreamEvent::TextStart);
                        }
                        ContentBlockStart::Thinking { thinking: _ } => {
                            self.current_block = Some(StreamingBlockType::Thinking);
                            return Some(StreamEvent::ThinkingStart);
                        }
                        ContentBlockStart::ToolUse { id, name, input: _ } => {
                            // Note: initial `input` is always {} - actual params come via input_json_delta
                            self.tool_use_buffer = Some(ToolUseBuffer {
                                id: id.clone(),
                                name: name.clone(),
                                input_json: String::new(), // Built up from deltas
                            });
                            // Don't emit yet - we'll emit when the block is complete
                            continue;
                        }
                    }
                }

                ApiStreamEvent::ContentBlockDelta { index, delta } => {
                    // Verify we're on the expected block
                    if self.current_block_index != Some(index) {
                        // Block index mismatch - update and continue
                        self.current_block_index = Some(index);
                    }

                    match delta {
                        ContentBlockDelta::TextDelta { text } => {
                            return Some(StreamEvent::TextDelta(text));
                        }
                        ContentBlockDelta::ThinkingDelta { thinking } => {
                            return Some(StreamEvent::ThinkingDelta(thinking));
                        }
                        ContentBlockDelta::InputJsonDelta { partial_json } => {
                            // Append to tool use buffer
                            if let Some(ref mut buffer) = self.tool_use_buffer {
                                buffer.input_json.push_str(&partial_json);
                            }
                            continue;
                        }
                    }
                }

                ApiStreamEvent::ContentBlockStop { index: _ } => {
                    // Emit block end if we're in a text/thinking block
                    if let Some(block_type) = self.current_block.take() {
                        match block_type {
                            StreamingBlockType::Thinking => {
                                return Some(StreamEvent::ThinkingEnd);
                            }
                            StreamingBlockType::Text => {
                                return Some(StreamEvent::TextEnd);
                            }
                        }
                    }

                    // If we have a tool use buffer, emit it now
                    if let Some(buffer) = self.tool_use_buffer.take() {
                        // Default to empty object if no deltas received (tools with no required params)
                        let input_str = if buffer.input_json.is_empty() { "{}" } else { &buffer.input_json };
                        let input = serde_json::from_str(input_str)
                            .unwrap_or(serde_json::json!({}));
                        return Some(StreamEvent::ToolUse {
                            id: buffer.id,
                            name: buffer.name,
                            input,
                        });
                    }

                    continue;
                }

                ApiStreamEvent::MessageDelta { delta, usage } => {
                    // Capture usage stats and stop reason
                    self.output_tokens = Some(usage.output_tokens);
                    self.stop_reason = delta.stop_reason.clone();
                    continue;
                }

                ApiStreamEvent::MessageStop => {
                    self.finished = true;
                    return Some(StreamEvent::Done {
                        stop_reason: self.stop_reason.take(),
                        input_tokens: self.input_tokens,
                        output_tokens: self.output_tokens,
                    });
                }

                ApiStreamEvent::Ping => {
                    // Ignore keepalive
                    continue;
                }
            }
        }
    }

    fn model(&self) -> &str {
        &self.model
    }
}

impl std::fmt::Debug for AnthropicStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicStream")
            .field("model", &self.model)
            .field("current_block", &self.current_block)
            .field("finished", &self.finished)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_message_text() {
        let msg = Message::user("hello");
        let api_msg = AnthropicProvider::convert_message(&msg);

        assert!(matches!(api_msg.role, ApiMessageRole::User));
        assert!(matches!(api_msg.content, ApiMessageContent::Text(ref t) if t == "hello"));
    }

    #[test]
    fn test_convert_message_tool_result() {
        let msg = Message::tool_results(vec![ContentBlock::ToolResult {
            tool_use_id: "tool_123".to_string(),
            content: "result text".to_string(),
            is_error: false,
        }]);
        let api_msg = AnthropicProvider::convert_message(&msg);

        assert!(matches!(api_msg.role, ApiMessageRole::User));
        match api_msg.content {
            ApiMessageContent::ContentBlocks(blocks) => {
                assert_eq!(blocks.len(), 1);
                match &blocks[0] {
                    RequestContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => {
                        assert_eq!(tool_use_id, "tool_123");
                        assert_eq!(content, "result text");
                        assert_eq!(*is_error, None);
                    }
                    _ => panic!("Expected ToolResult"),
                }
            }
            _ => panic!("Expected ContentBlocks"),
        }
    }

    #[test]
    fn test_default_model() {
        assert!(DEFAULT_MODEL.contains("claude"));
    }
}
