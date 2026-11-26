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

use gogi_proto::control::Control;
use gogi_proto::generation::{GenerationRequest, Sample};
use gogi_proto::identity::{Capability, ServedModel, ServerIdentity};
use gogi_proto::{Context, Role as CouncilRole};
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
    /// than `identity_max_age`, or after a generation refused for its `thinking`
    /// ([`Client::forget_identity`]). A server without council v2 answers the
    /// identity with 404, 405, or 501, and is refused as one. A model the
    /// server does not serve by that name or alias, or serves without
    /// `generate`, is refused naming what it serves. The server answers a
    /// model that is not ready. The flag says whether the server holds
    /// objects.
    pub(super) async fn served_model(&self, turn: &Turn, model: &str) -> LlmResult<(ServedModel, bool)> {
        let identity = self.read_identity(turn).await?;
        let served = served(&self.name, &identity, model, Capability::Generate)?;
        if !served.has(Capability::Generate) {
            return Err(LlmError::InvalidRequest(format!(
                "council backend '{}': served model {} does not list generate, so it cannot take turns; \
                 the server serves {}. Name a model that lists generate",
                self.name,
                served.model,
                served_list(&identity, Capability::Generate)
            )));
        }
        Ok((served.clone(), identity.has(Capability::Objects)))
    }

    /// The server's council v2 identity: the cached one, or one read when
    /// none is cached or the cached one is older than `identity_max_age`. A
    /// server without council v2 answers the identity with 404, 405, or 501,
    /// and is refused as one.
    pub(super) async fn read_identity(&self, turn: &Turn) -> LlmResult<ServerIdentity> {
        let cell = {
            let mut cached = self.identity.lock();
            if cached.get().is_some_and(|(_, read)| read.elapsed() >= self.identity_max_age) {
                *cached = Arc::default();
            }
            cached.clone()
        };
        let (identity, _) = cell
            .get_or_try_init(|| async {
                let (_slot, identity) = self
                    .call(turn, false, |_| async {
                        self.client.identity().await.map_err(|e| match e.status() {
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
        Ok(identity.clone())
    }

    /// Drops the cached council v2 identity, so the next turn reads it
    /// again. A generation refused for its `thinking` calls this: the
    /// served model no longer declares what the identity said.
    pub(super) fn forget_identity(&self) {
        *self.identity.lock() = Arc::default();
    }

    /// A streamed turn as a council v2 generation after the whole
    /// conversation.
    pub(super) async fn stream_generation(
        &self,
        opts: BuildOpts,
        history: Vec<Message>,
        (identity, holds_objects): (ServedModel, bool),
        turn: Turn,
    ) -> LlmResult<Stream> {
        check_controls(&self.name, &opts.controls, &identity)?;
        let thinking = wants_thinking(&self.name, opts.effort.as_deref(), &identity)?;
        let objects = self.place_media(&turn, &history, &identity, holds_objects).await?;
        let messages = build::messages(opts.system.as_deref(), &history, &objects)?;
        let tools = build::tools(&opts.tools)
            .map_err(|e| LlmError::InvalidRequest(format!("council backend '{}': {e}", self.name)))?;
        if messages.last().is_none_or(|m| m.role == CouncilRole::Assistant) {
            return Err(LlmError::InvalidRequest(format!(
                "council backend '{}': the conversation is empty or ends with an assistant message; a generation \
                 replies after a user turn, tool results, or a system message",
                self.name
            )));
        }
        let max_tokens = reply_max_tokens(&self.name, opts.required_max_tokens()?, identity.limits.context_tokens)?;
        let sample = (opts.temperature.is_some() || opts.top_p.is_some())
            .then(|| Sample { temperature: opts.temperature, top_p: opts.top_p, ..Sample::default() });

        let mut decisions = Vec::new();
        let mut ride_spec = None;
        use crate::vfs::VfsOps as _;
        if opts.ride_vibe && identity.has(Capability::Ride) {
            let vibe_path = kaijutsu_types::paths::config_path("council/vibe.json");
            let mut vibe_bytes = None;
            if let Some(vfs) = &opts.vfs {
                match vfs.read_all(std::path::Path::new(&vibe_path)).await {
                    Ok(b) => vibe_bytes = Some(b),
                    Err(e) if e.is_not_found() => {
                        let config_root = std::path::Path::new(kaijutsu_types::paths::CONFIG_ROOT);
                        if vfs.read_all(config_root).await.is_ok() || vfs.readdir(config_root).await.is_ok() {
                            // /config/kernel mounted, operator deleted vibe.json -> skip riding
                        } else {
                            vibe_bytes = Some(crate::config_seed::DEFAULT_COUNCIL_VIBE.as_bytes().to_vec());
                        }
                    }
                    Err(e) => {
                        tracing::warn!("failed to read council/vibe.json: {e}");
                    }
                }
            } else {
                vibe_bytes = Some(crate::config_seed::DEFAULT_COUNCIL_VIBE.as_bytes().to_vec());
            }

            if let Some(bytes) = vibe_bytes {
                if let Ok(text) = String::from_utf8(bytes) {
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
                        match gogi_proto::spec::parse_spec(&val, &gogi_proto::spec::SpecRules::default()) {
                            Ok(spec) => {
                                self.ensure_spec(&turn, &spec).await.map_err(|(_, e)| e)?;
                                decisions.push(gogi_proto::generation::RidingDecision {
                                    spec_id: spec.id(),
                                    ask: None,
                                    when: gogi_proto::generation::RideWhen::After,
                                });
                                ride_spec = Some(spec);
                            }
                            Err(e) => {
                                tracing::warn!("failed to parse council/vibe.json spec: {e:?}");
                            }
                        }
                    }
                }
            }
        }

        let request = GenerationRequest {
            model: identity.model.clone(),
            context: Context { thinking, tools, queue: None, control: opts.controls.clone(), messages },
            max_tokens,
            sample,
            stream: Some(true),
            decisions,
            class: None,
            priority: None,
            output: None,
            previews: None,
        };
        let started = std::time::Instant::now();
        let mut recorded = false;
        let client_self = self.clone();
        let ride_spec_clone = ride_spec;
        let (slot, inner) = self
            .call(&turn, true, |_| {
                // A `429` resend inside one call is the same request; it is written once.
                if !std::mem::replace(&mut recorded, true) {
                    crate::llm::request_dump::record(
                        opts.trace.as_deref(), &client_self.name, &opts.model, &client_self.base_url,
                        opts.system.as_deref(), &history, &request,
                    );
                }
                let client_ref = client_self.clone();
                let spec_ref = ride_spec_clone.clone();
                let turn_ref = turn.clone();
                let req_ref = request.clone();
                async move {
                    let mut opened = client_ref.client.generate_stream(&req_ref, client_ref.timeout).await;
                    if opened.as_ref().is_err_and(|e| e.status() == Some(404)) && !req_ref.decisions.is_empty() {
                        if let Some(spec) = &spec_ref {
                            client_ref.specs.lock().remove(&spec.id());
                            if client_ref.ensure_spec(&turn_ref, spec).await.is_ok() {
                                opened = client_ref.client.generate_stream(&req_ref, client_ref.timeout).await;
                            }
                        }
                    }
                    if opened.as_ref().is_err_and(CouncilError::refuses_thinking) {
                        client_ref.forget_identity();
                    }
                    opened
                }
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
            reply_identity: Some(ReplyIdentity { model: identity.model, weight_hash: identity.weight_hash }),
            live_council: opts.live_council,
            last_tps_tenths: None,
            entropies: Vec::new(),
        })
    }
}

impl Client {
    /// The object each media part of `history` goes as. A part goes as an
    /// object when the server holds objects and the served model reads its
    /// media type; the server is asked whether it holds the object, and it is
    /// put when it does not. Any other part gets the reason it is left out.
    /// A failed put fails the turn.
    pub(super) async fn place_media(&self, turn: &Turn, history: &[Message], model: &ServedModel, holds_objects: bool) -> LlmResult<build::Objects> {
        use base64::Engine as _;
        let mut placed = build::Objects::new();
        for (hash, mime, data) in media(history) {
            if placed.contains_key(hash) {
                continue;
            }
            // No bytes: the builder's marker says they were not found.
            let Some(data) = data else { continue };
            if let Some(why) = left_out(holds_objects, model, mime) {
                placed.insert(hash.to_string(), Err(why));
                continue;
            }
            let bytes = base64::engine::general_purpose::STANDARD.decode(data).map_err(|e| {
                LlmError::InvalidRequest(format!("council backend '{}': media {hash} holds bytes that are not base64: {e}", self.name))
            })?;
            let id = gogi_proto::content_id(&bytes);
            let (_slot, held) = self.call(turn, false, |_| self.client.has_object(&id)).await?;
            if !held {
                self.call(turn, false, |left| self.client.put_object(mime, bytes.clone(), left)).await?;
            }
            placed.insert(hash.to_string(), Ok(id));
        }
        Ok(placed)
    }
}

/// Why a media part of type `mime` is not sent to `model` as an object, or
/// `None` when it is: the server holds objects and the model reads the type.
fn left_out(holds_objects: bool, model: &ServedModel, mime: &str) -> Option<String> {
    if !holds_objects {
        Some("the server holds no objects".to_string())
    } else if !crate::llm::media_type_is_recorded(mime) {
        Some(crate::llm::UNRECORDED_MEDIA_TYPE.to_string())
    } else if !model.reads(mime) {
        Some(format!("{} does not read {mime}", model.model))
    } else {
        None
    }
}

impl Client {
    /// Why a media part of `media_type` would not go to the served model
    /// `model` as an object: the server holds no objects, the type is not
    /// recorded, or the model does not read it. Reads the identity, cached as
    /// for a turn, and sends nothing else.
    pub async fn media_refusal(&self, model: &str, media_type: &str) -> LlmResult<Option<String>> {
        let turn = self.turn();
        let (served, holds_objects) = self.served_model(&turn, model).await?;
        Ok(left_out(holds_objects, &served, media_type))
    }
}

/// The CAS hash, media type, and resolved base64 bytes of each media part a user
/// message shows, in order.
fn media(history: &[Message]) -> impl Iterator<Item = (&str, &str, Option<&str>)> {
    history.iter().filter(|m| m.role == crate::llm::Role::User).flat_map(|m| match &m.content {
        crate::llm::MessageContent::Blocks(blocks) => blocks.as_slice(),
        crate::llm::MessageContent::Text(_) => &[],
    }).filter_map(|b| match b {
        crate::llm::ContentBlock::Media { hash, media_type, data_base64 } => Some((hash.as_str(), media_type.as_str(), data_base64.as_deref())),
        _ => None,
    })
}

/// A control list needs a served model that declares `control_vectors`; one
/// that does not is refused the list, not sent the turn without it.
pub(super) fn check_controls(name: &str, controls: &[Control], model: &ServedModel) -> LlmResult<()> {
    if controls.is_empty() || model.has(Capability::ControlVectors) {
        return Ok(());
    }
    let declared = model
        .capabilities
        .iter()
        .map(|c| serde_json::to_value(c).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_else(|| format!("{c:?}")))
        .collect::<Vec<_>>()
        .join(", ");
    Err(LlmError::InvalidRequest(format!(
        "council backend '{name}': served model {} does not declare control_vectors, so it takes no control list, \
         and this context has {} control item(s); it declares {declared}. Run `kj context control clear`, \
         or name a model that declares control_vectors",
        model.model,
        controls.len()
    )))
}

/// Whether a turn on served model `model` thinks. The turn wants
/// thinking unless its effort is `none`. A model that declares `thinking`
/// thinks when the turn wants it, and any other model gets `false`. An
/// effort asked of a model without `thinking` is refused, not dropped.
fn wants_thinking(name: &str, effort: Option<&str>, model: &ServedModel) -> LlmResult<bool> {
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
pub(super) fn served<'a>(name: &str, identity: &'a ServerIdentity, model: &str, capability: Capability) -> LlmResult<&'a ServedModel> {
    identity.models.iter().find(|m| m.is_named(model)).ok_or_else(|| {
        LlmError::InvalidRequest(format!(
            "council backend '{name}': the council server serves no model named {model:?}; it serves {}. \
             Name one of them, or an alias, as the model",
            served_list(identity, capability)
        ))
    })
}

/// The served models, each marked with whether it lists `capability`.
pub(super) fn served_list(identity: &ServerIdentity, capability: Capability) -> String {
    let word = serde_json::to_value(capability).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_else(|| format!("{capability:?}"));
    identity
        .models
        .iter()
        .map(|m| format!("{} {} {word}", m.model, if m.has(capability) { "with" } else { "without" }))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The `max_tokens` a generation sends: the asked-for amount, no more than
/// the server's `context_tokens`. The server answers a prompt with no room
/// left as `context_full`.
fn reply_max_tokens(name: &str, asked: u64, context_tokens: u64) -> LlmResult<u32> {
    if asked == 0 {
        return Err(LlmError::InvalidRequest(format!("council backend '{name}': max_tokens is 0")));
    }
    Ok(asked.min(context_tokens).min(u64::from(u32::MAX)) as u32)
}
