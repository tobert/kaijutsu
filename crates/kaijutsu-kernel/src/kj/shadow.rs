//! `kj shadow` — the context shadows of a context (`docs/shadows.md`).
//!
//! ```text
//! kj shadow list [--context <ctx>]
//! kj shadow describe <block> [--context <ctx>]
//! ```
//!
//! `describe` writes a text description of an image block into the
//! description shadow of the block's context: a child that the source's own
//! performer plays, whose `model` slot is the performer's `image` slot. A
//! description is a rendition block in the shadow, linked to the source block
//! and indexed by the media's CAS hash, the kind, and the maker
//! (`<backend>/<model>`), so a second request for the same image by the same
//! maker, from any context, copies the first rendition and calls no model.

use std::time::Duration;

use clap::{Parser, Subcommand};
use kaijutsu_types::{BlockId, BlockKind, ContentType, ContextId, Role, Status};

use crate::kernel_db::{ContextRow, IndexKey, Rendition, DESCRIBE_SHADOW};
use crate::llm::{ContentBlock, Message, MessageContent};

use super::effect::{Classify, Effect};
use super::refs::resolve_context_arg;
use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};

/// How long `kj shadow describe` waits for the model's description. A request
/// that has not answered by then is an error and writes no rendition.
pub(crate) const DESCRIBE_TIMEOUT: Duration = Duration::from_secs(120);

/// Serializes the check-then-create of a description shadow, so two requests
/// for one source make one shadow.
static CREATE: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

#[derive(Parser, Debug)]
#[command(
    name = "shadow",
    about = "List a context's shadows, or ask for a description of its images",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct ShadowArgs {
    #[command(subcommand)]
    command: ShadowCommand,
}

#[derive(Subcommand, Debug)]
enum ShadowCommand {
    /// List the live shadows of a context: kind, id, performer, model, and how
    /// many renditions each holds.
    #[command(alias = "ls")]
    List {
        /// Source context: . (default) | .parent | <label> | <hex prefix>
        #[arg(long, short = 'c')]
        context: Option<String>,
    },
    /// Write a text description of an image block, and print it with the id
    /// of the rendition block that holds it. The block is a media block of the
    /// context whose CAS type is image/*. The context's performer needs an
    /// `image` slot (`kj character slot set <character> image
    /// <backend>/<model> --max-tokens <n>`). A description of the same
    /// image by the same model that any context already holds is copied
    /// without a model call, and the output says it was reused. Waits at most
    /// 120 seconds for the model.
    Describe {
        /// Block id: the full context_principal_seq id, or `<principal8>#<seq>`
        /// as `kj block list` prints it. In kaish, quote the short form,
        /// since kaish reads `#` as a comment
        block: String,
        /// Context the block is in: . (default) | .parent | <label> | <hex prefix>
        #[arg(long, short = 'c')]
        context: Option<String>,
    },
}

impl Classify for ShadowArgs {
    fn effect(&self) -> Effect {
        match &self.command {
            ShadowCommand::List { .. } => Effect::Read,
            // Makes the shadow if needed, writes blocks and index rows, and
            // spends a model call; all of it can be archived.
            ShadowCommand::Describe { .. } => Effect::Write,
        }
    }
}

/// What a `describe` request needs to check before making a shadow.
struct DescribeSlot {
    backend: String,
    model: String,
    character: String,
    existing_shadow: Option<String>,
    requires_max_tokens: bool,
    max_tokens: Option<i64>,
}

/// What a `describe` request produced.
struct Described {
    text: String,
    rendition: BlockId,
    shadow: ContextId,
    maker: String,
    media_type: String,
    hash: String,
    /// The text came from a rendition that already existed.
    reused: bool,
}

