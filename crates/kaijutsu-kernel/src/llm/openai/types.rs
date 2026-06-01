//! DeepSeek (OpenAI-compatible) chat-completions native types.
//!
//! Wire shapes matching <https://api-docs.deepseek.com/api/create-chat-completion>.
//! DeepSeek speaks the OpenAI chat-completions dialect with two
//! provider-specific extensions we care about:
//!
//! - **`reasoning_content`** on assistant deltas / messages — the
//!   chain-of-thought for thinking-mode models (V4 thinks by default).
//!   It streams *before* `content`. The V4 multi-turn contract
//!   (<https://api-docs.deepseek.com/guides/thinking_mode>): for an
//!   assistant turn that **performs tool calls**, its `reasoning_content`
//!   MUST be echoed back on every subsequent request or DeepSeek returns
//!   HTTP 400; on a turn with no tool call it is ignored if sent. Because
//!   the API checks only presence (not authenticity), [`super::build`]
//!   emits `reasoning_content` on every assistant message — the real
//!   chain-of-thought when we have it, an empty string as a fallback when
//!   we don't (tool-only or cross-provider history). An earlier model
//!   generation 400'd on *any* echoed reasoning; that rule inverted in V4.
//! - **cache accounting** in `usage`: `prompt_cache_hit_tokens` /
//!   `prompt_cache_miss_tokens` (DeepSeek caches automatically — no
//!   `cache_control` knob) and `completion_tokens_details.reasoning_tokens`.
//!
//! Unlike Anthropic's bracketed content blocks, OpenAI-style SSE does
//! not delimit blocks — the [`super::stream`] state machine synthesizes
//! the `*Start` / `*End` brackets kaijutsu's CRDT writer expects from
//! *which* delta field is populated.

use serde::{Deserialize, Serialize};

const DEFAULT_MAX_TOKENS: u64 = 8192;

// ============================================================================
// Request types (POST /chat/completions body)
// ============================================================================

/// Top-level POST body for `/chat/completions`.
#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<RequestMessage>,

    pub max_tokens: u64,

    /// `true` enables SSE streaming terminated by `data: [DONE]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,

    /// When streaming, request a trailing usage-only chunk before
    /// `[DONE]` so we can capture token accounting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,

    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<RequestTool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,

    /// Honored by `deepseek-v4-*` in non-thinking mode; silently ignored
    /// by the pure reasoning path. We still send it when set — DeepSeek
    /// doesn't error on it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
}

