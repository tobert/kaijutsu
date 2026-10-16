//! Held conversations on a council v2 server (`docs/council-api-v2.md`).
//!
//! When the server's `GET /council/v2/identity` lists `generate`, each turn
//! of a kernel context is a generation on a held context under that
//! context's id. The client remembers, per context, the messages the server
//! holds and the snapshots it reported. A turn whose messages extend that
//! record appends only its new user turn or tool results; any other turn
//! sends the whole context with a `PUT` first, and the server feeds from
//! the first difference.
//!
//! A context the client has not held yet, forked from one it has, starts
//! with `fork: {context: parent, at}`: `at` is the parent's last snapshot
//! before the first message the two differ in, so an exclude or an edit
//! takes effect at the fork and the parent is never changed
//! (`docs/conversation-session.md`).
//!
//! The record lives in memory, for the client's lifetime. After a restart
//! the first turn of each context is a `PUT`.

use std::collections::HashMap;
use std::sync::Arc;

use kaijutsu_mk::council::wire::{Capability, ServerIdentity, SnapshotId};
use kaijutsu_mk::council_v2::wire::{
    ContextPut, Fork, GenerationDone, GenerationRequest, Message as V2Message, ReplyToolCall, Role as V2Role,
    SnapshotInfo, ToolCall as V2ToolCall,
};
use kaijutsu_mk::generate::{Sample, Tool};
use kaijutsu_types::ContextId;

use super::reply::Reply;
use super::{build, Client, Inner, Stream, Turn};
use crate::llm::stream::{BuildOpts, HeldContext};
use crate::llm::{LlmError, LlmResult, Message};

/// What the server holds for one kernel context.
#[derive(Clone, Debug)]
pub(super) struct HeldChat {
    /// The messages, as sent and as the server wrote its replies.
    pub(super) messages: Vec<V2Message>,
    pub(super) tools: Vec<Tool>,
    pub(super) thinking: Option<bool>,
    pub(super) head: SnapshotId,
    pub(super) snapshots: Vec<SnapshotInfo>,
}

impl HeldChat {
    /// Whether a turn with these settings and messages can append to this
    /// record: the same tools and thinking, and messages that extend it by
    /// a tail with no assistant message.
    fn extends_into(&self, tools: &[Tool], thinking: Option<bool>, messages: &[V2Message], split: usize) -> bool {
        self.tools == tools
            && self.thinking == thinking
            && self.messages.len() >= split
            && self.messages.len() < messages.len()
            && messages[..self.messages.len()] == self.messages[..]
    }

    /// The last snapshot before the first message `messages` differs in,
    /// when the settings match.
    fn fork_point(&self, tools: &[Tool], thinking: Option<bool>, messages: &[V2Message]) -> Option<SnapshotId> {
        if self.tools != tools || self.thinking != thinking {
            return None;
        }
        let same = self.messages.iter().zip(messages).take_while(|(a, b)| a == b).count();
        self.snapshots
            .iter()
            .filter(|s| usize::try_from(s.message).is_ok_and(|m| m < same))
            .max_by_key(|s| s.message)
            .map(|s| s.id.clone())
    }
}

/// Each kernel context's held conversation.
pub(super) type HeldChats = Arc<parking_lot::Mutex<HashMap<ContextId, HeldChat>>>;

/// What a held stream records when its reply is done.
pub(super) struct HeldUpdate {
    pub(super) chats: HeldChats,
    pub(super) context: ContextId,
    /// The record with this turn's appended messages, before the reply.
    pub(super) record: HeldChat,
    pub(super) model: String,
    pub(super) weight_hash: String,
}

impl HeldUpdate {
    /// Records the reply as the context's new head. A reply with a call
    /// that did not parse is forgotten instead: the kernel replays that call
    /// in another form, so the next turn sends the whole context.
    pub(super) fn done(self, done: &GenerationDone) {
        let mut record = self.record;
        let mut calls = Vec::new();
        for call in &done.tool_calls {
            match call {
                ReplyToolCall::Parsed { id, name, arguments } => {
                    calls.push(V2ToolCall { id: id.clone(), name: name.clone(), arguments: arguments.clone() })
                }
                ReplyToolCall::Unparsed { .. } => {
                    self.chats.lock().remove(&self.context);
                    return;
                }
            }
        }
        record.messages.push(V2Message {
            content: Some(done.content.clone()).filter(|c| !c.is_empty()),
            reasoning: Some(done.reasoning.clone()).filter(|r| !r.is_empty()),
            tool_calls: calls,
            ..V2Message::assistant("")
        });
        record.head = done.head.clone();
        record.snapshots = done.snapshots.clone();
        self.chats.lock().insert(self.context, record);
    }

