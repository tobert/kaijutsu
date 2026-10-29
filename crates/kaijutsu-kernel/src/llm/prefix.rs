//! Where a request stops extending the one before it, and what that costs.
//!
//! A provider's prompt cache reads only a prefix an earlier request sent, and
//! Anthropic binds each thinking block to the conversation before it. A
//! request that only appends keeps both. One that changes or drops an earlier
//! message (a hydration that applies an exclusion, a sliding window, an edit)
//! re-sends everything after the change without a cache read, and a provider
//! that binds thinking drops the thinking blocks after it. [`SentPrefix`]
//! records what a request sent, one fingerprint per message, so the next
//! request can name the first message that differs and what follows it.

use std::hash::{Hash, Hasher};

use super::hydrate::estimate_tokens;
use super::{ContentBlock, Message, MessageContent, ToolDefinition};

/// What one request sent: the system prompt and tools as one fingerprint,
/// then one per message.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SentPrefix {
    head: u64,
    messages: Vec<u64>,
}

/// Where a request stops extending the previous one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Divergence {
    /// The first message that differs, or `None` when the system prompt or
    /// the tools changed, which comes before every message.
    pub at: Option<usize>,
    /// Messages in the new request.
    pub messages: usize,
    /// Estimated tokens from the first difference to the end: the part a
    /// prompt cache cannot read.
    pub tokens_after: u64,
    /// Signed thinking blocks from the first difference on, which a
    /// provider that binds thinking to the conversation drops.
    pub thinking_after: usize,
}

impl SentPrefix {
    /// Fingerprint a request.
    pub fn of(system: Option<&str>, tools: &[ToolDefinition], messages: &[Message]) -> Self {
        let mut head = std::collections::hash_map::DefaultHasher::new();
        system.hash(&mut head);
        for tool in tools {
            tool.name.hash(&mut head);
            tool.description.hash(&mut head);
            tool.input_schema.to_string().hash(&mut head);
        }
        Self { head: head.finish(), messages: messages.iter().map(fingerprint).collect() }
    }

    /// Where `next`, sent with `messages`, stops extending this request.
    /// `None` when it only appends, or only drops messages from the end.
    pub fn divergence(&self, next: &SentPrefix, messages: &[Message]) -> Option<Divergence> {
        let at = if self.head != next.head {
            None
        } else {
            let first = self.messages.iter().zip(&next.messages).position(|(a, b)| a != b)?;
            Some(first)
        };
        let from = at.unwrap_or(0).min(messages.len());
        let rest = &messages[from..];
        let thinking_after = rest
            .iter()
            .filter_map(|m| match &m.content {
                MessageContent::Blocks(blocks) => Some(blocks),
                MessageContent::Text(_) => None,
            })
            .flatten()
            .filter(|b| matches!(b, ContentBlock::Reasoning { signature: Some(s), .. } if !s.is_empty()))
            .count();
        Some(Divergence { at, messages: messages.len(), tokens_after: estimate_tokens(rest), thinking_after })
    }
}

/// What a history edit will cost the next request that applies it, from the
/// blocks it will send after the edit: the estimated tokens no cache reads,
/// and the signed thinking blocks a provider binding thinking to the
/// conversation drops.
pub fn edit_cost(after: &[kaijutsu_types::BlockSnapshot]) -> (u64, usize) {
    let tokens = estimate_tokens(&super::hydrate_from_blocks(after));
    let thinking = after
        .iter()
        .filter(|b| b.kind == kaijutsu_types::BlockKind::Thinking && super::hydrate::is_history_eligible(b))
        .filter(|b| b.signature.as_deref().is_some_and(|s| !s.is_empty()))
        .count();
    (tokens, thinking)
}

/// The sentence a history edit prints about its cost.
pub fn edit_cost_note(tokens: u64, thinking: usize) -> String {
    format!(
        "about {tokens} tokens after it are sent without a cache read, and {thinking} thinking \
         block(s) after it are dropped by a provider that binds thinking to the conversation"
    )
}