impl ChatRequest {
    /// Default max_tokens when [`crate::llm::BuildOpts`] leaves it at the
    /// generous kernel default — DeepSeek rejects requests whose
    /// `max_tokens` exceeds the per-model ceiling, so we don't blindly
    /// forward 64k.
    pub fn clamp_max_tokens(requested: u64) -> u64 {
        if requested == 0 {
            DEFAULT_MAX_TOKENS
        } else {
            requested
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

/// A turn in the conversation. OpenAI roles: `system`, `user`,
/// `assistant`, `tool`. tool results are their own `tool` messages
/// (one per `tool_call_id`), not block lists inside a user turn.
#[derive(Debug, Clone, Serialize)]
pub struct RequestMessage {
    pub role: MessageRole,

    /// `None` is valid for an assistant message that only carries
    /// `tool_calls`. OpenAI accepts `content: null` there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<MessageContent>,

    /// Thinking-mode chain-of-thought echoed back on assistant messages.
    /// Set (possibly to `""`) on every assistant turn so tool-call turns
    /// satisfy DeepSeek's V4 round-trip requirement; `None` (skipped) on
    /// system/user/tool roles. See the module doc.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,

    /// Present only on assistant messages that invoke tools.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<RequestToolCall>,

    /// Present only on `tool` messages — ties the result to the call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl RequestMessage {
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::System,
            content: Some(MessageContent::Text(text.into())),
            reasoning_content: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: Some(MessageContent::Text(text.into())),
            reasoning_content: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// An assistant turn. `reasoning_content` is `Some` (possibly `""`) for
    /// DeepSeek's V4 round-trip requirement and `None` for plain
    /// OpenAI-compatible servers that don't want the field; [`super::build`]
    /// decides based on the provider's `reasoning_required` flag.
    pub fn assistant(
        content: Option<MessageContent>,
        reasoning_content: Option<String>,
        tool_calls: Vec<RequestToolCall>,
    ) -> Self {
        Self {
            role: MessageRole::Assistant,
            content,
            reasoning_content,
            tool_calls,
            tool_call_id: None,
        }
    }

    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Tool,
            content: Some(MessageContent::Text(content.into())),
            reasoning_content: None,
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

/// Message content — a plain string or a multimodal content-part list
/// (text + images). The list form is OpenAI's vision shape.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

/// A multimodal content part. `image_url` carries a `data:` URL for
/// base64-inlined images (kaijutsu resolves CAS hashes to base64 before
/// the request is built).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

#[derive(Debug, Clone, Serialize)]
pub struct ImageUrl {
    /// Either an `https://…` URL or a `data:<mime>;base64,<data>` URL.
    pub url: String,
}

/// A tool call echoed back in an assistant request message.
#[derive(Debug, Clone, Serialize)]
pub struct RequestToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str, // always "function"
    pub function: RequestFunctionCall,
}

#[derive(Debug, Clone, Serialize)]
pub struct RequestFunctionCall {
    pub name: String,
    /// Arguments as a JSON-encoded *string* (OpenAI's shape), not a
    /// nested object.
    pub arguments: String,
}

/// Tool definition surfaced to the model.
#[derive(Debug, Clone, Serialize)]
pub struct RequestTool {
    #[serde(rename = "type")]
    pub kind: &'static str, // always "function"
    pub function: ToolFunction,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolFunction {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// `tool_choice` — defaults to `auto` server-side; only set to override.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)] // variants reserved for forced-tool paths
pub enum ToolChoice {
    Auto,
    None,
    Required,
}

// ============================================================================
// Streaming response types (text/event-stream chunks)
// ============================================================================

/// One `chat.completion.chunk` SSE data payload.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ChatChunk {
    #[serde(default)]
    pub choices: Vec<ChunkChoice>,
    /// Present only on the trailing usage chunk (requested via
    /// `stream_options.include_usage`). `None` on content chunks.
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ChunkChoice {
    #[serde(default)]
    pub delta: Delta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// Incremental delta. Exactly one of `content` / `reasoning_content` /
/// `tool_calls` carries payload on a given chunk in practice, but the
/// state machine tolerates any combination.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Delta {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    /// DeepSeek thinking-mode chain-of-thought. Streams before `content`.
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCallChunk>>,
}

/// A streamed tool-call fragment. `id` and `function.name` arrive on the
/// first fragment for a given `index`; `function.arguments` streams as
/// JSON-string fragments appended across chunks.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ToolCallChunk {
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionChunk>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct FunctionChunk {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

// ============================================================================
// Non-streaming response types (prompt() path)
// ============================================================================

/// Full response body for a non-streaming `/chat/completions` call.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatResponse {
    pub choices: Vec<ResponseChoice>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResponseChoice {
    pub message: ResponseMessage,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResponseMessage {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
}

// ============================================================================
// Usage + error
// ============================================================================

/// Token usage payload. Streaming reports it once on the trailing chunk;
/// non-streaming reports it on the response body. `prompt_cache_*` and
/// `reasoning_tokens` are DeepSeek-specific and default to 0 elsewhere.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    /// Input tokens served from DeepSeek's automatic context cache.
    #[serde(default)]
    pub prompt_cache_hit_tokens: u64,
    /// Input tokens that missed the cache (billed at full rate).
    #[serde(default)]
    pub prompt_cache_miss_tokens: u64,
    #[serde(default)]
    pub completion_tokens_details: Option<CompletionTokensDetails>,
}

impl Usage {
    /// Reasoning tokens, flattened from the nested details. 0 when the
    /// model didn't think or didn't report it.
    pub fn reasoning_tokens(&self) -> u64 {
        self.completion_tokens_details
            .as_ref()
            .map(|d| d.reasoning_tokens)
            .unwrap_or(0)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct CompletionTokensDetails {
    #[serde(default)]
    pub reasoning_tokens: u64,
}

/// Error body returned on a 4xx/5xx (OpenAI shape:
/// `{"error": {"message": ..., "type": ..., "code": ...}}`).
#[derive(Debug, Clone, Deserialize)]
pub struct ApiError {
    pub error: ApiErrorBody,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiErrorBody {
    pub message: String,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_request_omits_optional_fields() {
        let req = ChatRequest {
            model: "deepseek-v4-flash".into(),
            messages: vec![RequestMessage::user_text("hi")],
            max_tokens: 1024,
            stream: None,
            stream_options: None,
            tools: vec![],
            tool_choice: None,
            temperature: None,
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["model"], "deepseek-v4-flash");
        assert_eq!(v["max_tokens"], 1024);
        assert!(v.get("stream").is_none(), "stream must skip when None");
        assert!(v.get("tools").is_none(), "empty tools must skip");
        assert!(v.get("tool_choice").is_none());
        assert!(v.get("temperature").is_none());
        // messages[0] is a bare user text message
        assert_eq!(v["messages"][0]["role"], "user");
        assert_eq!(v["messages"][0]["content"], "hi");
        assert!(
            v["messages"][0].get("tool_calls").is_none(),
            "empty tool_calls must skip"
        );
    }

    #[test]
    fn streaming_request_includes_usage_option() {
        let req = ChatRequest {
            model: "deepseek-v4-pro".into(),
            messages: vec![RequestMessage::user_text("hi")],
            max_tokens: 256,
            stream: Some(true),
            stream_options: Some(StreamOptions { include_usage: true }),
            tools: vec![],
            tool_choice: None,
            temperature: None,
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["stream"], true);
        assert_eq!(v["stream_options"]["include_usage"], true);
    }

    #[test]
    fn assistant_message_with_tool_calls_serializes_openai_shape() {
        let msg = RequestMessage::assistant(
            None,
            Some("thinking about it".into()),
            vec![RequestToolCall {
                id: "call_abc".into(),
                kind: "function",
                function: RequestFunctionCall {
                    name: "read_file".into(),
                    arguments: r#"{"path":"/etc/hosts"}"#.into(),
                },
            }],
        );
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["role"], "assistant");
        // content omitted entirely when None
        assert!(v.get("content").is_none(), "null content must skip");
        // reasoning_content rides alongside the tool call (V4 requirement)
        assert_eq!(v["reasoning_content"], "thinking about it");
        assert_eq!(v["tool_calls"][0]["id"], "call_abc");
        assert_eq!(v["tool_calls"][0]["type"], "function");
        assert_eq!(v["tool_calls"][0]["function"]["name"], "read_file");
        // arguments is a JSON-encoded string, not a nested object
        assert_eq!(
            v["tool_calls"][0]["function"]["arguments"],
            r#"{"path":"/etc/hosts"}"#
        );
    }

    #[test]
    fn tool_result_message_is_its_own_tool_role_turn() {
        let msg = RequestMessage::tool_result("call_abc", "127.0.0.1 localhost");
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["role"], "tool");
        assert_eq!(v["tool_call_id"], "call_abc");
        assert_eq!(v["content"], "127.0.0.1 localhost");
    }

    #[test]
    fn tool_definition_serializes_function_wrapper() {
        let tool = RequestTool {
            kind: "function",
            function: ToolFunction {
                name: "get_weather".into(),
                description: "Look up weather".into(),
                parameters: serde_json::json!({"type": "object"}),
            },
        };
        let v = serde_json::to_value(&tool).unwrap();
        assert_eq!(v["type"], "function");
        assert_eq!(v["function"]["name"], "get_weather");
        assert_eq!(v["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn image_content_part_serializes_as_data_url() {
        let content = MessageContent::Parts(vec![
            ContentPart::Text {
                text: "what is this?".into(),
            },
            ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: "data:image/png;base64,AAAA".into(),
                },
            },
        ]);
        let v = serde_json::to_value(&content).unwrap();
        let arr = v.as_array().expect("parts serialize as array");
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[0]["text"], "what is this?");
        assert_eq!(arr[1]["type"], "image_url");
        assert_eq!(arr[1]["image_url"]["url"], "data:image/png;base64,AAAA");
    }

    #[test]
    fn content_chunk_with_text_delta_deserializes() {
        let json = r#"{"id":"x","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#;
        let chunk: ChatChunk = serde_json::from_str(json).unwrap();
        assert_eq!(chunk.choices.len(), 1);
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("Hello"));
        assert!(chunk.choices[0].delta.reasoning_content.is_none());
        assert!(chunk.choices[0].finish_reason.is_none());
        assert!(chunk.usage.is_none());
    }

    #[test]
    fn reasoning_content_delta_deserializes() {
        let json = r#"{"choices":[{"delta":{"reasoning_content":"let me think"},"finish_reason":null}]}"#;
        let chunk: ChatChunk = serde_json::from_str(json).unwrap();
        assert_eq!(
            chunk.choices[0].delta.reasoning_content.as_deref(),
            Some("let me think")
        );
        assert!(chunk.choices[0].delta.content.is_none());
    }

    #[test]
    fn tool_call_chunk_deserializes_with_index_and_fragment() {
        let json = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"f","arguments":"{\"a\":"}}]},"finish_reason":null}]}"#;
        let chunk: ChatChunk = serde_json::from_str(json).unwrap();
        let tcs = chunk.choices[0].delta.tool_calls.as_ref().unwrap();
        assert_eq!(tcs[0].index, 0);
        assert_eq!(tcs[0].id.as_deref(), Some("call_1"));
        let f = tcs[0].function.as_ref().unwrap();
        assert_eq!(f.name.as_deref(), Some("f"));
        assert_eq!(f.arguments.as_deref(), Some(r#"{"a":"#));
    }

    #[test]
    fn trailing_usage_chunk_deserializes_with_cache_and_reasoning() {
        // The include_usage trailing chunk: empty choices, full usage.
        let json = r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":50,"total_tokens":150,"prompt_cache_hit_tokens":80,"prompt_cache_miss_tokens":20,"completion_tokens_details":{"reasoning_tokens":30}}}"#;
        let chunk: ChatChunk = serde_json::from_str(json).unwrap();
        assert!(chunk.choices.is_empty());
        let usage = chunk.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 50);
        assert_eq!(usage.prompt_cache_hit_tokens, 80);
        assert_eq!(usage.prompt_cache_miss_tokens, 20);
        assert_eq!(usage.reasoning_tokens(), 30);
    }

    #[test]
    fn usage_without_optional_fields_defaults_zero() {
        let json = r#"{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}"#;
        let usage: Usage = serde_json::from_str(json).unwrap();
        assert_eq!(usage.prompt_cache_hit_tokens, 0);
        assert_eq!(usage.prompt_cache_miss_tokens, 0);
        assert_eq!(usage.reasoning_tokens(), 0);
    }

    #[test]
    fn done_sentinel_is_not_valid_chunk_json() {
        // Sanity: "[DONE]" is the SSE sentinel, handled by the parser
        // before JSON decode — it must not parse as a ChatChunk.
        assert!(serde_json::from_str::<ChatChunk>("[DONE]").is_err());
    }

    #[test]
    fn api_error_body_deserializes() {
        let json = r#"{"error":{"message":"Insufficient Balance","type":"insufficient_balance","code":null}}"#;
        let err: ApiError = serde_json::from_str(json).unwrap();
        assert_eq!(err.error.message, "Insufficient Balance");
        assert_eq!(err.error.kind.as_deref(), Some("insufficient_balance"));
    }

    #[test]
    fn non_streaming_response_extracts_content_and_reasoning() {
        let json = r#"{"choices":[{"message":{"role":"assistant","content":"the answer","reasoning_content":"the thinking"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#;
        let resp: ChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices[0].message.content.as_deref(), Some("the answer"));
        assert_eq!(
            resp.choices[0].message.reasoning_content.as_deref(),
            Some("the thinking")
        );
    }
}