    /// The reply did not finish, so what the server holds is unknown.
    pub(super) fn failed(self) {
        self.chats.lock().remove(&self.context);
    }
}

impl Client {
    /// The server's council v2 identity when it lists `generate`, read once.
    /// A server without v2 answers 404 and keeps the `/mk/v1` path.
    pub(super) async fn council_v2(&self, turn: &Turn) -> LlmResult<Option<ServerIdentity>> {
        let identity = self
            .v2
            .get_or_try_init(|| async {
                let (_slot, identity) = self
                    .call(turn, false, |_| async {
                        match self.mk.v2().identity().await {
                            Ok(identity) => Ok(Some(identity)),
                            Err(e) if matches!(e.status(), Some(404 | 405 | 501)) => Ok(None),
                            Err(e) => Err(e),
                        }
                    })
                    .await?;
                let usable = identity.filter(|i| i.capabilities.contains(&Capability::Generate));
                tracing::info!(backend = %self.name, council_v2 = usable.is_some(), "mk: chats are held contexts when council v2 lists generate");
                Ok::<_, LlmError>(usable)
            })
            .await?;
        Ok(identity.clone())
    }

    /// A streamed turn on the context `held`, as a council v2 generation.
    pub(super) async fn stream_held(
        &self,
        opts: BuildOpts,
        history: Vec<Message>,
        held: HeldContext,
        identity: ServerIdentity,
        turn: Turn,
    ) -> LlmResult<Stream> {
        let (thinking, _) = build::effort(opts.effort.as_deref())?;
        let messages = build::messages_v2(opts.system.as_deref(), &history)?;
        let tools = build::tools(&opts.tools)?;
        // The new turn is everything after the last assistant message.
        let split = messages
            .iter()
            .rposition(|m| m.role == V2Role::Assistant)
            .map(|i| i + 1)
            .unwrap_or(usize::from(messages.first().is_some_and(|m| m.role == V2Role::System)));
        if split == messages.len() {
            return Err(LlmError::InvalidRequest(format!(
                "mk backend '{}': the conversation ends with an assistant message; a generation appends a user \
                 turn or tool results",
                self.name
            )));
        }
        let max_tokens = held_max_tokens(&self.name, opts.max_tokens, identity.limits.context_tokens)?;
        let sample = (opts.temperature.is_some() || opts.top_p.is_some())
            .then(|| Sample { temperature: opts.temperature, top_p: opts.top_p, ..Sample::default() });
        crate::llm::request_dump::record(
            opts.trace.as_deref().map(|t| t.summary_only()).as_ref(),
            &self.name, &opts.model, "", opts.system.as_deref(), &history, &serde_json::Value::Null,
        );

        let known = self.held.lock().get(&held.context).cloned();
        // `fresh`: the server holds `base` because this turn just sent it.
        let (mut base, mut fresh) = match known.clone().filter(|h| h.extends_into(&tools, thinking, &messages, split)) {
            Some(h) => (h, false),
            None => (self.put_held(&turn, &identity, held, known.is_none(), &tools, thinking, &messages[..split]).await?, true),
        };
        let started = std::time::Instant::now();
        let (slot, inner) = loop {
            let request = GenerationRequest {
                context: held.context.to_string(),
                messages: messages[base.messages.len()..].to_vec(),
                max_tokens: Some(max_tokens),
                sample: sample.clone(),
            };
            let opened = self
                .call(&turn, true, |_| async {
                    match self.mk.v2().generate_stream(&request, self.timeout).await {
                        Err(e) if e.status() == Some(404) => Ok(None),
                        other => other.map(Some),
                    }
                })
                .await;
            match opened {
                Ok((slot, Some(inner))) => break (slot, inner),
                // The server no longer holds the context: send it whole.
                // The 404's slot goes first: the PUT takes one of its own,
                // and a backend of one slot would wait on itself.
                Ok((slot, None)) if !fresh => {
                    drop(slot);
                    self.held.lock().remove(&held.context);
                    base = self.put_held(&turn, &identity, held, false, &tools, thinking, &messages[..split]).await?;
                    fresh = true;
                }
                Ok((_, None)) => {
                    self.held.lock().remove(&held.context);
                    return Err(LlmError::ApiError(format!(
                        "mk backend '{}': the server answered 404 for context {} right after holding it",
                        self.name, held.context
                    )));
                }
                Err(e) => {
                    if !matches!(e, LlmError::ContextFull(_)) {
                        self.held.lock().remove(&held.context);
                    }
                    return Err(e);
                }
            }
        };
        let mut record = base;
        record.messages.extend_from_slice(&messages[record.messages.len()..]);
        Ok(Stream {
            name: self.name.clone(),
            inner: Some(Inner::Held(inner)),
            reply: Some(Reply::new(thinking.unwrap_or(true), !tools.is_empty())),
            pending: Default::default(),
            cancel: tokio_util::sync::CancellationToken::new(),
            finished: false,
            started,
            first_token: None,
            slot,
            held: Some(Box::new(HeldUpdate {
                chats: self.held.clone(),
                context: held.context,
                record,
                model: identity.model,
                weight_hash: identity.weight_hash,
            })),
        })
    }

