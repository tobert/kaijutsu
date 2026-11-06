//! Alibaba's explicit cache: `cache_control` markers placed from
//! [`BuildOpts::cache_breakpoints`](crate::llm::stream::BuildOpts).
//!
//! A marker on a message's text caches the prompt prefix through it (at least
//! 1,024 tokens). Alibaba takes markers only on message content, never on
//! tools, and finds an earlier cache only within 20 content blocks before a
//! marker. So, as Claude's builder does with its automatic tail breakpoint,
//! a request that asks for caching also marks its last message: each
//! request's tail marker is then a turn or so past the previous one's.
//! See `docs/alibaba-backend.md`, "Explicit cache".

use crate::llm::openai::build::should_warn_effort_dropped;
use crate::llm::openai::types::{CacheControl, ContentPart, MessageContent, RequestMessage};
use crate::llm::stream::{CacheTarget, CacheTtl};

/// The most `cache_control` markers one request carries.
const MAX_MARKERS: usize = 4;

/// Mark `messages` (the wire messages) from `breakpoints`.
///
/// - `System` marks the system message. `Tools` does too: tools render
///   before the first message, so its marker covers them.
/// - `MessageIndex(i)` marks the last wire message kaijutsu message `i`
///   produced; `ends[i]` is the wire index one past it.
/// - When `breakpoints` is not empty, the last message is marked too.
///
/// A marker lands on the last text part of a message; a message with no text
/// (an assistant message that only calls tools, an image) passes its marker
/// back to the nearest earlier message with text. Breakpoints apply in
/// declaration order, the tail last, up to [`MAX_MARKERS`]; two that land on
/// one message share its marker. Every dropped breakpoint is warned.
pub fn place(
    messages: &mut [RequestMessage],
    breakpoints: &[CacheTarget],
    has_system: bool,
    has_tools: bool,
    ends: &[usize],
    model: &str,
) {
    if breakpoints.is_empty() || messages.is_empty() {
        return;
    }
    let mut marked: Vec<usize> = Vec::new();
    for bp in breakpoints {
        if bp.ttl() == CacheTtl::Extended && should_warn_effort_dropped("alibaba", model, "cache_ttl", "extended") {
            tracing::warn!(model, target = ?bp, "Alibaba caches for 5 minutes; an extended breakpoint is sent as ephemeral");
        }
        let target = match bp {
            CacheTarget::Tools(_) if !has_tools => {
                tracing::warn!(model, target = ?bp, "cache breakpoint dropped: the request has no tools");
                continue;
            }
            CacheTarget::System(_) if !has_system => {
                tracing::warn!(model, target = ?bp, "cache breakpoint dropped: the request has no system prompt");
                continue;
            }
            CacheTarget::Tools(_) | CacheTarget::System(_) => 0,
            CacheTarget::MessageIndex(i, _) => match ends.get(*i) {
                Some(&end) if end > 0 => end - 1,
                _ => {
                    tracing::warn!(model, target = ?bp, len = ends.len(), "cache breakpoint dropped: index out of range");
                    continue;
                }
            },
        };
        if marked.len() == MAX_MARKERS {
            tracing::warn!(model, target = ?bp, "cache breakpoint dropped: {MAX_MARKERS}-marker cap reached");
            continue;
        }
        mark_from(messages, target, &mut marked, model);
    }
    if marked.len() < MAX_MARKERS {
        mark_from(messages, messages.len() - 1, &mut marked, model);
    }
}

/// Mark the nearest message at or before `from` that has text, unless a
/// message on the way already carries a marker.
fn mark_from(messages: &mut [RequestMessage], from: usize, marked: &mut Vec<usize>, model: &str) {
    for i in (0..=from).rev() {
        if marked.contains(&i) {
            return;
        }
        if mark(&mut messages[i]) {
            marked.push(i);
            return;
        }
    }
    tracing::warn!(model, index = from, "cache breakpoint dropped: no message with text at or before it");
}

/// Put a marker on the last text part of `msg`. A string becomes one text
/// part. False when `msg` has no text.
fn mark(msg: &mut RequestMessage) -> bool {
    match &mut msg.content {
        Some(MessageContent::Text(text)) if !text.is_empty() => {
            let text = std::mem::take(text);
            msg.content =
                Some(MessageContent::Parts(vec![ContentPart::Text { text, cache_control: Some(CacheControl::Ephemeral) }]));
            true
        }
        Some(MessageContent::Parts(parts)) => {
            match parts.iter_mut().rev().find_map(|p| match p {
                ContentPart::Text { cache_control, .. } => Some(cache_control),
                ContentPart::ImageUrl { .. } => None,
            }) {
                Some(cache_control) => {
                    *cache_control = Some(CacheControl::Ephemeral);
                    true
                }
                None => false,
            }
        }
        _ => false,
    }
}
