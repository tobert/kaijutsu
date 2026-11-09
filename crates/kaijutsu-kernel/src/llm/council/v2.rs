//! Turns on a council v2 server (gogi's `docs/council-api.md`).
//!
//! When the server's `GET /council/v2/identity` lists the turn's model as
//! a served model with `generate`, each turn is one generation that sends
//! the whole conversation: the system message,
//! the history with `snap` on the end of each user turn and run of tool
//! results, the tools, and whether the model thinks. The server keeps no
//! record of the chat. It starts from the longest snapshot it holds along
//! the messages, so a turn that extends the last one feeds only its new
//! messages, and a fork or an exclude reuses the prefix it keeps. The
//! client keeps nothing between turns: the kernel's history is the context.

use std::sync::Arc;

use gogi_proto::generation::{GenerationRequest, Sample};
use gogi_proto::identity::{Capability, ServedModel, ServerIdentity};
use gogi_proto::{Context, Role as V2Role};
use kaijutsu_council::CouncilError;

use super::reply::Reply;
use super::{build, Client, Stream, Turn};
use crate::llm::stream::BuildOpts;
use crate::llm::{LlmError, LlmResult, Message};

/// The server identity a council v2 reply reports its usage under.
pub(super) struct ReplyIdentity {
    pub(super) model: String,
    pub(super) weight_hash: String,
}

impl Client {
    /// The served model `model` names, which must list `generate`. The
    /// identity is read when none is cached, when the cached one is older
    /// than `v2_max_age`, or after a generation refused for its `thinking`
    /// ([`Client::forget_v2`]). A server without council v2 answers the
    /// identity with 404, 405, or 501, and is refused as one. A model the
    /// server does not serve by that name or alias, or serves without
    /// `generate`, is refused naming what it serves. The server answers a
    /// model that is not ready.
    pub(super) async fn council_v2(&self, turn: &Turn, model: &str) -> LlmResult<ServedModel> {
        let cell = {
            let mut cached = self.v2.lock();
            if cached.get().is_some_and(|(_, read)| read.elapsed() >= self.v2_max_age) {
                *cached = Arc::default();
            }
            cached.clone()
        };
        let (identity, _) = cell
            .get_or_try_init(|| async {
                let (_slot, identity) = self
                    .call(turn, false, |_| async {
                        self.client.council().identity().await.map_err(|e| match e.status() {
                            Some(404 | 405 | 501) => CouncilError::Request(format!(
                                "{e}; the server has no council v2 identity, so it cannot take turns. \
                                 Point the backend at a council v2 server"
                            )),
                            _ => e,
                        })
                    })
                    .await?;
                Ok::<_, LlmError>((identity, tokio::time::Instant::now()))
            })
            .await?;
        let served = served(&self.name, identity, model)?;
        if !served.has(Capability::Generate) {
            return Err(LlmError::InvalidRequest(format!(
                "council backend '{}': served model {} does not list generate, so it cannot take turns; \
                 the server serves {}. Name a model that lists generate",
                self.name,
                served.model,
                served_list(identity)
            )));
        }
        Ok(served.clone())
    }

    /// Drops the cached council v2 identity, so the next turn reads it
    /// again. A generation refused for its `thinking` calls this: the
    /// served model no longer declares what the identity said.
    pub(super) fn forget_v2(&self) {
        *self.v2.lock() = Arc::default();
    }