    /// Sends `prefix` as the whole context and returns the new record. A
    /// context `new` to this client, forked from one it holds, asserts the
    /// fork when the server lists `fork`; a fork whose snapshot is gone
    /// (409) is sent again without it.
    #[allow(clippy::too_many_arguments)]
    async fn put_held(
        &self,
        turn: &Turn,
        identity: &ServerIdentity,
        held: HeldContext,
        new: bool,
        tools: &[Tool],
        thinking: Option<bool>,
        prefix: &[V2Message],
    ) -> LlmResult<HeldChat> {
        let fork = match held.forked_from {
            Some(parent) if new && identity.capabilities.contains(&Capability::Fork) => self
                .held
                .lock()
                .get(&parent)
                .and_then(|p| p.fork_point(tools, thinking, prefix))
                .map(|at| Fork { context: parent.to_string(), at }),
            _ => None,
        };
        let mut body = ContextPut {
            thinking,
            tools: tools.to_vec(),
            messages: prefix.to_vec(),
            fork,
            ..ContextPut::default()
        };
        let id = held.context.to_string();
        let put = loop {
            let forked = body.fork.is_some();
            let (_slot, answered) = self
                .call(turn, false, |_| async {
                    match self.mk.v2().put_context(&id, &body, None).await {
                        Err(e) if forked && e.status() == Some(409) => Ok(Err(e)),
                        other => other.map(Ok),
                    }
                })
                .await?;
            match answered {
                Ok(put) => break put,
                Err(gone) => {
                    tracing::info!(backend = %self.name, context = %held.context, error = %gone, "mk: the fork's snapshot is gone; the context is sent whole");
                    body.fork = None;
                }
            }
        };
        let record = HeldChat {
            messages: prefix.to_vec(),
            tools: tools.to_vec(),
            thinking,
            head: put.head,
            snapshots: put.snapshots,
        };
        self.held.lock().insert(held.context, record.clone());
        Ok(record)
    }
}

/// The `max_tokens` a generation sends: the asked-for amount, no more than
/// the server's `context_tokens`. The server answers a prompt with no room
/// left as `context_full`.
fn held_max_tokens(name: &str, asked: u64, context_tokens: u64) -> LlmResult<u32> {
    if asked == 0 {
        return Err(LlmError::InvalidRequest(format!("mk backend '{name}': max_tokens is 0")));
    }
    Ok(asked.min(context_tokens).min(u64::from(u32::MAX)) as u32)
}

/// The context's held record, for tests.
#[cfg(test)]
pub(super) fn record(client: &Client, context: ContextId) -> Option<HeldChat> {
    client.held.lock().get(&context).cloned()
}