impl KjDispatcher {
    pub(crate) async fn dispatch_shadow(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<ShadowArgs>();
        }
        let parsed = match ShadowArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj shadow: {e}"));
            }
        };
        match parsed.command {
            ShadowCommand::List { context } => self.shadow_list(context.as_deref(), caller),
            ShadowCommand::Describe { block, context } => {
                // A description is a new block, so it takes the capability
                // `kj block create` takes.
                let cap = crate::mcp::Capability::Tool {
                    instance: crate::mcp::InstanceId::new("builtin.block"),
                    tool: "block_create".to_string(),
                };
                if let Err(denied) = self.require_cap(caller, cap, "shadow") {
                    return denied;
                }
                match self.describe_block(&block, context.as_deref(), caller, DESCRIBE_TIMEOUT).await {
                    Ok(done) => describe_result(&done),
                    Err(e) => KjResult::Err(format!("kj shadow describe: {e}")),
                }
            }
        }
    }

    fn shadow_list(&self, context: Option<&str>, caller: &KjCaller) -> KjResult {
        let db = self.kernel_db().lock();
        let source = match resolve_context_arg(context, caller, &db) {
            Ok(id) => id,
            Err(e) => return KjResult::Err(format!("kj shadow list: {e}")),
        };
        let shadows = match db.shadows_of(source) {
            Ok(s) => s,
            Err(e) => return KjResult::Err(format!("kj shadow list: {e}")),
        };
        if shadows.is_empty() {
            return KjResult::ok(format!("{} has no shadows\n", db.context_name(source)));
        }
        let mut text = String::new();
        let mut data = Vec::new();
        for (row, kind) in &shadows {
            let performer = row.performer.map_or_else(|| "(none)".to_string(), |p| db.name_for(p));
            let model = match db.context_slot(row.context_id, crate::model_resolution::MODEL) {
                Ok(Some(slot)) => match db.get_backend(slot.backend_id) {
                    Ok(Some(backend)) => format!("{}/{}", backend.name, slot.model),
                    Ok(None) => format!("(backend {} missing)/{}", slot.backend_id, slot.model),
                    Err(e) => return KjResult::Err(format!("kj shadow list: {e}")),
                },
                Ok(None) => "(none)".to_string(),
                Err(e) => return KjResult::Err(format!("kj shadow list: {e}")),
            };
            let renditions = match db.rendition_count(row.context_id) {
                Ok(n) => n,
                Err(e) => return KjResult::Err(format!("kj shadow list: {e}")),
            };
            text.push_str(&format!(
                "{kind:<9} {}  performer {performer}  model {model}  {renditions} rendition(s)\n",
                row.context_id.to_hex()
            ));
            data.push(serde_json::json!({
                "kind": kind,
                "context_id": row.context_id.to_hex(),
                "label": row.label,
                "performer": performer,
                "model": model,
                "renditions": renditions,
            }));
        }
        KjResult::ok_with_data(text, serde_json::Value::Array(data))
    }

    /// The whole of `kj shadow describe`.
    async fn describe_block(
        &self,
        block: &str,
        context: Option<&str>,
        caller: &KjCaller,
        timeout: Duration,
    ) -> Result<Described, String> {
        let id = self.resolve_block_id(block, context, caller)?;
        let source = id.context_id;
        if context.is_some() {
            let named = {
                let db = self.kernel_db().lock();
                resolve_context_arg(context, caller, &db)?
            };
            if named != source {
                return Err(format!("block {} is in context {}, not {}", id.to_key(), source.short(), named.short()));
            }
        }
        let (hash, media_type) = self.describable_media(id)?;
        let hash_text = hash.to_string();
        let slot = self.describe_preflight(source)?;
        let maker = format!("{}/{}", slot.backend, slot.model);
        let key = IndexKey::new(&hash_text, DESCRIBE_SHADOW, &maker);

        let hit = self.kernel_db().lock().indexed_rendition(&key).map_err(|e| e.to_string())?;
        if let Some(earlier) = hit {
            let text = self.rendition_text(earlier)?;
            let shadow = self.ensure_description_shadow(source)?;
            let performer = shadow.performer.ok_or_else(|| format!("description shadow {} has no performer", shadow.context_id.short()))?;
            let held_here = earlier.context_id == shadow.context_id
                && self
                    .kernel_db()
                    .lock()
                    .renditions_of(shadow.context_id)
                    .map_err(|e| e.to_string())?
                    .iter()
                    .any(|r| r.block == earlier && r.source == id);
            let rendition = if held_here {
                earlier
            } else {
                self.write_rendition(shadow.context_id, performer, &text, id, None)?.0
            };
            return Ok(Described { text, rendition, shadow: shadow.context_id, maker, media_type, hash: hash_text, reused: true });
        }

        if slot.requires_max_tokens
            && slot.max_tokens.and_then(|v| u64::try_from(v).ok()).filter(|m| *m > 0).is_none()
        {
            let archive_note = match &slot.existing_shadow {
                Some(ctx) => format!(", then archive {ctx} so the next request makes a new shadow"),
                None => String::new(),
            };
            return Err(format!(
                "{} slot {} has no max_tokens, which backend {} requires; `kj character slot set {} image {maker} --max-tokens <n>`{archive_note}",
                slot.existing_shadow.as_deref().unwrap_or(&slot.character),
                crate::model_resolution::IMAGE,
                slot.backend,
                slot.character,
            ));
        }
        let provider = self.kernel.llm().read().await.get(&slot.backend).ok_or_else(|| {
            format!(
                "backend '{}' of the image slot is not registered (missing key? see `kj backend show {}`)",
                slot.backend, slot.backend
            )
        })?;
        if let Some(why) = provider.media_refusal(&slot.model, &media_type).await.map_err(|e| e.to_string())? {
            return Err(format!(
                "{maker} cannot read {media_type} ({why}), so the request would send a marker instead of the media. No model was called"
            ));
        }
        let instruction = crate::config_seed::load_description_prompt(self.kernel().vfs().as_ref())
            .await
            .map_err(|e| format!("description prompt: {e}"))?;
        let mut messages = vec![Message {
            role: crate::llm::Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::Media {
                hash: hash_text.clone(),
                media_type: media_type.clone(),
                data_base64: None,
            }]),
            turn_scoped: false,
        }];
        let cas: std::sync::Arc<dyn kaijutsu_cas::ContentStore> = self.kernel().cas().clone();
        crate::llm::resolve_media_blocks_from_cas(&mut messages, cas, None).await;
        let resolved = matches!(
            &messages[0].content,
            MessageContent::Blocks(parts) if matches!(parts.first(), Some(ContentBlock::Media { data_base64: Some(d), .. }) if !d.is_empty())
        );
        if !resolved {
            return Err(format!("the bytes of {hash_text} were not found in CAS, so there is nothing to describe"));
        }

        let shadow = self.ensure_description_shadow(source)?;
        let bound = {
            let db = self.kernel_db().lock();
            crate::model_resolution::role_slot(&db, shadow.context_id, crate::model_resolution::MODEL)
                .map_err(|e| format!("could not read the description shadow's model: {e}"))?
        };
        let bound = match bound {
            crate::model_resolution::RoleSlot::Slot(bound) => bound,
            crate::model_resolution::RoleSlot::None(none) => return Err(none.refusal_as("cannot describe")),
        };
        let max_tokens = bound.request_max_tokens(crate::model_resolution::IMAGE).map_err(|error| {
            format!(
                "{error}; `kj character slot set {} image {maker} --max-tokens <n>`, \
                 then archive {} so the next request makes a new shadow",
                bound.character, bound.context
            )
        })?;
        let performer = shadow.performer.ok_or_else(|| format!("description shadow {} has no performer", shadow.context_id.short()))?;
        let reply = self.call_describer(&provider, &bound, shadow.context_id, performer, max_tokens, &instruction, messages, timeout).await?;
        let text = reply.trim().to_string();
        // A request that made the same description at the same time may have
        // taken the key first; this one is then linked and not indexed.
        let (rendition, _) = self.write_rendition(shadow.context_id, performer, &text, id, Some(&key))?;
        Ok(Described { text, rendition, shadow: shadow.context_id, maker, media_type, hash: hash_text, reused: false })
    }

    /// Resolve the `image` slot (or existing shadow's `model` slot) for
    /// `source` without creating a shadow.
    fn describe_preflight(&self, source: ContextId) -> Result<DescribeSlot, String> {
        let db = self.kernel_db().lock();
        let source_row = db
            .get_context(source)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("context {} has no row", source.short()))?;
        if source_row.is_archived() {
            return Err(format!(
                "{} is archived, and a shadow of it would be a live context under an archived senpai. \
                 Bring it back first with `kj context promote {}`",
                db.context_name(source),
                source.to_hex()
            ));
        }
        let performer = source_row.performer.ok_or_else(|| {
            format!("{} has no performer, so it has no image slot", db.context_name(source))
        })?;
        let sheet = db
            .get_character(performer)
            .map_err(|e| e.to_string())?
            .filter(|sheet| sheet.retired_at.is_none())
            .ok_or_else(|| format!("the performer of {} is not a live character", db.context_name(source)))?;
        if let Some(existing) = db.live_shadow(source, DESCRIBE_SHADOW).map_err(|e| e.to_string())? {
            let bound = crate::model_resolution::role_slot(&db, existing.context_id, crate::model_resolution::MODEL)
                .map_err(|e| format!("could not read the description shadow's model: {e}"))?;
            return match bound {
                crate::model_resolution::RoleSlot::Slot(bound) => Ok(DescribeSlot {
                    backend: bound.backend.clone(),
                    model: bound.slot.model.clone(),
                    character: bound.character.clone(),
                    existing_shadow: Some(bound.context.clone()),
                    requires_max_tokens: bound.kind().sends_output_limit(),
                    max_tokens: bound.slot.max_tokens,
                }),
                crate::model_resolution::RoleSlot::None(none) => Err(none.refusal_as("cannot describe")),
            };
        }
        let slot = db
            .get_character_slot(performer, crate::model_resolution::IMAGE)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| {
                format!(
                    "{} cannot get a description: its performer {} has no `image` slot. \
                     Fix: `kj character slot set {} image <backend>/<model> --max-tokens <n>`",
                    db.context_name(source),
                    sheet.name,
                    sheet.name
                )
            })?;
        let backend = db
            .get_backend(slot.backend_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("character {} image slot names a backend that does not exist", sheet.name))?;
        let kind = crate::llm::BackendKind::parse(&backend.kind).map_err(|e| e.to_string())?;
        let model_row = db
            .list_backend_models(slot.backend_id)
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|m| m.model_id == slot.model);
        let max_tokens = slot.max_tokens.or_else(|| model_row.and_then(|m| m.tunables.max_tokens));
        Ok(DescribeSlot {
            backend: backend.name,
            model: slot.model,
            character: sheet.name,
            existing_shadow: None,
            requires_max_tokens: kind.sends_output_limit(),
            max_tokens,
        })
    }

    /// The CAS hash and recorded media type of `id`, a media block whose
    /// object is an image or video, or why it is not one.
    fn describable_media(&self, id: BlockId) -> Result<(kaijutsu_cas::ContentHash, String), String> {
        let key = id.to_key();
        let snapshots = self.blocks.non_draft_snapshots(id.context_id).map_err(|e| e.to_string())?;
        let snap = snapshots.into_iter().find(|b| b.id == id).ok_or_else(|| format!("block {key} not found"))?;
        if snap.content_type != ContentType::Media {
            return Err(format!(
                "block {key} is a {} block of type {}, not a media block; describe takes an image or video block",
                snap.kind.as_str(),
                snap.content_type.as_mime()
            ));
        }
        let hash = snap
            .content
            .trim()
            .parse::<kaijutsu_cas::ContentHash>()
            .map_err(|_| format!("block {key} is a media block whose content {:?} is not a CAS hash", snap.content))?;
        let reference = kaijutsu_cas::ContentStore::inspect(self.kernel().cas().as_ref(), &hash)
            .map_err(|e| format!("reading the CAS record of {hash}: {e}"))?
            .ok_or_else(|| format!("CAS has no object {hash}, which block {key} names"))?;
        let media_type = reference.mime_type;
        if !media_type.starts_with("image/") {
            return Err(format!(
                "block {key} holds {media_type}, which describe does not read; it takes image/* (video waits for a `video` slot)"
            ));
        }
        Ok((hash, media_type))
    }

    /// The live description shadow of `source`, made when it has none. The
    /// source's performer needs an `image` slot; one with none refuses and
    /// names the verb that sets it.
    fn ensure_description_shadow(&self, source: ContextId) -> Result<ContextRow, String> {
        let _create = CREATE.lock();
        enum State {
            Existing(ContextRow),
            Created(ContextRow),
        }
        let state = {
            let db = self.kernel_db().lock();
            let source_row = db
                .get_context(source)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("context {} has no row", source.short()))?;
            if source_row.is_archived() {
                return Err(format!(
                    "{} is archived, and a shadow of it would be a live context under an archived senpai. \
                     Bring it back first with `kj context promote {}`",
                    db.context_name(source),
                    source.to_hex()
                ));
            }
            let performer = source_row.performer.ok_or_else(|| {
                format!("{} has no performer, so it has no image slot", db.context_name(source))
            })?;
            let sheet = db
                .get_character(performer)
                .map_err(|e| e.to_string())?
                .filter(|sheet| sheet.retired_at.is_none())
                .ok_or_else(|| format!("the performer of {} is not a live character", db.context_name(source)))?;
            if let Some(existing) = db.live_shadow(source, DESCRIBE_SHADOW).map_err(|e| e.to_string())? {
                State::Existing(existing)
            } else {
                if db
                    .get_character_slot(performer, crate::model_resolution::IMAGE)
                    .map_err(|e| e.to_string())?
                    .is_none()
                {
                    return Err(format!(
                        "{} cannot get a description: its performer {} has no `image` slot. \
                         Fix: `kj character slot set {} image <backend>/<model> --max-tokens <n>`",
                        db.context_name(source),
                        sheet.name,
                        sheet.name
                    ));
                }
                let row = crate::council::shadow::shadow_row(&db, &source_row, DESCRIBE_SHADOW).map_err(|e| e.to_string())?;
                db.in_transaction(|db| {
                    super::context::insert_new_context_rows(
                        db,
                        &row,
                        super::context::CharacterPlacement::Shadow {
                            performer,
                            senpai: source,
                            model_role: crate::model_resolution::IMAGE,
                        },
                    )?;
                    db.mark_shadow(row.context_id, DESCRIBE_SHADOW)
                })
                .map_err(|e| e.to_string())?;
                State::Created(row)
            }
        };
        let row = match state {
            State::Existing(existing) => {
                crate::council::shadow::ensure_existing_shadow_document(
                    &self.blocks,
                    &self.drift,
                    &existing,
                    source,
                )?;
                return Ok(existing);
            }
            State::Created(row) => {
                crate::council::shadow::create_shadow_document(
                    self.kernel_db(),
                    &self.blocks,
                    &self.drift,
                    &row,
                    source,
                )?;
                row
            }
        };
        self.kernel_db()
            .lock()
            .get_context(row.context_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("shadow {} has no row after it was made", row.context_id.short()))
    }

    /// The text of a rendition block, or why it cannot be read.
    fn rendition_text(&self, block: BlockId) -> Result<String, String> {
        let snapshots = self.blocks.non_draft_snapshots(block.context_id).map_err(|e| {
            format!("the index names rendition {} but its context cannot be read: {e}", block.to_key())
        })?;
        snapshots
            .into_iter()
            .find(|b| b.id == block)
            .map(|b| b.content)
            .ok_or_else(|| format!("the index names rendition {} but that block is not in its context", block.to_key()))
    }

    /// A rendition block of `source` in `shadow`, its link, and (with `key`)
    /// its index row, committed in one journal transaction: all of them or
    /// none. Returns the block and whether `key` names it; a `key` that
    /// another rendition already holds stays with that one.
    fn write_rendition(
        &self,
        shadow: ContextId,
        performer: kaijutsu_types::PrincipalId,
        text: &str,
        source: BlockId,
        key: Option<&IndexKey>,
    ) -> Result<(BlockId, bool), String> {
        let indexed = std::cell::Cell::new(false);
        let block = self
            .blocks
            .append_block_recorded(
                shadow,
                crate::block_store::RecordedBlockInsert {
                    role: Role::Model,
                    kind: BlockKind::Text,
                    content: text.to_string(),
                    status: Status::Done,
                    content_type: ContentType::Plain,
                    author: performer,
                    ansi: None,
                    shell: None,
                },
                |db, block| {
                    let rendition = Rendition { block: *block, source, span: None };
                    match key {
                        Some(key) => indexed.set(db.record_rendition_unless_indexed(&rendition, key)?),
                        None => db.record_rendition(&rendition, None)?,
                    }
                    Ok(())
                },
            )
            .map_err(|e| format!("could not write the rendition: {e}"))?;
        Ok((block, indexed.get()))
    }

    /// One model call on the shadow's `model` slot, recorded as a turn of the
    /// shadow's performer before it is made, and bounded by
    /// `timeout`. The reply is the description text.
    #[allow(clippy::too_many_arguments)]
    async fn call_describer(
        &self,
        provider: &crate::llm::Provider,
        bound: &crate::model_resolution::BoundSlot,
        shadow: ContextId,
        performer: kaijutsu_types::PrincipalId,
        max_tokens: Option<u64>,
        instruction: &str,
        messages: Vec<Message>,
        timeout: Duration,
    ) -> Result<String, String> {
        let model = bound.slot.model.clone();
        let db = self.kernel_db().clone();
        let turn_id = kaijutsu_types::TurnId::new();
        let row = db
            .lock()
            .begin_turn(&crate::TurnStart {
                turn_id,
                context_id: shadow,
                performer,
                role: crate::model_resolution::IMAGE.to_string(),
                provider: bound.backend.clone(),
                model: model.clone(),
                started_at: kaijutsu_types::now_millis() as i64,
            })
            .map(|()| super::OpenTurnRow { db: db.clone(), turn_id, open: true })
            .map_err(|e| format!("could not write the description's turn record: {e}. The call was not made."))?;
        let slot = &bound.slot;
        let tunables = crate::llm::SlotTunables {
            max_tokens: None,
            temperature: slot.temperature,
            top_p: slot.top_p,
            effort: slot.effort.clone(),
            thinking_budget: slot.thinking_budget.and_then(|v| u64::try_from(v).ok()),
            thinking_style: slot.thinking_style.clone(),
            verbosity: None,
            reasoning_summary: None, replay_phase: None,
            decide: None,
        };
        let opts = crate::llm::apply_tunables(
            crate::llm::BuildOpts::with_output_limit(&model, max_tokens).with_system(instruction),
            &tunables,
        );
        let reply = match tokio::time::timeout(timeout, provider.prompt_messages(opts, messages)).await {
            Ok(reply) => reply,
            Err(_) => Err(crate::llm::OneShotError::from(crate::llm::LlmError::NetworkError(format!(
                "no description within {timeout:?} (DESCRIBE_TIMEOUT)"
            )))),
        };
        let empty = matches!(&reply, Ok(reply) if reply.text.trim().is_empty());
        let (usage, stop_reason) = match &reply {
            Ok(reply) if empty => (Some(&reply.usage), "failed"),
            Ok(reply) => (
                Some(&reply.usage),
                crate::runtime::llm_stream::turn_stop_reason(reply.stop_reason.as_deref()).as_str(),
            ),
            Err(failed) => (failed.usage.as_ref(), "failed"),
        };
        let recorded = usage.map_or(Ok(()), |usage| {
            crate::runtime::llm_stream::record_turn_spend(
                &db, Some(turn_id), provider.name(), &model,
                usage.input_tokens, usage.output_tokens, &usage.extra,
            ).map(|_| ())
        });
        let record = recorded
            .and(row.finish(stop_reason))
            .map_err(|e| format!("could not write the description's turn record: {e}"));
        if let Err(error) = &record {
            tracing::error!(context = %shadow, turn = %turn_id, "{error}");
        }
        let reply = reply.map_err(|e| format!("{}/{model} failed: {e}", bound.backend))?;
        record?;
        if empty {
            return Err(format!("{}/{model} returned an empty description; no rendition was written", bound.backend));
        }
        Ok(reply.text)
    }
}

