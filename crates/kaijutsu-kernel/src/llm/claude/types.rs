//! Anthropic Messages API native types.
//!
//! Wire shapes matching <https://docs.anthropic.com/en/api/messages> and
//! <https://docs.anthropic.com/en/api/messages-streaming>. Constructed
//! from kaijutsu's [`Message`](crate::llm::Message) /
//! [`ContentBlock`](crate::llm::ContentBlock) by [`super::build`], serialized
//! to JSON via reqwest in [`super::Client::stream`].
//!
//! Field names are `snake_case` on the wire (Anthropic's convention) —
//! `#[serde(rename_all)]` not needed since our Rust field names already
//! match. `cache_control` and `tool_choice` are serialized only when
//! set; `#[serde(skip_serializing_if)]` keeps the wire compact and
//! avoids triggering API validation paths we don't intend to exercise.

use serde::{Deserialize, Serialize};

/// Top-level POST body for `/v1/messages`.
#[derive(Debug, Clone, Serialize)]
pub struct MessagesRequest {
    pub model: String,
    pub max_tokens: u64,

    /// Conversation history. First message must be `user`; thereafter
    /// roles must alternate. tool_result blocks are role=user, tool_use
    /// blocks are role=assistant.
    pub messages: Vec<RequestMessage>,

    /// System prompt as either a single string or a content-block list.
    /// Block list is required when applying `cache_control` to the
    /// system prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemPrompt>,

    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<RequestTool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,

    /// `true` enables SSE streaming; the response Content-Type is
    /// `text/event-stream` and bodies are emitted as the events listed
    /// in `super::sse`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,

    /// Extended thinking knob. Claude 4.x; older models reject this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Thinking>,

    /// Optional list of stop sequences.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
}

/// `system` field shape — string or block list.
///
/// The block list form is required for `cache_control` on the system
/// prompt; a plain string can't carry the breakpoint annotation.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum SystemPrompt {
    /// Single text string; the simple, common case.
    Text(String),
    /// Block list — required when any block carries `cache_control`.
    Blocks(Vec<SystemBlock>),
}

#[derive(Debug, Clone, Serialize)]
pub struct SystemBlock {
    #[serde(rename = "type")]
    pub kind: &'static str, // always "text"
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

impl SystemBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            kind: "text",
            text: text.into(),
            cache_control: None,
        }
    }

    pub fn with_cache_control(mut self, cc: CacheControl) -> Self {
        self.cache_control = Some(cc);
        self
    }
}

/// A turn in the conversation.
#[derive(Debug, Clone, Serialize)]
pub struct RequestMessage {
    pub role: MessageRole,
    pub content: MessageContent,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
}

/// Message content — string or block list.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<RequestContent>),
}

/// A content block within a request message.
///
/// `#[serde(tag = "type")]` produces Anthropic's `{"type": "text", "text":
/// "..."}` shape rather than the externally-tagged enum default.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RequestContent {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    Image {
        source: ImageSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// Echoed back from assistant history. Required when extended
    /// thinking is enabled AND there's a tool_use block in the same
    /// turn — signature is what Anthropic uses to verify we're not
    /// fabricating reasoning chains.
    Thinking {
        thinking: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct ImageSource {
    #[serde(rename = "type")]
    pub kind: &'static str, // always "base64"
    pub media_type: String,
    pub data: String,
}

/// Tool definition surfaced to the model.
#[derive(Debug, Clone, Serialize)]
pub struct RequestTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

/// `tool_choice` field. Defaults to `auto` server-side — only set when
/// we want to override (force a specific tool, force any tool, or
/// disable tools for a turn).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)] // variants reserved for Phase 2.5+
pub enum ToolChoice {
    Auto,
    Any,
    Tool { name: String },
    None,
}

/// Thinking config for `/v1/messages`.
///
/// `Adaptive` is the current-generation shape (Claude 4.6+): the model
/// decides when and how much to think. `display` controls only whether
/// the response's thinking text is a readable summary or an empty
/// string — thinking happens and is billed the same either way. Opus
/// 4.7+ / Sonnet 5 / Fable 5 default to `omitted` server-side, so a
/// client that wants visible reasoning must send `summarized`
/// explicitly.
///
/// `Enabled { budget_tokens }` is the legacy pre-4.6 shape. It is
/// rejected with a 400 on Opus 4.7+ / Sonnet 5 / Fable 5 — only valid
/// for models [`Thinking::default_for_model`] returns `None` for.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)] // Enabled/Disabled reserved for explicit overrides
pub enum Thinking {
    Adaptive {
        #[serde(skip_serializing_if = "Option::is_none")]
        display: Option<ThinkingDisplay>,
    },
    Enabled {
        budget_tokens: u64,
    },
    Disabled,
}

/// Visibility of thinking text in responses (`thinking.display`).
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)] // Omitted reserved for explicit override
pub enum ThinkingDisplay {
    Summarized,
    Omitted,
}