/// One message's fingerprint. An image counts by its content hash, not the
/// bytes a provider resolved for it, so resolving an image again is not a
/// change.
fn fingerprint(message: &Message) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (message.role as u8).hash(&mut h);
    message.turn_scoped.hash(&mut h);
    match &message.content {
        MessageContent::Text(text) => {
            0u8.hash(&mut h);
            text.hash(&mut h);
        }
        MessageContent::Blocks(blocks) => {
            1u8.hash(&mut h);
            for block in blocks {
                match block {
                    ContentBlock::Image { hash, media_type, .. } => {
                        "image".hash(&mut h);
                        hash.hash(&mut h);
                        media_type.hash(&mut h);
                    }
                    other => serde_json::to_string(other).unwrap_or_default().hash(&mut h),
                }
            }
        }
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reasoning() -> Message {
        Message {
            role: super::super::Role::Assistant,
            content: MessageContent::Blocks(vec![
                ContentBlock::Reasoning { text: "think".into(), signature: Some("sig".into()) },
                ContentBlock::Text { text: "Clack.".into() },
            ]),
            turn_scoped: false,
        }
    }

    fn convo() -> Vec<Message> {
        vec![Message::user("a"), reasoning(), Message::user("b"), reasoning(), Message::user("c")]
    }

    fn sent(messages: &[Message]) -> SentPrefix {
        SentPrefix::of(Some("stance"), &[], messages)
    }

    /// Appending is what every turn does; it costs no cache and drops no
    /// thinking.
    #[test]
    fn an_appended_request_does_not_diverge() {
        let before = convo();
        let mut after = before.clone();
        after.push(reasoning());
        after.push(Message::user("d"));
        assert_eq!(sent(&before).divergence(&sent(&after), &after), None);
    }

    /// A changed or removed earlier message names its position, the tokens
    /// re-sent from there, and the thinking blocks after it.
    #[test]
    fn a_changed_or_removed_message_names_where_and_what_follows() {
        let before = convo();
        let mut changed = before.clone();
        changed[2] = Message::user("b, edited");
        let d = sent(&before).divergence(&sent(&changed), &changed).expect("diverges");
        assert_eq!(d.at, Some(2));
        assert_eq!(d.messages, 5);
        assert_eq!(d.tokens_after, estimate_tokens(&changed[2..]));
        assert_eq!(d.thinking_after, 1);

        let mut removed = before.clone();
        removed.remove(1);
        let d = sent(&before).divergence(&sent(&removed), &removed).expect("diverges");
        assert_eq!(d.at, Some(1));
        assert_eq!(d.thinking_after, 1);
    }

    /// The system prompt and tools come before every message.
    #[test]
    fn a_changed_system_prompt_diverges_before_every_message() {
        let messages = convo();
        let d = sent(&messages)
            .divergence(&SentPrefix::of(Some("a new stance"), &[], &messages), &messages)
            .expect("diverges");
        assert_eq!(d.at, None);
        assert_eq!(d.tokens_after, estimate_tokens(&messages));
        assert_eq!(d.thinking_after, 2);
    }

    /// Dropping only the tail re-sends nothing, and an image resolved again
    /// is the same image.
    #[test]
    fn a_shorter_tail_or_a_resolved_image_does_not_diverge() {
        let before = convo();
        let shorter = before[..3].to_vec();
        assert_eq!(sent(&before).divergence(&sent(&shorter), &shorter), None);

        let image = |data: Option<&str>| Message {
            role: super::super::Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::Image {
                hash: "abc".into(),
                media_type: "image/png".into(),
                data_base64: data.map(str::to_string),
            }]),
            turn_scoped: false,
        };
        let unresolved = vec![image(None)];
        let resolved = vec![image(Some("iVBOR"))];
        assert_eq!(sent(&unresolved).divergence(&sent(&resolved), &resolved), None);
    }
}