    /// A streamed turn as a council v2 generation after the whole
    /// conversation.
    pub(super) async fn stream_v2(
        &self,
        opts: BuildOpts,
        history: Vec<Message>,
        identity: ServedModel,
        turn: Turn,
    ) -> LlmResult<Stream> {
        let thinking = v2_thinking(&self.name, opts.effort.as_deref(), &identity)?;
        let messages = build::messages_v2(opts.system.as_deref(), &history)?;
        let tools = build::tools_v2(&opts.tools)
            .map_err(|e| LlmError::InvalidRequest(format!("council backend '{}': {e}", self.name)))?;
        if messages.last().is_none_or(|m| m.role == V2Role::Assistant) {
            return Err(LlmError::InvalidRequest(format!(
                "council backend '{}': the conversation is empty or ends with an assistant message; a generation \
                 replies after a user turn, tool results, or a system message",
                self.name
            )));
        }
        let max_tokens = v2_max_tokens(&self.name, opts.max_tokens, identity.limits.context_tokens)?;
        let sample = (opts.temperature.is_some() || opts.top_p.is_some())
            .then(|| Sample { temperature: opts.temperature, top_p: opts.top_p, ..Sample::default() });
        crate::llm::request_dump::record(
            opts.trace.as_deref().map(|t| t.summary_only()).as_ref(),
            &self.name, &opts.model, "", opts.system.as_deref(), &history, &serde_json::Value::Null,
        );
        let request = GenerationRequest {
            model: identity.model.clone(),
            context: Context { thinking, tools, queue: None, control: Vec::new(), messages },
            max_tokens,
            sample,
            stream: None,
            decisions: Vec::new(),
            class: None,
            priority: None,
            output: None,
            previews: None,
        };
        let started = std::time::Instant::now();
        let (slot, inner) = self
            .call(&turn, true, |_| async {
                let opened = self.client.council().generate_stream(&request, self.timeout).await;
                if opened.as_ref().is_err_and(CouncilError::refuses_thinking) {
                    self.forget_v2();
                }
                opened
            })
            .await?;
        Ok(Stream {
            name: self.name.clone(),
            inner: Some(inner),
            reply: Some(Reply::new(thinking)),
            pending: Default::default(),
            cancel: tokio_util::sync::CancellationToken::new(),
            finished: false,
            started,
            first_token: None,
            slot,
            v2: Some(ReplyIdentity { model: identity.model, weight_hash: identity.weight_hash }),
        })
    }
}

/// Whether a v2 turn on served model `model` thinks. The turn wants
/// thinking unless its effort is `none`. A model that declares `thinking`
/// thinks when the turn wants it, and any other model gets `false`. An
/// effort asked of a model without `thinking` is refused, not dropped.
fn v2_thinking(name: &str, effort: Option<&str>, model: &ServedModel) -> LlmResult<bool> {
    let wants = build::effort(effort)?;
    match (wants, model.has(Capability::Thinking)) {
        (Some(true), false) => Err(LlmError::InvalidRequest(format!(
            "council backend '{name}': served model {} does not declare thinking, so it takes no effort {:?}; \
             set effort none or leave it unset",
            model.model,
            effort.unwrap_or_default()
        ))),
        (wants, declared) => Ok(declared && wants != Some(false)),
    }
}

/// The served model `model` names, by its `model` or an alias.
fn served<'a>(name: &str, identity: &'a ServerIdentity, model: &str) -> LlmResult<&'a ServedModel> {
    identity.models.iter().find(|m| m.is_named(model)).ok_or_else(|| {
        LlmError::InvalidRequest(format!(
            "council backend '{name}': the council server serves no model named {model:?}; it serves {}. \
             Name one of them, or an alias, as the model",
            served_list(identity)
        ))
    })
}

/// The served models, each marked with whether it lists `generate`.
fn served_list(identity: &ServerIdentity) -> String {
    identity
        .models
        .iter()
        .map(|m| format!("{} {} generate", m.model, if m.has(Capability::Generate) { "with" } else { "without" }))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The `max_tokens` a generation sends: the asked-for amount, no more than
/// the server's `context_tokens`. The server answers a prompt with no room
/// left as `context_full`.
fn v2_max_tokens(name: &str, asked: u64, context_tokens: u64) -> LlmResult<u32> {
    if asked == 0 {
        return Err(LlmError::InvalidRequest(format!("council backend '{name}': max_tokens is 0")));
    }
    Ok(asked.min(context_tokens).min(u64::from(u32::MAX)) as u32)
}