impl Thinking {
    /// Adaptive thinking with readable (summarized) thinking text.
    pub fn adaptive_summarized() -> Self {
        Self::Adaptive {
            display: Some(ThinkingDisplay::Summarized),
        }
    }

    /// Model-gated default thinking config for a Claude model id.
    ///
    /// Adaptive thinking exists on the 4.6+ generation (Opus 4.6/4.7/
    /// 4.8, Sonnet 4.6, Sonnet 5) and the Fable/Mythos tier; sending
    /// `type: "adaptive"` to anything older is a 400, so those return
    /// `None` (thinking simply off, matching prior behavior). Parses
    /// `claude-<family>-<major>[-<minor>][-<date>]` rather than
    /// allowlisting ids so future point releases inherit the right
    /// default without a table edit.
    pub fn default_for_model(model: &str) -> Option<Self> {
        let mut parts = model.split('-');
        if parts.next() != Some("claude") {
            return None;
        }
        let family = parts.next()?;
        match family {
            // Always-on thinking tier; adaptive is the only accepted shape.
            "fable" | "mythos" => Some(Self::adaptive_summarized()),
            "opus" | "sonnet" => {
                let major: u32 = parts.next()?.parse().ok()?;
                // Minor is absent on bare ids like `claude-sonnet-5` and
                // date-suffixed on pinned ids; a non-numeric next part
                // (the date) reads as minor 0, which is correct for
                // `claude-sonnet-5-20260203`-style ids.
                let minor: u32 = parts
                    .next()
                    .and_then(|p| if p.len() <= 2 { p.parse().ok() } else { None })
                    .unwrap_or(0);
                ((major, minor) >= (4, 6)).then(Self::adaptive_summarized)
            }
            // haiku-4-5 and unknown families: no adaptive support.
            _ => None,
        }
    }
}

/// `cache_control` annotation. Anthropic accepts this on tools,
/// system blocks, and any message content block. Up to 4 breakpoints
/// per request; extras are ignored server-side.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub kind: CacheControlKind,
    /// `Some("1h")` for the extended variant; omit for default 5-minute.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CacheControlKind {
    Ephemeral,
}

impl CacheControl {
    /// 5-minute ephemeral cache (Anthropic's default).
    pub fn ephemeral() -> Self {
        Self {
            kind: CacheControlKind::Ephemeral,
            ttl: None,
        }
    }

    /// 1-hour extended cache.
    pub fn extended() -> Self {
        Self {
            kind: CacheControlKind::Ephemeral,
            ttl: Some("1h"),
        }
    }
}

// ============================================================================
// Response types (non-streaming /v1/messages reply)
// ============================================================================

/// Full response body for non-streaming `/v1/messages` calls.
#[derive(Debug, Clone, Deserialize)]
pub struct MessagesResponse {
    pub id: String,
    pub model: String,
    pub role: String,
    pub content: Vec<ResponseContent>,
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
    pub usage: ResponseUsage,
}

/// A single content block in a non-streaming response.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseContent {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        #[serde(default)]
        signature: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

