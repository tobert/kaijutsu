//! Gemini SSE → kaijutsu [`StreamEvent`] state machine.

use super::build::GEMINI_SIG_PREFIX;
use super::sse::GeminiSseEvent;
use super::types::{GenerateContentResponse, UsageMetadata};
use crate::llm::stream::{
    FinishReason, GeminiUsageExtra, SpentUsage, StreamError, StreamErrorKind, StreamEvent,
    UsageExtra,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenBlock {
    Thinking,
    Text,
}

/// State machine converting [`GeminiSseEvent`]s into [`StreamEvent`]s.
#[derive(Debug, Default)]
pub struct StateMachine {
    open: Option<OpenBlock>,
    pending_thinking_signature: Option<String>,
    usage: UsageMetadata,
    finish_reason: Option<String>,
    finish_message: Option<String>,
    next_call_seq: u64,
    terminated: bool,
}

impl StateMachine {
    pub fn new() -> Self {
        Self::default()
    }

    fn close_open(&mut self, out: &mut Vec<StreamEvent>) {
        match self.open.take() {
            Some(OpenBlock::Thinking) => {
                out.push(StreamEvent::ThinkingEnd {
                    signature: self.pending_thinking_signature.take(),
                });
            }
            Some(OpenBlock::Text) => {
                out.push(StreamEvent::TextEnd);
            }
            None => {}
        }
    }

    fn ensure_thinking(&mut self, out: &mut Vec<StreamEvent>) {
        if self.open == Some(OpenBlock::Thinking) {
            return;
        }
        self.close_open(out);
        out.push(StreamEvent::ThinkingStart);
        self.open = Some(OpenBlock::Thinking);
    }

    fn ensure_text(&mut self, out: &mut Vec<StreamEvent>) {
        if self.open == Some(OpenBlock::Text) {
            return;
        }
        self.close_open(out);
        out.push(StreamEvent::TextStart);
        self.open = Some(OpenBlock::Text);
    }

    fn usage_extra(&self) -> Option<UsageExtra> {
        Some(UsageExtra::Gemini(GeminiUsageExtra {
            cached_content_tokens: self.usage.cached_content_token_count.unwrap_or(0),
            thoughts_tokens: self.usage.thoughts_token_count.unwrap_or(0),
            tool_use_prompt_tokens: self.usage.tool_use_prompt_token_count.unwrap_or(0),
        }))
    }

    fn spent_usage(&self) -> Option<SpentUsage> {
        if self.usage.prompt_token_count.is_none() && self.usage.candidates_token_count.is_none() {
            return None;
        }
        Some(SpentUsage {
            input_tokens: self.usage.prompt_token_count,
            output_tokens: self.usage.candidates_token_count,
            extra: self.usage_extra(),
        })
    }

    fn alloc_call_id(&mut self, wire_id: Option<String>) -> String {
        if let Some(id) = wire_id.filter(|s| !s.is_empty()) {
            return id;
        }
        self.next_call_seq += 1;
        format!("gemini_call_{}", self.next_call_seq)
    }

    /// Process one decoded SSE event.
    pub fn step(&mut self, event: GeminiSseEvent) -> Vec<StreamEvent> {
        if self.terminated {
            return Vec::new();
        }
        let mut out = Vec::new();
        match event {
            GeminiSseEvent::Error(mut err) => {
                self.close_open(&mut out);
                if err.usage.is_none()
                    && let Some(spent) = self.spent_usage()
                {
                    err = err.with_usage(spent);
                }
                self.terminated = true;
                out.push(StreamEvent::Error(err));
            }
            GeminiSseEvent::Chunk(chunk) => {
                self.handle_chunk(chunk, &mut out);
            }
        }
        out
    }

    fn handle_chunk(&mut self, chunk: GenerateContentResponse, out: &mut Vec<StreamEvent>) {
        if let Some(u) = chunk.usage_metadata {
            self.usage = u;
        }

        if let Some(fb) = chunk.prompt_feedback
            && let Some(reason) = fb.block_reason
        {
            self.close_open(out);
            self.terminated = true;
            let msg = fb
                .block_reason_message
                .unwrap_or_else(|| format!("prompt blocked: {reason}"));
            let mut err = StreamError::new(StreamErrorKind::ContentFilter, msg);
            if let Some(spent) = self.spent_usage() {
                err = err.with_usage(spent);
            }
            out.push(StreamEvent::Error(err));
            return;
        }

        let Some(candidate) = chunk.candidates.into_iter().next() else {
            return;
        };

        if let Some(content) = candidate.content {
            for part in content.parts {
                let sig = part
                    .thought_signature
                    .filter(|s| !s.is_empty())
                    .map(|s| format!("{GEMINI_SIG_PREFIX}{s}"));

                if part.thought == Some(true) {
                    self.ensure_thinking(out);
                    if let Some(text) = part.text
                        && !text.is_empty()
                    {
                        out.push(StreamEvent::ThinkingDelta(text));
                    }
                    if let Some(s) = sig {
                        self.pending_thinking_signature = Some(s);
                    }
                } else if let Some(fc) = part.function_call {
                    self.close_open(out);
                    let id = self.alloc_call_id(fc.id);
                    let input = if fc.args.is_null() {
                        serde_json::json!({})
                    } else {
                        fc.args
                    };
                    out.push(StreamEvent::ToolUse {
                        id,
                        name: fc.name,
                        input,
                        signature: sig,
                    });
                } else if let Some(text) = part.text {
                    if !text.is_empty() {
                        self.ensure_text(out);
                        out.push(StreamEvent::TextDelta(text));
                    } else if let Some(s) = sig
                        && self.open == Some(OpenBlock::Thinking)
                    {
                        self.pending_thinking_signature = Some(s);
                    }
                }
            }
        }

        if let Some(reason) = candidate.finish_reason {
            self.finish_reason = Some(reason);
            self.finish_message = candidate.finish_message;
        }
    }

    /// Finalize when the SSE byte stream reaches EOF.
    pub fn finish(&mut self) -> Vec<StreamEvent> {
        if self.terminated {
            return Vec::new();
        }
        self.terminated = true;
        let mut out = Vec::new();
        self.close_open(&mut out);

        match self.finish_reason.as_deref() {
            Some("STOP") => {
                out.push(StreamEvent::Done {
                    stop_reason: Some(FinishReason::EndTurn.as_str().to_string()),
                    input_tokens: self.usage.prompt_token_count,
                    output_tokens: self.usage.candidates_token_count,
                    extra: self.usage_extra(),
                });
            }
            Some("MAX_TOKENS") => {
                out.push(StreamEvent::Done {
                    stop_reason: Some(FinishReason::MaxTokens.as_str().to_string()),
                    input_tokens: self.usage.prompt_token_count,
                    output_tokens: self.usage.candidates_token_count,
                    extra: self.usage_extra(),
                });
            }
            Some(r @ ("SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "IMAGE_SAFETY")) => {
                let msg = self
                    .finish_message
                    .clone()
                    .unwrap_or_else(|| format!("response blocked by Gemini ({r})"));
                let mut err = StreamError::new(StreamErrorKind::ContentFilter, msg);
                if let Some(spent) = self.spent_usage() {
                    err = err.with_usage(spent);
                }
                out.push(StreamEvent::Error(err));
            }
            Some(other) => {
                let msg = self
                    .finish_message
                    .clone()
                    .unwrap_or_else(|| format!("Gemini stream stopped with reason: {other}"));
                let mut err = StreamError::new(StreamErrorKind::Truncated(other.to_string()), msg);
                if let Some(spent) = self.spent_usage() {
                    err = err.with_usage(spent);
                }
                out.push(StreamEvent::Error(err));
            }
            None => {
                let mut err = StreamError::new(
                    StreamErrorKind::Truncated("no_finish_reason".to_string()),
                    "Gemini stream ended without finishReason",
                );
                if let Some(spent) = self.spent_usage() {
                    err = err.with_usage(spent);
                }
                out.push(StreamEvent::Error(err));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::gemini::sse::decode_event;

    fn chunk_event(json: &str) -> GeminiSseEvent {
        let ev = eventsource_stream::Event {
            event: String::new(),
            data: json.to_string(),
            id: String::new(),
            retry: None,
        };
        decode_event(&ev).unwrap().unwrap()
    }

    #[test]
    fn streams_thinking_text_and_signed_tool_call() {
        let mut sm = StateMachine::new();
        let mut events = Vec::new();
        events.extend(sm.step(chunk_event(
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"let me check","thought":true,"thoughtSignature":"tSig1"}]}}]}"#,
        )));
        events.extend(sm.step(chunk_event(
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Running ls."}]}}]}"#,
        )));
        events.extend(sm.step(chunk_event(
            r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"ls","args":{"path":"."}},"thoughtSignature":"cSig1"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":20,"cachedContentTokenCount":40,"thoughtsTokenCount":10}}"#,
        )));
        events.extend(sm.finish());

        assert_eq!(events.len(), 8, "{events:?}");
        assert!(matches!(events[0], StreamEvent::ThinkingStart));
        assert!(matches!(&events[1], StreamEvent::ThinkingDelta(t) if t == "let me check"));
        assert!(matches!(&events[2], StreamEvent::ThinkingEnd { signature: Some(s) } if s == "gemini:tSig1"));
        assert!(matches!(events[3], StreamEvent::TextStart));
        assert!(matches!(&events[4], StreamEvent::TextDelta(t) if t == "Running ls."));
        assert!(matches!(events[5], StreamEvent::TextEnd));
        assert!(matches!(
            &events[6],
            StreamEvent::ToolUse { name, signature: Some(s), .. } if name == "ls" && s == "gemini:cSig1"
        ));
        assert!(matches!(
            &events[7],
            StreamEvent::Done {
                stop_reason: Some(r),
                input_tokens: Some(100),
                output_tokens: Some(20),
                extra: Some(UsageExtra::Gemini(g)),
            } if r == "end_turn" && g.cached_content_tokens == 40 && g.thoughts_tokens == 10
        ));
    }

    #[test]
    fn maps_safety_finish_reason_to_content_filter_with_spent_usage() {
        let mut sm = StateMachine::new();
        let mut events = sm.step(chunk_event(
            r#"{"candidates":[{"finishReason":"SAFETY"}],"usageMetadata":{"promptTokenCount":50,"candidatesTokenCount":0}}"#,
        ));
        events.extend(sm.finish());
        assert!(matches!(
            events.as_slice(),
            [StreamEvent::Error(e)] if e.kind == StreamErrorKind::ContentFilter
                && e.usage.as_ref().and_then(|u| u.input_tokens) == Some(50)
        ));
    }
}