fn describe_result(done: &Described) -> KjResult {
    let origin = if done.reused { "reused an existing description" } else { "described" };
    let text = format!(
        "{origin} of {} ({}) by {}\n{}\nrendition: {}\n",
        done.hash,
        done.media_type,
        done.maker,
        done.text,
        done.rendition.to_key()
    );
    KjResult::ok_with_data(
        text,
        serde_json::json!({
            "description": done.text,
            "rendition": done.rendition.to_key(),
            "shadow": done.shadow.to_hex(),
            "maker": done.maker,
            "media_type": done.media_type,
            "content_id": done.hash,
            "reused": done.reused,
        }),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use kaijutsu_cas::ContentStore;
    use kaijutsu_council::test_server::{reply as http, serve, sse, Captured, Reply};
    use kaijutsu_types::{BlockId, BlockKind, ContentType, ContextId, PrincipalId, Role, Status};
    use serde_json::json;

    use super::*;
    use crate::kj::test_helpers::*;
    use crate::llm::{council, Provider};

    fn s(v: &str) -> String {
        v.to_string()
    }

    const PICTURE: &[u8] = b"not really a png";

    /// A council identity serving two models that read `image/png` and give
    /// the first the alias `qwen`.
    fn identity(inputs: serde_json::Value) -> Reply {
        let model = |name: &str, aliases: serde_json::Value| {
            json!({"model": name, "aliases": aliases, "status": "ready", "weight_hash": "w",
                   "tokenizer_hash": "t", "template": "x", "engine": "e",
                   "limits": {"context_tokens": 32768, "decision_context_tokens": 3072, "snapshot_bytes": 1,
                              "contexts_per_decision": 8, "default_timeout_ms": 2000},
                   "capabilities": ["generate"],
                   "media": {"inputs": inputs, "outputs": []}})
        };
        http(
            200,
            json!({"models": [model("lfm3-omni", json!(["qwen"])), model("lfm3-vl", json!([]))],
                   "limits": {"case_bytes": 1000, "choice_options": 26},
                   "capabilities": ["objects"]})
            .to_string(),
        )
    }

    fn object_put(bytes: &[u8], mime: &str) -> Reply {
        http(201, json!({"id": gogi_proto::content_id(bytes), "mime": mime, "bytes": bytes.len()}).to_string())
    }

    fn generation(content: &str, model: &str) -> Reply {
        let token = json!({"id": 1, "piece": content, "phase": "content", "p": 0.9, "h": 0.1, "n": 1, "tps": 10.0});
        let done = json!({"model": model, "content": content, "reasoning": "", "tool_calls": [], "finish": "end_turn",
                          "usage": {"prompt": 47, "kept": 40, "fed": 7, "completion": 3, "reasoning": 0}, "head": null, "seed": 11,
                          "snapshots": []});
        sse(format!("event: token\ndata: {token}\n\nevent: done\ndata: {done}\n\n"))
    }

    type Seen = Arc<std::sync::Mutex<Vec<Captured>>>;

    fn requests(seen: &Seen) -> Vec<String> {
        seen.lock().unwrap().iter().map(|c| format!("{} {}", c.method, c.path)).collect()
    }

    /// A kernel with a council backend `nagaya` at `base`, and a character
    /// `describer` that plays a context `src`. `describe` is the model of its
    /// `image` slot, when it has one.
    struct World {
        d: KjDispatcher,
        performer: PrincipalId,
        source: ContextId,
    }

    async fn world(base: &str, describe: Option<&str>) -> World {
        let d = test_dispatcher().await;
        let set = d
            .dispatch(&[s("backend"), s("set"), s("nagaya"), s("--kind"), s("council"), s("--base-url"), s(base), s("--key-optional")], &test_caller())
            .await;
        assert!(set.is_ok(), "{}", set.message());
        d.kernel()
            .llm()
            .write()
            .await
            .register("nagaya", Arc::new(Provider::Council(council::Client::new("nagaya", base, Duration::from_secs(5)).unwrap())));
        let performer = character(&d, "describer", describe);
        let source = context_of(&d, "src", performer);
        World { d, performer, source }
    }

    fn character(d: &KjDispatcher, name: &str, image: Option<&str>) -> PrincipalId {
        let performer = PrincipalId::new();
        let db = d.kernel_db().lock();
        db.insert_character(&crate::kernel_db::CharacterRow {
            rc_dir: Some("coder".into()), console_ctx: None, mcp_hub_ctx: None, principal_id: performer,
            name: name.into(), created_at: 0, retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
        })
        .unwrap();
        let mut entries = vec![("model", "mock", "mock-model")];
        if let Some(model) = image {
            entries.push(("image", "nagaya", model));
        }
        db.seed_test_slot_entries(performer, &entries).unwrap();
        performer
    }

    fn context_of(d: &KjDispatcher, label: &str, performer: PrincipalId) -> ContextId {
        let ctx = register_rooted_context(d, Some(label), performer);
        d.block_store().create_document(ctx, kaijutsu_types::DocKind::Conversation, None).unwrap();
        ctx
    }

    /// A media block in `ctx` that holds `bytes` as `mime`.
    fn media(d: &KjDispatcher, ctx: ContextId, bytes: &[u8], mime: &str) -> BlockId {
        let hash = d.kernel().cas().store(bytes, mime).unwrap();
        d.block_store()
            .insert_block(ctx, None, None, Role::Asset, BlockKind::File, hash.to_string(), Status::Done, ContentType::Media)
            .unwrap()
    }

    async fn describe(d: &KjDispatcher, ctx: ContextId, block: BlockId) -> KjResult {
        d.dispatch(&[s("shadow"), s("describe"), block.to_key()], &confirmed_caller(ctx)).await
    }

    fn shadow_of(d: &KjDispatcher, ctx: ContextId) -> ContextRow {
        d.kernel_db().lock().live_shadow(ctx, DESCRIBE_SHADOW).unwrap().expect("a description shadow")
    }

    fn rendition_text(d: &KjDispatcher, block: BlockId) -> String {
        d.block_store().block_snapshots(block.context_id).unwrap().into_iter().find(|b| b.id == block).unwrap().content
    }

    /// With no `image` slot on the performer, a request is refused naming
    /// the verb that sets one, makes no shadow, and calls no model.
    ///
    /// Falsified by a fallback to the performer's chat model.
    #[tokio::test]
    async fn describe_without_an_image_slot_names_the_fix() {
        let (base, seen) = serve(vec![]).await;
        let w = world(&base, None).await;
        let image = media(&w.d, w.source, PICTURE, "image/png");
        let r = describe(&w.d, w.source, image).await;
        assert!(
            matches!(&r, KjResult::Err(m) if m.contains("kj character slot set describer image <backend>/<model>")),
            "{r:?}"
        );
        assert!(w.d.kernel_db().lock().live_shadow(w.source, DESCRIBE_SHADOW).unwrap().is_none(), "no shadow is made");
        assert!(requests(&seen).is_empty());
    }

    /// A text block and an audio block are refused, saying what they are, and
    /// nothing is made.
    ///
    /// Falsified by accepting any media block, or any block with a hash.
    #[tokio::test]
    async fn a_block_that_is_not_an_image_is_refused() {
        let (base, seen) = serve(vec![]).await;
        let w = world(&base, Some("qwen")).await;
        let text = w
            .d
            .block_store()
            .insert_block(w.source, None, None, Role::User, BlockKind::Text, "hello", Status::Done, ContentType::Plain)
            .unwrap();
        let r = describe(&w.d, w.source, text).await;
        assert!(matches!(&r, KjResult::Err(m) if m.contains("not a media block") && m.contains("text/plain")), "{r:?}");
        let audio = media(&w.d, w.source, b"RIFFfakewav", "audio/wav");
        let r = describe(&w.d, w.source, audio).await;
        assert!(matches!(&r, KjResult::Err(m) if m.contains("audio/wav") && m.contains("it takes image/*")), "{r:?}");
        let video = media(&w.d, w.source, b"fake mp4", "video/mp4");
        let r = describe(&w.d, w.source, video).await;
        assert!(matches!(&r, KjResult::Err(m) if m.contains("video/mp4") && m.contains("`video` slot")), "{r:?}");
        assert!(w.d.kernel_db().lock().live_shadow(w.source, DESCRIBE_SHADOW).unwrap().is_none());
        assert!(requests(&seen).is_empty());
    }

    /// A describe on a council backend puts the image as an object, sends it
    /// in one user message under the kernel's instruction, and writes a
    /// rendition block in a shadow that the source's performer plays and whose
    /// `model` slot is the `image` slot. The rendition is linked to the
    /// source block and indexed, and the model call is a recorded turn.
    ///
    /// Falsified by sending a marker, by writing the text to the source
    /// context, or by leaving the rendition out of the link or the index.
    #[tokio::test]
    async fn a_description_is_written_linked_and_indexed() {
        let (base, seen) = serve(vec![
            identity(json!(["image/png"])),
            http(404, ""),
            object_put(PICTURE, "image/png"),
            generation("a red square on white", "lfm3-omni"),
        ])
        .await;
        let w = world(&base, Some("qwen")).await;
        let image = media(&w.d, w.source, PICTURE, "image/png");
        let r = describe(&w.d, w.source, image).await;
        assert!(r.is_ok(), "{}", r.message());
        assert!(r.message().contains("a red square on white") && r.message().contains("rendition: "), "{}", r.message());

        let id = gogi_proto::content_id(PICTURE);
        assert_eq!(
            requests(&seen),
            ["GET /council/v2/identity", &format!("HEAD /council/v2/objects/{id}"), &format!("PUT /council/v2/objects/{id}"), "POST /council/v2/generations"]
        );
        let body: serde_json::Value = serde_json::from_str(&seen.lock().unwrap()[3].body).unwrap();
        let messages = body["context"]["messages"].as_array().unwrap();
        let user = messages.iter().rfind(|m| m["role"] == "user").unwrap();
        assert_eq!(user["content"], json!([{"type": "object", "id": id, "mime": "image/png"}]));
        assert!(messages[0]["content"].to_string().contains("Describe the attached image"), "{}", messages[0]);

        let shadow = shadow_of(&w.d, w.source);
        assert_eq!((shadow.forked_from, shadow.senpai_ctx, shadow.performer), (Some(w.source), Some(w.source), Some(w.performer)));
        let db = w.d.kernel_db().lock();
        let model = db.context_slot(shadow.context_id, "model").unwrap().unwrap();
        assert_eq!(model.model, "qwen", "the shadow's model slot is the image slot, not the chat model");
        let links = db.renditions_of(shadow.context_id).unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!((links[0].source, links[0].span), (image, None));
        let hash = db.indexed_rendition(&IndexKey::new(&w.d.kernel().cas().store(PICTURE, "image/png").unwrap().to_string(), "describe", "nagaya/qwen")).unwrap();
        assert_eq!(hash, Some(links[0].block));
        let turns = db.list_turns(shadow.context_id, 10).unwrap();
        assert_eq!(turns.len(), 1, "{turns:?}");
        assert_eq!((turns[0].start.role.as_str(), turns[0].stop_reason.as_deref()), ("image", Some("end_turn")));
        drop(db);
        assert_eq!(rendition_text(&w.d, links_block(&w.d, shadow.context_id)), "a red square on white");
        assert!(
            w.d.block_store().block_snapshots(w.source).unwrap().iter().all(|b| b.content != "a red square on white"),
            "the source context holds no rendition"
        );
    }

    fn links_block(d: &KjDispatcher, shadow: ContextId) -> BlockId {
        d.kernel_db().lock().renditions_of(shadow).unwrap()[0].block
    }

    /// The same image by the same maker, asked for in another context, copies
    /// the first rendition into that context's own shadow with its own link
    /// and calls no model; a different maker calls the model again.
    ///
    /// Falsified by an index that ignores the maker, or a reuse that calls
    /// the model anyway.
    #[tokio::test]
    async fn a_description_is_reused_across_contexts_and_another_maker_asks_again() {
        let (base, seen) = serve(vec![
            identity(json!(["image/png"])),
            http(404, ""),
            object_put(PICTURE, "image/png"),
            generation("a red square on white", "lfm3-omni"),
            http(200, ""),
            generation("a small red square", "lfm3-vl"),
        ])
        .await;
        let w = world(&base, Some("qwen")).await;
        let first = media(&w.d, w.source, PICTURE, "image/png");
        assert!(describe(&w.d, w.source, first).await.is_ok());
        assert_eq!(requests(&seen).len(), 4);

        // Another context of the same performer, with the same image.
        let other = context_of(&w.d, "other", w.performer);
        let again = media(&w.d, other, PICTURE, "image/png");
        let r = describe(&w.d, other, again).await;
        assert!(r.is_ok(), "{}", r.message());
        assert!(r.message().contains("reused"), "{}", r.message());
        assert_eq!(requests(&seen).len(), 4, "no model call, no object traffic: {:?}", requests(&seen));
        let (a, b) = (shadow_of(&w.d, w.source), shadow_of(&w.d, other));
        assert_ne!(a.context_id, b.context_id, "each source has its own shadow");
        let copy = w.d.kernel_db().lock().renditions_of(b.context_id).unwrap();
        assert_eq!(copy.len(), 1);
        assert_eq!(copy[0].source, again, "the copy links its own source block");
        assert_eq!(copy[0].block.context_id, b.context_id);
        assert_eq!(rendition_text(&w.d, copy[0].block), "a red square on white");

        // A performer whose image slot is another model does not reuse it.
        let second = character(&w.d, "describer2", Some("lfm3-vl"));
        let third = context_of(&w.d, "third", second);
        let image = media(&w.d, third, PICTURE, "image/png");
        let r = describe(&w.d, third, image).await;
        assert!(r.is_ok(), "{}", r.message());
        assert!(!r.message().contains("reused") && r.message().contains("a small red square"), "{}", r.message());
        assert_eq!(requests(&seen).len(), 6);
    }

    /// A served model that does not read the media's type is refused before
    /// any generation, naming the model and the type.
    ///
    /// Falsified by calling the model and describing the marker.
    #[tokio::test]
    async fn media_the_model_cannot_read_is_refused_before_the_model_is_called() {
        let (base, seen) = serve(vec![identity(json!(["image/png"]))]).await;
        let w = world(&base, Some("qwen")).await;
        let jpeg = media(&w.d, w.source, b"fake jpeg", "image/jpeg");
        let r = describe(&w.d, w.source, jpeg).await;
        assert!(
            matches!(&r, KjResult::Err(m) if m.contains("nagaya/qwen") && m.contains("image/jpeg") && m.contains("No model was called")),
            "{r:?}"
        );
        assert_eq!(requests(&seen), ["GET /council/v2/identity"]);
        assert!(
            w.d.kernel_db().lock().live_shadow(w.source, DESCRIBE_SHADOW).unwrap().is_none(),
            "a refused marker must not create an empty shadow"
        );
    }

    #[tokio::test]
    async fn preflight_refusals_do_not_create_an_empty_shadow() {
        // Unregistered provider leaves no shadow.
        let d = test_dispatcher().await;
        let performer = PrincipalId::new();
        {
            let db = d.kernel_db().lock();
            db.upsert_backend(&crate::kernel_db::BackendRow {
                backend_id: kaijutsu_types::BackendId::new(),
                name: "unreg".into(),
                kind: "council".into(),
                base_url: Some("http://127.0.0.1:9".into()),
                api_key_env: None,
                api_key_file: None,
                key_optional: true,
                request_timeout_secs: None,
                idle_timeout_secs: None,
                max_concurrent: None,
                created_at: 0,
                created_by: PrincipalId::system(),
            })
            .unwrap();
            db.insert_character(&crate::kernel_db::CharacterRow {
                rc_dir: Some("coder".into()),
                console_ctx: None,
                mcp_hub_ctx: None,
                principal_id: performer,
                name: "describer-unreg".into(),
                created_at: 0,
                retired_at: None,
                handoff_ctx: None,
                root_ctx: None,
                root: false,
            })
            .unwrap();
            db.seed_test_slot_entries(performer, &[("model", "mock", "mock-model"), ("image", "unreg", "qwen")]).unwrap();
        }
        let source = context_of(&d, "unreg-src", performer);
        let image = media(&d, source, PICTURE, "image/png");
        let r = describe(&d, source, image).await;
        assert!(matches!(&r, KjResult::Err(m) if m.contains("not registered")), "{r:?}");
        assert!(d.kernel_db().lock().live_shadow(source, DESCRIBE_SHADOW).unwrap().is_none());

        // Missing max_tokens on a backend that requires it leaves no shadow.
        let set_anth = d
            .dispatch(&[s("backend"), s("set"), s("anth"), s("--kind"), s("anthropic"), s("--key-optional")], &test_caller())
            .await;
        assert!(set_anth.is_ok(), "{}", set_anth.message());
        let no_max_performer = PrincipalId::new();
        {
            let db = d.kernel_db().lock();
            db.insert_character(&crate::kernel_db::CharacterRow {
                rc_dir: Some("coder".into()),
                console_ctx: None,
                mcp_hub_ctx: None,
                principal_id: no_max_performer,
                name: "describer-nomax".into(),
                created_at: 0,
                retired_at: None,
                handoff_ctx: None,
                root_ctx: None,
                root: false,
            })
            .unwrap();
            db.seed_test_slot_entries(no_max_performer, &[("model", "mock", "mock-model")]).unwrap();
        }
        let no_max_src = context_of(&d, "nomax-src", no_max_performer);
        {
            let db = d.kernel_db().lock();
            let anth_id = db.get_backend_by_name("anth").unwrap().unwrap().backend_id;
            db.set_character_slot(&crate::kernel_db::CharacterSlotRow {
                principal_id: no_max_performer,
                role: crate::model_resolution::IMAGE.into(),
                backend_id: anth_id,
                model: "claude-sonnet".into(),
                max_tokens: None,
                temperature: None,
                top_p: None,
                effort: None,
                thinking_budget: None,
                thinking_style: None,
            })
            .unwrap();
        }
        let no_max_img = media(&d, no_max_src, PICTURE, "image/png");
        let r = describe(&d, no_max_src, no_max_img).await;
        assert!(matches!(&r, KjResult::Err(m) if m.contains("max_tokens")), "{r:?}");
        assert!(d.kernel_db().lock().live_shadow(no_max_src, DESCRIBE_SHADOW).unwrap().is_none());

        // Missing/empty CAS bytes leaves no shadow.
        let (base, seen) = serve(vec![identity(json!(["image/png"]))]).await;
        let w = world(&base, Some("qwen")).await;
        let missing_bytes_img = media(&w.d, w.source, b"temp-png-bytes", "image/png");
        let hash = w.d.kernel().cas().store(b"temp-png-bytes", "image/png").unwrap();
        let obj_path = w
            .d
            .kernel()
            .cas()
            .config()
            .objects_dir()
            .join(hash.prefix())
            .join(hash.remainder());
        std::fs::write(&obj_path, b"").unwrap();
        let r = describe(&w.d, w.source, missing_bytes_img).await;
        assert!(matches!(&r, KjResult::Err(m) if m.contains("were not found in CAS")), "{r:?}");
        assert_eq!(requests(&seen), ["GET /council/v2/identity"]);
        assert!(w.d.kernel_db().lock().live_shadow(w.source, DESCRIBE_SHADOW).unwrap().is_none());
    }

    #[tokio::test]
    async fn ensure_description_shadow_does_not_strand_on_create_document_failure_and_recovers_existing_shadow() {
        let (base, _) = serve(vec![]).await;
        let w = world(&base, Some("qwen")).await;

        // 1) Diverging workspace causes create_document to fail; no live_shadow row may remain.
        let alt_ws = kaijutsu_types::WorkspaceId::new();
        {
            let db = w.d.kernel_db().lock();
            db.insert_workspace(&crate::kernel_db::WorkspaceRow {
                workspace_id: alt_ws,
                label: "alt".into(),
                description: None,
                created_at: 0,
                created_by: w.performer,
                archived_at: None,
            })
            .unwrap();
            db.update_workspace(w.source, Some(alt_ws)).unwrap();
        }
        let err = w.d.ensure_description_shadow(w.source).unwrap_err();
        assert!(err.contains("shadow document"), "{err}");
        assert!(
            w.d.kernel_db().lock().live_shadow(w.source, DESCRIBE_SHADOW).unwrap().is_none(),
            "failed create_document must not leave a live_shadow row"
        );

        // 2) An existing live_shadow row without a loaded BlockStore document is recovered on lookup.
        let other = context_of(&w.d, "recover-src", w.performer);
        let stranded = {
            let db = w.d.kernel_db().lock();
            let source_row = db.get_context(other).unwrap().unwrap();
            let row = crate::council::shadow::shadow_row(&db, &source_row, DESCRIBE_SHADOW).unwrap();
            db.in_transaction(|db| {
                super::super::context::insert_new_context_rows(
                    db,
                    &row,
                    super::super::context::CharacterPlacement::Shadow {
                        performer: w.performer,
                        senpai: other,
                        model_role: crate::model_resolution::IMAGE,
                    },
                )?;
                db.mark_shadow(row.context_id, DESCRIBE_SHADOW)
            })
            .unwrap();
            row
        };
        assert!(!w.d.block_store().contains(stranded.context_id));
        let recovered = w.d.ensure_description_shadow(other).unwrap();
        assert_eq!(recovered.context_id, stranded.context_id);
        assert!(w.d.block_store().contains(recovered.context_id));
    }

    /// A model that does not answer within the timeout is an error that names
    /// the timeout, writes no rendition, and closes its turn as failed.
    ///
    /// Falsified by a call with no bound.
    #[tokio::test]
    async fn a_model_that_does_not_answer_in_time_writes_nothing() {
        let slow = Reply { delay: Duration::from_secs(5), ..generation("late", "lfm3-omni") };
        let (base, _) = serve(vec![identity(json!(["image/png"])), http(404, ""), object_put(PICTURE, "image/png"), slow]).await;
        let w = world(&base, Some("qwen")).await;
        let image = media(&w.d, w.source, PICTURE, "image/png");
        let caller = confirmed_caller(w.source);
        let started = std::time::Instant::now();
        let err = w.d.describe_block(&image.to_key(), None, &caller, Duration::from_millis(300)).await.err().expect("a timeout");
        assert!(err.contains("DESCRIBE_TIMEOUT") && err.contains("300ms"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(4), "the call is bounded");
        let shadow = shadow_of(&w.d, w.source);
        assert_eq!(w.d.kernel_db().lock().rendition_count(shadow.context_id).unwrap(), 0);
        let turns = w.d.kernel_db().lock().list_turns(shadow.context_id, 10).unwrap();
        assert_eq!(turns[0].stop_reason.as_deref(), Some("failed"), "{turns:?}");
    }

    /// The journal rows of a document.
    fn journal_rows(d: &KjDispatcher, ctx: ContextId) -> i64 {
        let db = d.kernel_db().lock();
        db.conn_for_ledger()
            .query_row("SELECT COUNT(*) FROM oplog WHERE document_id = ?1", [ctx.as_bytes().to_vec()], |r| r.get(0))
            .unwrap()
    }

    /// Make every link insert fail, as a database fault would.
    fn break_links(d: &KjDispatcher) {
        d.kernel_db()
            .lock()
            .conn_for_ledger()
            .execute_batch(
                "CREATE TRIGGER break_links BEFORE INSERT ON shadow_renditions
                 BEGIN SELECT RAISE(ABORT, 'injected link failure'); END;",
            )
            .unwrap();
    }

    /// A rendition block, its link, and its index row commit together: when the
    /// link cannot be written, the request fails and the journal holds no block
    /// of the shadow, on the fresh path and on the reuse path.
    ///
    /// Falsified by writing the block in its own transaction before the link.
    #[tokio::test]
    async fn a_rendition_block_is_not_kept_without_its_link() {
        let (base, seen) = serve(vec![
            identity(json!(["image/png"])),
            http(404, ""),
            object_put(PICTURE, "image/png"),
            generation("a red square on white", "lfm3-omni"),
        ])
        .await;
        let w = world(&base, Some("qwen")).await;
        let image = media(&w.d, w.source, PICTURE, "image/png");
        // Fresh path: the model answers, and the link fails.
        let shadow = w.d.ensure_description_shadow(w.source).unwrap().context_id;
        let before = journal_rows(&w.d, shadow);
        break_links(&w.d);
        let r = describe(&w.d, w.source, image).await;
        assert!(matches!(&r, KjResult::Err(m) if m.contains("injected link failure")), "{r:?}");
        assert_eq!(journal_rows(&w.d, shadow), before, "the block was not journaled");
        assert_eq!(w.d.kernel_db().lock().rendition_count(shadow).unwrap(), 0);
        w.d.kernel_db().lock().conn_for_ledger().execute_batch("DROP TRIGGER break_links").unwrap();

        // Reuse path: a rendition exists elsewhere, and the copy's link fails.
        let other_ctx = context_of(&w.d, "other", w.performer);
        let again = media(&w.d, other_ctx, PICTURE, "image/png");
        let key = IndexKey::new(&w.d.kernel().cas().store(PICTURE, "image/png").unwrap().to_string(), DESCRIBE_SHADOW, "nagaya/qwen");
        // A failed commit leaves its document refusing writes until restart, so
        // the earlier rendition lives in a shadow of a third context.
        let third = context_of(&w.d, "third", w.performer);
        let third_shadow = w.d.ensure_description_shadow(third).unwrap().context_id;
        let kept = w.d.write_rendition(third_shadow, w.performer, "kept", image, Some(&key)).unwrap();
        assert!(kept.1);
        let other = w.d.ensure_description_shadow(other_ctx).unwrap().context_id;
        let before = journal_rows(&w.d, other);
        break_links(&w.d);
        let r = describe(&w.d, other_ctx, again).await;
        assert!(matches!(&r, KjResult::Err(m) if m.contains("injected link failure")), "{r:?}");
        assert_eq!(journal_rows(&w.d, other), before, "the copy was not journaled");
        assert_eq!(requests(&seen).len(), 4, "the reuse path made no model call");
    }

    /// A request that finds its key taken after it made its description keeps
    /// the description, linked and not indexed, and the key keeps naming the
    /// first rendition; the shadow takes further writes.
    ///
    /// Falsified by indexing without checking the key, which fails the request
    /// on the key.
    #[tokio::test]
    async fn a_request_that_loses_the_key_still_gets_its_description() {
        let (base, _) = serve(vec![]).await;
        let w = world(&base, Some("qwen")).await;
        let image = media(&w.d, w.source, PICTURE, "image/png");
        let shadow = w.d.ensure_description_shadow(w.source).unwrap().context_id;
        let key = IndexKey::new("abc", DESCRIBE_SHADOW, "nagaya/qwen");
        let (first, first_indexed) = w.d.write_rendition(shadow, w.performer, "first", image, Some(&key)).unwrap();
        let (second, second_indexed) = w.d.write_rendition(shadow, w.performer, "second", image, Some(&key)).unwrap();
        assert!(first_indexed && !second_indexed);
        assert_ne!(first, second);
        assert_eq!(rendition_text(&w.d, second), "second");
        let db = w.d.kernel_db().lock();
        assert_eq!(db.rendition_count(shadow).unwrap(), 2, "both are linked");
        assert_eq!(db.indexed_rendition(&key).unwrap(), Some(first), "the key keeps the first");
        drop(db);
        w.d.write_rendition(shadow, w.performer, "third", image, None).expect("the shadow's document takes further writes");
    }

    /// A source context that is archived is refused before any shadow is made,
    /// saying it is archived and naming `kj context promote`.
    ///
    /// Falsified by leaving out the check, which makes a live shadow under an
    /// archived senpai.
    #[tokio::test]
    async fn an_archived_source_gets_no_shadow() {
        let (base, seen) = serve(vec![]).await;
        let w = world(&base, Some("qwen")).await;
        let image = media(&w.d, w.source, PICTURE, "image/png");
        assert!(w.d.kernel_db().lock().archive_context(w.source).unwrap());
        let r = describe(&w.d, w.source, image).await;
        assert!(
            matches!(&r, KjResult::Err(m) if m.contains("archived") && m.contains(&format!("kj context promote {}", w.source.to_hex()))),
            "{r:?}"
        );
        assert!(w.d.kernel_db().lock().fork_children(w.source).unwrap().is_empty(), "no shadow was made");
        assert!(requests(&seen).is_empty());
    }

    /// A reply with no text closes its turn as failed, writes no rendition,
    /// and says the description was empty.
    ///
    /// Falsified by closing the turn as `end_turn` before the text is checked.
    #[tokio::test]
    async fn an_empty_description_fails_its_turn_and_writes_nothing() {
        let (base, _) = serve(vec![
            identity(json!(["image/png"])),
            http(404, ""),
            object_put(PICTURE, "image/png"),
            generation("", "lfm3-omni"),
        ])
        .await;
        let w = world(&base, Some("qwen")).await;
        let image = media(&w.d, w.source, PICTURE, "image/png");
        let r = describe(&w.d, w.source, image).await;
        assert!(matches!(&r, KjResult::Err(m) if m.contains("empty description")), "{r:?}");
        let shadow = shadow_of(&w.d, w.source);
        assert_eq!(w.d.kernel_db().lock().rendition_count(shadow.context_id).unwrap(), 0);
        let turns = w.d.kernel_db().lock().list_turns(shadow.context_id, 10).unwrap();
        assert_eq!(turns[0].stop_reason.as_deref(), Some("failed"), "{turns:?}");
    }

    /// `kj shadow list` shows each live shadow with its kind, id, performer,
    /// model, and rendition count.
    #[tokio::test]
    async fn list_shows_the_shadows_of_a_context() {
        let (base, _) = serve(vec![
            identity(json!(["image/png"])),
            http(404, ""),
            object_put(PICTURE, "image/png"),
            generation("a red square", "lfm3-omni"),
        ])
        .await;
        let w = world(&base, Some("qwen")).await;
        let caller = confirmed_caller(w.source);
        let none = w.d.dispatch(&[s("shadow"), s("list")], &caller).await;
        assert!(none.is_ok() && none.message().contains("no shadows"), "{}", none.message());
        let image = media(&w.d, w.source, PICTURE, "image/png");
        assert!(describe(&w.d, w.source, image).await.is_ok());
        let listed = w.d.dispatch(&[s("shadow"), s("list")], &caller).await;
        let shadow = shadow_of(&w.d, w.source);
        let line = listed.message();
        assert!(
            line.starts_with("describe") && line.contains(&shadow.context_id.to_hex()) && line.contains("describer")
                && line.contains("nagaya/qwen") && line.contains("1 rendition"),
            "{line}"
        );
    }

    /// A `kj shadow` verb is classified: listing reads, describing writes.
    #[test]
    fn list_reads_and_describe_writes() {
        use crate::kj::effect::{classify, Effect};
        assert_eq!(classify(&[s("shadow"), s("list")]).unwrap(), Effect::Read);
        assert_eq!(classify(&[s("shadow"), s("describe"), s("abc#1")]).unwrap(), Effect::Write);
    }
}