/// Token usage payload — both non-streaming responses and the streaming
/// `message_start` / `message_delta` events use this shape.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ResponseUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    /// Tokens read from a cache hit; only populated when at least one
    /// breakpoint matched a prior request's prefix.
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    /// Tokens written when a new cache entry was created (charged at
    /// 1.25x base input cost for the 5-minute variant, 2x for 1-hour).
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_request_minimal_serializes_without_optional_fields() {
        let req = MessagesRequest {
            model: "claude-haiku-4-5".into(),
            max_tokens: 1024,
            messages: vec![RequestMessage {
                role: MessageRole::User,
                content: MessageContent::Text("hi".into()),
            }],
            system: None,
            tools: vec![],
            tool_choice: None,
            temperature: None,
            stream: None,
            thinking: None,
            stop_sequences: vec![],
        };
        let v = serde_json::to_value(&req).unwrap();
        // Optional fields skip-serialize when None / empty.
        assert!(v.get("system").is_none(), "system must omit when None");
        assert!(v.get("tools").is_none(), "empty tools must skip-serialize");
        assert!(v.get("stream").is_none());
        assert!(v.get("thinking").is_none());
        assert_eq!(v["model"], "claude-haiku-4-5");
        assert_eq!(v["max_tokens"], 1024);
    }

    #[test]
    fn thinking_adaptive_summarized_wire_shape() {
        let v = serde_json::to_value(Thinking::adaptive_summarized()).unwrap();
        assert_eq!(v["type"], "adaptive");
        assert_eq!(v["display"], "summarized");
    }

    #[test]
    fn thinking_adaptive_without_display_omits_field() {
        let v = serde_json::to_value(Thinking::Adaptive { display: None }).unwrap();
        assert_eq!(v["type"], "adaptive");
        assert!(v.get("display").is_none());
    }

    #[test]
    fn thinking_default_covers_adaptive_generation() {
        // 4.6+ opus/sonnet, sonnet 5, and the fable/mythos tier get
        // adaptive + summarized.
        for model in [
            "claude-opus-4-6",
            "claude-opus-4-7",
            "claude-opus-4-8",
            "claude-sonnet-4-6",
            "claude-sonnet-5",
            "claude-fable-5",
            "claude-mythos-5",
        ] {
            let t = Thinking::default_for_model(model)
                .unwrap_or_else(|| panic!("{model} must default to adaptive thinking"));
            let v = serde_json::to_value(&t).unwrap();
            assert_eq!(v["type"], "adaptive", "{model}");
            assert_eq!(v["display"], "summarized", "{model}");
        }
    }

    #[test]
    fn thinking_default_off_where_adaptive_would_400() {
        // Pre-4.6 models and haiku reject `type: "adaptive"` — the
        // default must omit thinking entirely, matching prior behavior.
        for model in [
            "claude-haiku-4-5",
            "claude-haiku-4-5-20251001",
            "claude-sonnet-4-5",
            "claude-sonnet-4-5-20250929",
            "claude-opus-4-1",
            "claude-3-5-sonnet-20241022",
            "deepseek-v4-flash", // non-claude id must never match
            "gpt-4o",
        ] {
            assert!(
                Thinking::default_for_model(model).is_none(),
                "{model} must not default to thinking"
            );
        }
    }

    #[test]
    fn cache_control_ephemeral_omits_ttl() {
        let cc = CacheControl::ephemeral();
        let v = serde_json::to_value(cc).unwrap();
        assert_eq!(v["type"], "ephemeral");
        assert!(v.get("ttl").is_none());
    }

    #[test]
    fn cache_control_extended_includes_1h() {
        let cc = CacheControl::extended();
        let v = serde_json::to_value(cc).unwrap();
        assert_eq!(v["type"], "ephemeral");
        assert_eq!(v["ttl"], "1h");
    }

    #[test]
    fn request_content_text_serializes_with_type_tag() {
        let block = RequestContent::Text {
            text: "hello".into(),
            cache_control: None,
        };
        let v = serde_json::to_value(&block).unwrap();
        assert_eq!(v["type"], "text");
        assert_eq!(v["text"], "hello");
        assert!(v.get("cache_control").is_none());
    }

    #[test]
    fn request_content_tool_use_round_trip() {
        let block = RequestContent::ToolUse {
            id: "toolu_01ABC".into(),
            name: "get_weather".into(),
            input: serde_json::json!({"location": "Tokyo"}),
            cache_control: None,
        };
        let v = serde_json::to_value(&block).unwrap();
        assert_eq!(v["type"], "tool_use");
        assert_eq!(v["id"], "toolu_01ABC");
        assert_eq!(v["name"], "get_weather");
        assert_eq!(v["input"]["location"], "Tokyo");
    }

    #[test]
    fn system_prompt_block_list_carries_cache_control() {
        let sys = SystemPrompt::Blocks(vec![
            SystemBlock::text("You are helpful.").with_cache_control(CacheControl::ephemeral())
        ]);
        let v = serde_json::to_value(&sys).unwrap();
        // Untagged enum: the array form serializes directly as JSON array.
        let arr = v.as_array().expect("system blocks must serialize as array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[0]["text"], "You are helpful.");
        assert_eq!(arr[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn thinking_enabled_serializes_with_budget() {
        let t = Thinking::Enabled { budget_tokens: 5000 };
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(v["type"], "enabled");
        assert_eq!(v["budget_tokens"], 5000);
    }

    #[test]
    fn response_content_text_deserializes() {
        let json = r#"{"type":"text","text":"hello"}"#;
        let block: ResponseContent = serde_json::from_str(json).unwrap();
        match block {
            ResponseContent::Text { text } => assert_eq!(text, "hello"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn response_usage_with_cache_stats_deserializes() {
        let json = r#"{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":80,"cache_creation_input_tokens":0}"#;
        let usage: ResponseUsage = serde_json::from_str(json).unwrap();
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.cache_read_input_tokens, 80);
    }

    #[test]
    fn response_usage_missing_cache_fields_defaults_to_zero() {
        // Older API responses (or pre-caching prefixes) don't include the
        // cache_* fields. Defaults must let them parse cleanly.
        let json = r#"{"input_tokens":100,"output_tokens":50}"#;
        let usage: ResponseUsage = serde_json::from_str(json).unwrap();
        assert_eq!(usage.cache_read_input_tokens, 0);
        assert_eq!(usage.cache_creation_input_tokens, 0);
    }
}
