//! Turns on a council v2 server (gogi's `docs/council-api-v2.md`).
//!
//! When the server's `GET /council/v2/identity` lists the turn's model as
//! a served model with `generate`, each turn is one generation that sends the whole conversation: the system message,
//! the history with `snap` on the end of each user turn and run of tool
//! results, the tools, and whether the model thinks. The server keeps no
//! record of the chat. It starts from the longest snapshot it holds along
//! the messages, so a turn that extends the last one feeds only its new
//! messages, and a fork or an exclude reuses the prefix it keeps. The
//! client keeps nothing between turns: the kernel's history is the context.

use gogi_proto::generation::{GenerationRequest, Sample};
use gogi_proto::identity::{Capability, ServedModel, ServerIdentity};
use gogi_proto::{Context, Role as V2Role};

use super::reply::Reply;
use super::{build, Client, Inner, Stream, Turn};
use crate::llm::stream::BuildOpts;
use crate::llm::{LlmError, LlmResult, Message};

/// The server identity a council v2 reply reports its usage under.
pub(super) struct ReplyIdentity {
    pub(super) model: String,
    pub(super) weight_hash: String,
}

impl Client {
    /// The served model `model` names on a council v2 server, when it lists
    /// `generate`. The identity is read once. A server without v2 answers
    /// 404, and it and a model without `generate` keep the `/mk/v1` path. A
    /// v2 server that serves no model by that name or alias is an error
    /// naming the ones it serves. The server answers a model that is not
    /// ready.
    pub(super) async fn council_v2(&self, turn: &Turn, model: &str) -> LlmResult<Option<ServedModel>> {
        let identity = self
            .v2
            .get_or_try_init(|| async {
                let (_slot, identity) = self
                    .call(turn, false, |_| async {
                        match self.mk.council().identity().await {
                            Ok(identity) => Ok(Some(identity)),
                            Err(e) if matches!(e.status(), Some(404 | 405 | 501)) => Ok(None),
                            Err(e) => Err(e),
                        }
                    })
                    .await?;
                tracing::info!(backend = %self.name, council_v2 = identity.is_some(), "mk: turns are council v2 generations when council v2 lists the model with generate");
                Ok::<_, LlmError>(identity)
            })
            .await?;
        let Some(identity) = identity else { return Ok(None) };
        let served = served(&self.name, identity, model)?;
        Ok(served.has(Capability::Generate).then(|| served.clone()))
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
        let (thinking, _) = build::effort(opts.effort.as_deref())?;
        let messages = build::messages_v2(opts.system.as_deref(), &history)?;
        let tools = build::tools(&opts.tools)?
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| LlmError::InvalidRequest(format!("mk backend '{}': a tool does not serialize: {e}", self.name)))?;
        if messages.last().is_none_or(|m| m.role == V2Role::Assistant) {
            return Err(LlmError::InvalidRequest(format!(
                "mk backend '{}': the conversation is empty or ends with an assistant message; a generation \
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
        let has_tools = !tools.is_empty();
        let request = GenerationRequest {
            model: Some(identity.model.clone()),
            context: Context { thinking, tools, queue: None, messages },
            max_tokens: Some(max_tokens),
            sample,
            stream: None,
            decisions: Vec::new(),
            class: None,
            priority: None,
        };
        let started = std::time::Instant::now();
        let (slot, inner) = self.call(&turn, true, |_| async { self.mk.council().generate_stream(&request, self.timeout).await }).await?;
        Ok(Stream {
            name: self.name.clone(),
            inner: Some(Inner::V2(inner)),
            reply: Some(Reply::new(thinking.unwrap_or(true), has_tools)),
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

/// The served model `model` names, by its `model` or an alias.
fn served<'a>(name: &str, identity: &'a ServerIdentity, model: &str) -> LlmResult<&'a ServedModel> {
    identity.models.iter().find(|m| m.is_named(model)).ok_or_else(|| {
        let names: Vec<&str> = identity.models.iter().map(|m| m.model.as_str()).collect();
        LlmError::InvalidRequest(format!(
            "mk backend '{name}': the council server serves no model named {model:?}; it serves {}. \
             Name one of them, or an alias, as the model",
            names.join(", ")
        ))
    })
}

/// The `max_tokens` a generation sends: the asked-for amount, no more than
/// the server's `context_tokens`. The server answers a prompt with no room
/// left as `context_full`.
fn v2_max_tokens(name: &str, asked: u64, context_tokens: u64) -> LlmResult<u32> {
    if asked == 0 {
        return Err(LlmError::InvalidRequest(format!("mk backend '{name}': max_tokens is 0")));
    }
    Ok(asked.min(context_tokens).min(u64::from(u32::MAX)) as u32)
}
