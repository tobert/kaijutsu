//! `kj character` — the persistent someone a name resolves to (slice 1,
//! "the sheet": `docs/character.md`). `create` mints a fresh principal id
//! and its sheet row, idempotent on the name; `list`/`show` read it;
//! `retire` stamps `retired_at` and, in the same act, concludes and
//! archives every live context the character performs.
//!
//! `model set|unset|show` and `slot set|unset|list` (`kj/slot.rs`) hold the
//! model a character runs and the model each of its roles runs where that
//! differs. `unset` and `list` work on a retired character
//! too, so its slots can be tidied; `set` refuses one.

use clap::{Parser, Subcommand};
use kaijutsu_types::{ContentType, ContextId, ContextState, PrincipalId, SessionId};

use super::{KjCaller, KjDispatcher, KjResult, clap_help_for};
use crate::kernel_db::{CharacterRow, ContextRow};

/// The rc bundle a root context runs.
pub use kaijutsu_types::ROOT_CONTEXT_TYPE;

use kaijutsu_types::MCP_HUB_CONTEXT_TYPE;

/// The label of a root's MCP hub is this prefix and the root's name.
pub const MCP_HUB_LABEL_PREFIX: &str = "mcp/";

#[derive(Parser, Debug)]
#[command(
    name = "character",
    about = "The persistent someone a name resolves to",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct CharacterArgs {
    #[command(subcommand)]
    command: CharacterCommand,
}

#[derive(Subcommand, Debug)]
enum CharacterCommand {
    /// Create a character: mint a fresh principal id and write its sheet
    /// row. Idempotent on the name — creating an existing name returns the
    /// existing row rather than erroring or minting a second principal for
    /// the same name.
    Create {
        /// The character's kernel-owned given name.
        name: String,
        /// Create a root character: a place with hands on it, and a model
        /// context of its own. Its `kj` requests always need approval.
        #[arg(long)]
        root: bool,
        /// Backend of the character's model. Needs --model; a root runs no
        /// model.
        #[arg(long, requires = "model")]
        backend: Option<String>,
        /// Model id of the character's model. Needs --backend.
        #[arg(long, requires = "backend")]
        model: Option<String>,
        /// Maximum RESPONSE tokens of the character's model. Needs --model.
        /// Omit to read it from the model's `kj backend model set` row.
        #[arg(long = "max-tokens", requires = "model")]
        max_tokens: Option<u64>,
        /// The context type this character usually plays, such as `coder`.
        /// `kj context create <name>` uses it when `--type` is absent. Default:
        /// none, and then every create names `--type`. A root has none.
        #[arg(long = "type", value_name = "CONTEXT_TYPE")]
        context_type: Option<String>,
    },
    /// List the live (non-retired) characters.
    #[command(alias = "ls")]
    List {
        /// Include retired characters too.
        #[arg(long)]
        all: bool,
        /// Emit each row as a JSON object (name, id, timestamps, root)
        /// instead of the default array of ids.
        #[arg(long)]
        json: bool,
    },
    /// Show one character's sheet and its live contexts.
    Show {
        /// The character's name.
        name: String,
    },
    /// Update a character's sheet: whether it is a root, and the context type
    /// it usually plays. Name at least one option.
    Set {
        /// The character to update.
        name: String,
        /// Make this character a root: a model context whose `kj` requests
        /// always need approval.
        #[arg(long, conflicts_with = "no_root")]
        root: bool,
        /// Make this character an ordinary one that a model can play.
        #[arg(long = "no-root", conflicts_with = "root")]
        no_root: bool,
        /// The context type this character usually plays, such as `coder`.
        /// `kj context create <name>` uses it when `--type` is absent. A root
        /// has none.
        #[arg(long = "type", value_name = "CONTEXT_TYPE", conflicts_with = "no_type")]
        context_type: Option<String>,
        /// Clear the character's context type; every create then names `--type`.
        #[arg(long = "no-type", conflicts_with = "context_type")]
        no_type: bool,
    },
    /// The model a character runs. A context copies it when it is made, so a
    /// change reaches a running context at `kj context rotate`.
    #[command(subcommand)]
    Model(super::slot::CharacterModelCommand),
    /// The model one role of a character runs, where that differs from its
    /// model. A context copies the slots when it is made, so a change reaches
    /// a running context at `kj context rotate`.
    #[command(subcommand)]
    Slot(super::slot::CharacterSlotCommand),
    /// Retire a character and conclude and archive every live context it plays.
    /// The contexts retain their performer assignment.
    Retire {
        /// The character's name.
        name: String,
    },
}

fn root_lifecycle_caller(
    caller: &KjCaller,
    character: PrincipalId,
    context: ContextId,
) -> KjCaller {
    KjCaller {
        actor_id: character,
        reviewer_id: None,
        context_id: Some(context),
        confirmed: false,
        privileged: false,
        ..caller.clone()
    }
}

impl KjDispatcher {
    pub(crate) async fn dispatch_character(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<CharacterArgs>();
        }
        let parsed = match CharacterArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj character: {e}"));
            }
        };

        // Mutating verbs need the same admin-shaped gate the model
        // configuration writes use — minting, re-rooting, or retiring an identity is global,
        // cross-context state, not something a context should be able to do
        // to itself.
        let mutating = matches!(
            parsed.command,
            CharacterCommand::Create { .. }
                | CharacterCommand::Set { .. }
                | CharacterCommand::Retire { .. }
                | CharacterCommand::Model(super::slot::CharacterModelCommand::Set { .. })
                | CharacterCommand::Model(super::slot::CharacterModelCommand::Unset { .. })
                | CharacterCommand::Slot(super::slot::CharacterSlotCommand::Set { .. })
                | CharacterCommand::Slot(super::slot::CharacterSlotCommand::Unset { .. })
        );
        if mutating
            && let Err(denied) =
                self.require_cap(caller, crate::mcp::Capability::ConfigWrite, "character")
        {
            return denied;
        }

        match parsed.command {
            CharacterCommand::Create { name, root, backend, model, max_tokens, context_type } => {
                self.character_create(&name, root, backend.zip(model), max_tokens, context_type.as_deref(), caller).await
            }
            CharacterCommand::List { all, json } => self.character_list(all, json),
            CharacterCommand::Show { name } => self.character_show(&name),
            CharacterCommand::Set { name, root, no_root, context_type, no_type } => {
                if !root && !no_root && context_type.is_none() && !no_type {
                    return KjResult::Err("kj character set: specify --root, --no-root, --type <type> or --no-type".to_string());
                }
                self.character_set(&name, root, no_root, context_type.as_deref(), no_type, caller).await
            }
            CharacterCommand::Model(cmd) => self.character_model(cmd),
            CharacterCommand::Slot(cmd) => self.character_slot(cmd),
            CharacterCommand::Retire { name } => self.character_retire(&name, caller).await,
        }
    }

    /// Idempotent on `name`: an existing row is returned unchanged rather
    /// than minting a second principal for the same name — the double-mint
    /// hazard `add-key` avoids by binding instead of minting
    /// (`docs/character.md`, "Adding a key binds; it never mints") applies
    /// just as much to a repeated `create`.
    /// A character's type must be one `kj context create` accepts: a known,
    /// non-reserved type whose rc create bucket has a script.
    async fn check_character_type(&self, context_type: &str) -> Result<(), String> {
        super::rc::check_context_type(self.kernel().vfs(), context_type).await.map_err(|e| e.to_string())?;
        match crate::rc::create_bucket_has_candidates(self, context_type).await? {
            true => Ok(()),
            false => Err(format!(
                "context type '{context_type}' has no rc create scripts at {}, so no context of it can be made",
                kaijutsu_types::paths::rc_dir(context_type, crate::rc::VERB_CREATE),
            )),
        }
    }

    async fn character_create(
        &self,
        name: &str,
        root: bool,
        model: Option<(String, String)>,
        max_tokens: Option<u64>,
        context_type: Option<&str>,
        caller: &KjCaller,
    ) -> KjResult {
        if root && context_type.is_some() {
            return KjResult::Err(
                "kj character create: a root has no context type; drop --type, or drop --root".to_string(),
            );
        }
        // An existing name returns unchanged below, so its --type is not
        // checked; a new character's type must be one a context can be made of.
        let exists = matches!(self.kernel_db().lock().get_character_by_name(name), Ok(Some(_)));
        if let Some(context_type) = context_type
            && !exists
            && let Err(e) = self.check_character_type(context_type).await
        {
            return KjResult::Err(format!("kj character create: --type: {e}"));
        }
        if root && model.is_some() {
            return KjResult::Err(
                "kj character create: a root runs no model; drop --backend and --model, or drop --root".to_string(),
            );
        }
        let row = {
            let db = self.kernel_db().lock();
            match db.get_character_by_name(name) {
                Ok(Some(existing)) => {
                    let mut note = if model.is_some() {
                        format!("; its model is unchanged (`kj character model set {name} --backend B --model M` changes it)")
                    } else {
                        String::new()
                    };
                    if context_type.is_some() {
                        note.push_str(&format!("; its type is unchanged (`kj character set {name} --type T` changes it)"));
                    }
                    return KjResult::ok_with_data(
                        format!("{name} already exists ({}){note}", existing.principal_id.short()),
                        character_to_json(&existing),
                    );
                }
                Ok(None) => {}
                Err(e) => return KjResult::Err(format!("kj character create: {e}")),
            }
            // Resolve the model before the character exists, so a bad backend
            // name leaves no character behind.
            let model_row = match &model {
                None => None,
                Some((backend, model_id)) => {
                    if model_id.trim().is_empty() {
                        return KjResult::Err("kj character create: --model must not be empty".to_string());
                    }
                    match db.get_backend_by_name(backend) {
                        Ok(Some(b)) => Some((b.backend_id, model_id.clone())),
                        Ok(None) => {
                            return KjResult::Err(format!(
                                "kj character create: backend '{backend}' not found; nothing was created"
                            ));
                        }
                        Err(e) => return KjResult::Err(format!("kj character create: {e}")),
                    }
                }
            };

            let row = CharacterRow {
                principal_id: PrincipalId::new(),
                name: name.to_string(),
                created_at: kaijutsu_types::now_millis() as i64,
                retired_at: None,
                handoff_ctx: None, root_ctx: None,
                root,
                context_type: context_type.map(str::to_string),
            };
            if let Err(e) = db.insert_character(&row) {
                return KjResult::Err(format!("kj character create: {e}"));
            }
            if let Some((backend_id, model_id)) = model_row
                && let Err(e) = db.set_character_model(&crate::kernel_db::CharacterModelRow {
                    principal_id: row.principal_id,
                    backend_id,
                    model: model_id,
                    max_tokens: max_tokens.map(|v| v as i64),
                    temperature: None,
                    top_p: None,
                    effort: None,
                    thinking_budget: None,
                    thinking_style: None,
                })
            {
                return KjResult::Err(format!(
                    "kj character create: created {name} ({}) but could not set its model: {e}; \
                     `kj character model set {name} --backend B --model M` sets it",
                    row.principal_id.short()
                ));
            }
            row
        };
        if !root {
            let with_model = match &model {
                Some((backend, model_id)) => format!(" with model {backend}/{model_id}"),
                None => String::new(),
            };
            return KjResult::ok_with_data(
                format!("created {name} ({}){with_model}", row.principal_id.short()),
                character_to_json(&row),
            );
        }
        let root_ctx = match self.ensure_root_context(row.principal_id, caller).await {
            Ok(ctx) => ctx,
            Err(e) => {
                let committed = match self.kernel_db().lock().get_character(row.principal_id) {
                    Ok(Some(sheet)) => sheet.root_ctx,
                    Ok(None) => None,
                    Err(read_error) => return KjResult::Err(format!(
                        "kj character create: created {name} as a root, but its root-context \
                         lifecycle failed ({e}) and the committed state could not be read: \
                         {read_error}"
                    )),
                };
                return KjResult::Err(match committed {
                    Some(context) => format!(
                        "kj character create: created {name} as a root and committed root context \
                         {}, but its lifecycle did not complete: {e}",
                        context.short()
                    ),
                    None => format!(
                        "kj character create: created {name} as a root, but not its root context: {e}"
                    ),
                });
            }
        };
        let row = CharacterRow { root_ctx: Some(root_ctx), ..row };
        let hub = match self.ensure_mcp_hub(row.principal_id) {
            Ok(hub) => hub,
            Err(e) => return KjResult::Err(format!(
                "kj character create: created {name} as a root with root context {}, but not its MCP hub: {e}",
                root_ctx.short()
            )),
        };
        KjResult::ok_with_data(
            format!(
                "created {name} ({}) as a root, with root context '{name}' ({}) and MCP hub 'mcp/{name}' ({})",
                row.principal_id.short(),
                root_ctx.short(),
                hub.short()
            ),
            character_to_json(&row),
        )
    }

    /// Create the root context of each live root character that has none,
    /// and rebind loudly (`docs/character.md`) any that already has one but
    /// no usable loadout — e.g. a context created before its rc bucket
    /// existed. Every new context parents under a root, so an inert root
    /// deadlocks the whole tree beneath it: rebind needs a shell, a shell
    /// needs a loadout. The kernel calls this at start; a character it
    /// cannot repair fails boot rather than serve from a deadlock. Returns
    /// the contexts it created (not the ones it rebound). It also gives each
    /// live root its MCP hub ([`Self::ensure_mcp_hub`]).
    pub async fn ensure_root_contexts(&self, requester: PrincipalId) -> Result<Vec<ContextId>, String> {
        let live_roots: Vec<CharacterRow> = self
            .kernel_db()
            .lock()
            .list_characters(false)
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|row| row.root)
            .collect();
        let mut created = Vec::new();
        let startup_caller = KjCaller {
            principal_id: requester,
            actor_id: requester,
            reviewer_id: None,
            context_id: None,
            session_id: SessionId::new(),
            confirmed: false,
            rc_depth: 0,
            privileged: false,
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        for row in live_roots {
            match row.root_ctx {
                None => created.push(self.ensure_root_context(row.principal_id, &startup_caller).await?),
                Some(ctx) => self.rebind_root_context_if_unbound(&row, ctx, &startup_caller).await?,
            }
            // The hub is a view of the root's sessions: a label held by
            // another context, or a failed create, is reported and does not
            // stop boot.
            if let Err(e) = self.ensure_mcp_hub(row.principal_id) {
                tracing::error!(
                    character = %row.name,
                    character_id = %row.principal_id.short(),
                    "root character has no MCP hub: {e}",
                );
            }
        }
        Ok(created)
    }

    /// A root character's root context already exists — verify it still has
    /// a usable loadout, and rebind it (the same repair `kj context rebind`
    /// performs) when it does not. Warns loudly rather than repairing
    /// silently: a silent repair would hide exactly the gap ("the host rc
    /// tree lacked the `root` bucket when this context was created") that
    /// deadlocked boot in the first place. Refuses when the context still
    /// has no usable loadout afterward — serving with an inert root context
    /// is worse than not serving, since every new context forks beneath it.
    async fn rebind_root_context_if_unbound(
        &self,
        character: &CharacterRow,
        ctx: ContextId,
        caller: &KjCaller,
    ) -> Result<(), String> {
        match self.has_usable_loadout(ctx) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(e) => {
                return Err(format!(
                    "root character '{}' ({}) root context {}: could not read its loadout \
                     at boot, so there is no way to tell a repair from a re-run: {e}",
                    character.name,
                    character.principal_id.short(),
                    ctx.short()
                ));
            }
        }

        tracing::warn!(
            character = %character.name,
            character_id = %character.principal_id.short(),
            context = %ctx.short(),
            context_type = ROOT_CONTEXT_TYPE,
            "root context has no usable loadout at boot — rebinding via the rc create lifecycle",
        );

        let (row, admission) = {
            let db = self.kernel_db().lock();
            let row = db.get_context(ctx).map_err(|e| e.to_string())?.ok_or_else(|| {
                format!(
                    "root character '{}' root context {} not found",
                    character.name,
                    ctx.short()
                )
            })?;
            let admission = crate::runtime::admission::ContextAdmission::acquire(&db, ctx)
                .map_err(|e| e.to_string())?;
            (row, admission)
        };
        let rc_caller = root_lifecycle_caller(caller, character.principal_id, ctx);
        let outcome = self.rebind_loadout(ctx, row.forked_from, &admission, &rc_caller).await;
        if matches!(outcome, Ok(true)) {
            return Ok(());
        }
        let detail = match &outcome {
            Err(e) => format!(": {e}"),
            _ => String::new(),
        };
        Err(format!(
            "root character '{}' root context {} (type '{ROOT_CONTEXT_TYPE}') still has no \
             usable loadout after rebinding at boot{detail}. Expected rc bucket \
             /config/rc/{ROOT_CONTEXT_TYPE}/create — restore it (e.g. `kaijutsu-server rc \
             reseed`) and restart.",
            character.name,
            ctx.short()
        ))
    }

    /// Return a live root character's MCP hub, creating it when no live
    /// context holds the label `mcp/<name>`: type `mcp-hub`, created by the
    /// root, with no parent and no performer, answering to the root's console.
    /// It runs no turns; it is the `forked_from` of the root's MCP sessions
    /// (`docs/senpai-kohai.md`, "MCP sessions"). A live context of another
    /// type on the label is an error naming it.
    ///
    /// The lookup and the insert run under one `KernelDb` lock, so two
    /// callers cannot each make one.
    pub(crate) fn ensure_mcp_hub(&self, character: PrincipalId) -> Result<ContextId, String> {
        let new_id = ContextId::new();
        let (name, label) = {
            let db = self.kernel_db().lock();
            let sheet = db
                .get_character(character)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("character {} has no sheet", character.short()))?;
            if !sheet.root {
                return Err(format!("{} is not a root character", sheet.name));
            }
            if sheet.retired_at.is_some() {
                return Err(format!("{} is retired", sheet.name));
            }
            let label = format!("{MCP_HUB_LABEL_PREFIX}{}", sheet.name);
            if let Some(held) = db.find_context_by_label(&label).map_err(|e| e.to_string())? {
                if held.context_type == MCP_HUB_CONTEXT_TYPE {
                    return Ok(held.context_id);
                }
                return Err(format!(
                    "{} (type '{}') holds the label '{label}', which {}'s MCP hub needs; rename or archive it",
                    held.context_id.short(), held.context_type, sheet.name
                ));
            }
            let default_ws = db.get_or_create_default_workspace(character).map_err(|e| e.to_string())?;
            let console = sheet.root_ctx.ok_or_else(|| {
                format!("{} has no root console for its MCP hub to answer to", sheet.name)
            })?;
            let row = ContextRow {
                context_id: new_id,
                label: Some(label.clone()),
                system_prompt: None,
                context_state: ContextState::Live,
                context_type: MCP_HUB_CONTEXT_TYPE.to_string(),
                created_at: kaijutsu_types::now_millis() as i64,
                created_by: character,
                forked_from: None,
                fork_kind: None,
                archived_at: None,
                workspace_id: None,
                preset_id: None,
                concluded_at: None,
                last_activity_at: None,
                promoted_at: None,
                demoted_at: None,
                paused_at: None,
                origin_host: None,
                performer: None, senpai_ctx: Some(console), persona: None,
            };
            db.insert_placed_context(&row, default_ws)
                .map_err(|e| format!("could not create the MCP hub '{label}': {e}"))?;
            (sheet.name, label)
        };

        // `insert_context_with_document` commits the KernelDb document row
        // but does not seed the in-memory BlockStore. `DocumentAlreadyExists`
        // means another path already seeded it.
        match self.block_store().create_document(new_id, kaijutsu_types::DocKind::Conversation, None) {
            Ok(()) | Err(crate::block_store::BlockStoreError::DocumentAlreadyExists(_)) => {}
            Err(e) => return Err(format!("failed to seed the document of MCP hub '{label}': {e}")),
        }
        self.drift_router()
            .write()
            .register(new_id, Some(&label), None, character)
            .map_err(|e| format!("could not register MCP hub '{label}' for {name}: {e}"))?;
        Ok(new_id)
    }

    /// Return a live root character's root context, creating it when the
    /// sheet has none: type `root`, labeled with the character's name,
    /// played by it, with no parent. `requester` becomes `created_by`.
    async fn ensure_root_context(&self, character: PrincipalId, caller: &KjCaller) -> Result<ContextId, String> {
        let requester = caller.principal_id;
        let new_id = ContextId::new();
        let (name, admission) = {
            let db = self.kernel_db().lock();
            let sheet = db
                .get_character(character)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("character {} has no sheet", character.short()))?;
            if let Some(existing) = sheet.root_ctx {
                return Ok(existing);
            }
            if !sheet.root {
                return Err(format!("{} is not a root character", sheet.name));
            }
            if sheet.retired_at.is_some() {
                return Err(format!("{} is retired", sheet.name));
            }
            let row = ContextRow {
                context_id: new_id,
                label: Some(sheet.name.clone()),
                system_prompt: None,
                context_state: ContextState::Live,
                context_type: ROOT_CONTEXT_TYPE.to_string(),
                created_at: kaijutsu_types::now_millis() as i64,
                created_by: requester,
                forked_from: None,
                fork_kind: None,
                archived_at: None,
                workspace_id: None,
                preset_id: None,
                concluded_at: None,
                last_activity_at: None,
                promoted_at: None,
                demoted_at: None,
                paused_at: None,
                origin_host: None,
                performer: None, senpai_ctx: None, persona: None,
            };
            db.in_transaction(|db| {
                let default_ws = db.get_or_create_default_workspace(requester)?;
                db.insert_context_with_document(&row, default_ws)?;
                db.write_context_character(new_id, character, None, None)?;
                db.set_character_root_ctx(character, Some(new_id))?;
                Ok(())
            })
            .map_err(|e| format!("could not create the root context '{}': {e}", sheet.name))?;
            let admission = crate::runtime::admission::ContextAdmission::for_inserted(&row);
            (sheet.name, admission)
        };

        self.drift_router()
            .write()
            .register(new_id, Some(&name), None, requester)
            .map_err(|e| format!("could not register the root context '{name}': {e}"))?;

        let rc_caller = root_lifecycle_caller(caller, character, new_id);
        crate::rc::run(
            self,
            crate::rc::RcInvocation::new("create", &admission, &rc_caller.cancel),
            &rc_caller,
        )
            .await
            .map_err(|e| format!("root context '{name}' rc create lifecycle: {e}"))?;

        // A script failure lands as an Error block, not an `Err` above — the
        // lifecycle can "succeed" while binding nothing, e.g. an rc tree with
        // no `root` bucket at all (`load_scripts` reads an absent directory as
        // zero scripts, never a failure). Leaving that context behind inert
        // would deadlock every context forked beneath it (deny-by-default, and
        // every new context parents under a root). The row and its
        // `root_ctx` pointer stay committed on this Err — the character
        // already has a name for the context, and the next boot's
        // `ensure_root_contexts` rebind pass is the retry path once the
        // bucket exists, the same way `kj context rebind` repairs any other
        // unbound context.
        match self.has_usable_loadout(new_id) {
            Ok(true) => Ok(new_id),
            Ok(false) => Err(format!(
                "root context '{name}' ({}) was created but its rc create lifecycle bound \
                 no loadout — it would be inert, and it parents every context forked from \
                 this character. Expected rc bucket /config/rc/{ROOT_CONTEXT_TYPE}/create; \
                 restore it (e.g. `kaijutsu-server rc reseed`) — the committed context is \
                 left in place for the next boot's rebind to repair.",
                new_id.short()
            )),
            Err(e) => Err(format!(
                "root context '{name}' ({}) was created, but reading its loadout back to \
                 confirm the rc create lifecycle bound one failed: {e}",
                new_id.short()
            )),
        }
    }

    /// `list` → an array of full-id strings by default
    /// (`project_kj_structured_data` convention): a caller iterating the
    /// result never has to re-derive a truncated display id. `--json` opts
    /// into the full row per entry, `root` included, for a caller
    /// that wants the sheet without a `show` per name.
    fn character_list(&self, all: bool, json: bool) -> KjResult {
        let db = self.kernel_db().lock();
        let rows = match db.list_characters(all) {
            Ok(r) => r,
            Err(e) => return KjResult::Err(format!("kj character list: {e}")),
        };
        if json {
            let data = serde_json::Value::Array(rows.iter().map(character_to_json).collect());
            return KjResult::ok_with_data(data.to_string(), data);
        }
        let data = serde_json::Value::Array(
            rows.iter()
                .map(|r| serde_json::Value::String(r.principal_id.to_hex()))
                .collect(),
        );
        if rows.is_empty() {
            return KjResult::ok_with_data("(no characters yet)".to_string(), data);
        }
        let name_w = rows.iter().map(|r| r.name.len()).max().unwrap_or(0);
        let lines: Vec<String> = rows
            .iter()
            .map(|r| {
                let status = if r.retired_at.is_some() { "  (retired)" } else { "" };
                format!("  {:<name_w$}  {}{status}", r.name, r.principal_id.short())
            })
            .collect();
        KjResult::ok_with_data(lines.join("\n"), data)
    }

    /// `show` → an object carrying the whole row (`project_kj_structured_data`
    /// convention).
    fn character_show(&self, name: &str) -> KjResult {
        let db = self.kernel_db().lock();
        match db.get_character_by_name(name) {
            Ok(Some(row)) => {
                let slots = match db.list_character_slots(row.principal_id) {
                    Ok(slots) => slots,
                    Err(e) => return KjResult::Err(format!("kj character show: {e}")),
                };
                let backends: std::collections::HashMap<_, _> = match db.list_backends() {
                    Ok(b) => b.into_iter().map(|b| (b.backend_id, b.name)).collect(),
                    Err(e) => return KjResult::Err(format!("kj character show: {e}")),
                };
                let model = match db.get_character_model(row.principal_id) {
                    Ok(m) => m,
                    Err(e) => return KjResult::Err(format!("kj character show: {e}")),
                };
                let slot_names: Vec<String> = slots
                    .iter()
                    .map(|s| format!("{} → {}/{}", s.role, backends.get(&s.backend_id).map_or("(missing backend)", String::as_str), s.model))
                    .collect();
                let contexts = match db.character_live_contexts(row.principal_id) {
                    Ok(live) => live,
                    Err(e) => return KjResult::Err(format!("kj character show: {e}")),
                };
                let context_names: Vec<String> = contexts.iter().map(|c| db.context_name(*c)).collect();
                let mut text = format!(
                    "Character:       {}\nID:              {}\nRoot:            {}\n\
                     Type:            {}\nModel:           {}\nSlots:           {}\nCreated:         {}\nRetired:         {}",
                    row.name,
                    row.principal_id.to_hex(),
                    if row.root { "yes" } else { "no" },
                    row.context_type.as_deref().unwrap_or("-"),
                    model.as_ref().map_or_else(|| "-".to_string(), |m| super::slot::model_summary(m, &backends)),
                    if slot_names.is_empty() { "-".to_string() } else { slot_names.join(", ") },
                    super::format::format_timestamp(row.created_at),
                    row.retired_at
                        .map(super::format::format_timestamp)
                        .unwrap_or_else(|| "-".to_string()),
                );
                text.push_str(&format!("\nLive contexts:   {}", if context_names.is_empty() { "-".to_string() } else { context_names.join(", ") }));
                let mut data = character_to_json(&row);
                data["model"] = super::slot::model_data(model.as_ref(), &backends);
                data["slots"] = super::slot::slot_data(&slots, &backends);
                data["contexts"] = serde_json::Value::Array(
                    contexts.iter().map(|c| serde_json::json!({
                        "id": c.to_hex(),
                        "name": db.context_name(*c),
                        "senpai": db.senpai_of(*c).ok().flatten().map(|s| db.context_name(s)),
                    })).collect(),
                );
                KjResult::ok_with_data(text, data)
            }
            // A missing mapping is corruption, never a fallback to a
            // default identity — say so loudly rather than rendering
            // something that looks like an answer.
            Ok(None) => KjResult::Err(format!(
                "kj character show: no character named '{name}' — \
                 `kj character list` to see who exists"
            )),
            Err(e) => KjResult::Err(format!("kj character show: {e}")),
        }
    }

    /// Set or clear `root`. A root is a model context like any other: a
    /// model plays it, and its `kj` requests always need approval
    /// (`docs/character.md`, "Roots and rotation").
    async fn character_set(
        &self,
        name: &str,
        root: bool,
        no_root: bool,
        context_type: Option<&str>,
        no_type: bool,
        caller: &KjCaller,
    ) -> KjResult {
        if !root && !no_root && context_type.is_none() && !no_type {
            return KjResult::Err("kj character set: specify --root, --no-root, --type <type> or --no-type".to_string());
        }
        if root && context_type.is_some() {
            return KjResult::Err("kj character set: a root has no context type; drop --type, or drop --root".to_string());
        }
        if let Some(context_type) = context_type
            && let Err(e) = self.check_character_type(context_type).await
        {
            return KjResult::Err(format!("kj character set: --type: {e}"));
        }
        let principal_id = {
            let db = self.kernel_db().lock();
            let row = match db.get_character_by_name(name) {
                Ok(Some(r)) => r,
                Ok(None) => {
                    return KjResult::Err(format!(
                        "kj character set: no character named '{name}' — \
                         `kj character list` to see who exists"
                    ));
                }
                Err(e) => return KjResult::Err(format!("kj character set: {e}")),
            };
            if no_root && let Some(ctx) = row.root_ctx {
                match db.get_context(ctx) {
                    Ok(Some(context)) if !context.is_archived() => {
                        return KjResult::Err(format!(
                            "kj character set: {name} plays its live root context '{}' ({}), \
                             and a root context is played by a root; archive it first",
                            context.label.as_deref().unwrap_or(name),
                            ctx.short()
                        ));
                    }
                    Ok(_) => {}
                    Err(e) => return KjResult::Err(format!("kj character set: {e}")),
                }
            }
            if context_type.is_some() && row.root && !no_root {
                return KjResult::Err(format!(
                    "kj character set: {name} is a root, and a root has no context type; \
                     `kj character set {name} --no-root --type <type>` makes it an ordinary character first"
                ));
            }
            if root && row.context_type.is_some() {
                return KjResult::Err(format!(
                    "kj character set: {name} has a context type, and a root has none; \
                     `kj character set {name} --no-type` first"
                ));
            }
            if root {
                match db.get_character_model(row.principal_id) {
                    Ok(None) => {}
                    Ok(Some(_)) => {
                        return KjResult::Err(format!(
                            "kj character set: {name} has a model, and a root runs no model; \
                             `kj character model unset {name}` first"
                        ));
                    }
                    Err(e) => return KjResult::Err(format!("kj character set: {e}")),
                }
            }
            if (root || no_root) && let Err(e) = db.update_character_root(row.principal_id, root) {
                return KjResult::Err(format!("kj character set: {e}"));
            }
            if (context_type.is_some() || no_type)
                && let Err(e) = db.update_character_context_type(row.principal_id, context_type)
            {
                return KjResult::Err(format!("kj character set: {e}"));
            }
            row.principal_id
        };
        if root && let Err(e) = self.ensure_root_context(principal_id, caller).await {
            let committed = match self.kernel_db().lock().get_character(principal_id) {
                Ok(Some(sheet)) => sheet.root_ctx,
                Ok(None) => None,
                Err(read_error) => return KjResult::Err(format!(
                    "kj character set: {name} is now a root, but its root-context lifecycle \
                     failed ({e}) and the committed state could not be read: {read_error}"
                )),
            };
            return KjResult::Err(match committed {
                Some(context) => format!(
                    "kj character set: {name} is now a root and root context {} was committed, \
                     but its lifecycle did not complete: {e}",
                    context.short()
                ),
                None => format!(
                    "kj character set: {name} is now a root, but has no root context: {e}"
                ),
            });
        }
        if root && let Err(e) = self.ensure_mcp_hub(principal_id) {
            return KjResult::Err(format!("kj character set: {name} is now a root, but has no MCP hub: {e}"));
        }
        let updated = match self.kernel_db().lock().get_character(principal_id) {
            Ok(Some(r)) => r,
            Ok(None) => return KjResult::Err(format!("kj character set: {name} vanished")),
            Err(e) => return KjResult::Err(format!("kj character set: {e}")),
        };
        let mut changes = Vec::new();
        if root {
            changes.push("is now a root".to_string());
        } else if no_root {
            changes.push("is no longer a root".to_string());
        }
        if let Some(context_type) = context_type {
            changes.push(format!("usually plays {context_type}"));
        } else if no_type {
            changes.push("has no context type".to_string());
        }
        let msg = format!("{name} {}", changes.join(" and "));
        KjResult::ok_with_data(msg, character_to_json(&updated))
    }

    /// Retire: stamp `retired_at`, and conclude + archive every live
    /// context of the character's instances (conversations, forks and the
    /// contexts their rc lifecycles made), in the same act
    /// (`docs/character.md`, "Retire takes its contexts with it").
    /// `Destroy`-classed (`kj/effect.rs`); the dispatcher latches
    /// an unconfirmed call before this handler runs, whether or not the
    /// character has anything live to archive — the archival loop below is
    /// not separately confirmed, since that one latch already covers the
    /// whole batch as one act.
    async fn character_retire(&self, name: &str, caller: &KjCaller) -> KjResult {
        let live = {
            let db = self.kernel_db().lock();
            match db.get_character_by_name(name) {
                Ok(Some(row)) if row.retired_at.is_none() => match db.contexts_played_by(row.principal_id) {
                    Ok(live) => live,
                    Err(e) => return KjResult::Err(format!("kj character retire: {e}")),
                },
                _ => Vec::new(),
            }
        };
        for ctx in &live {
            if let Err(e) = self.run_archive_lifecycle(ctx.context_id, caller).await {
                return KjResult::Err(format!(
                    "kj character retire: {name} stays live, and {} stays live: {e}",
                    ctx.label.as_deref().unwrap_or(&ctx.context_id.short())
                ));
            }
        }
        self.character_retire_commit(name)
    }

    /// Retire after each played context's archive lifecycle has run.
    fn character_retire_commit(&self, name: &str) -> KjResult {
        let db = self.kernel_db().lock();
        let row = match db.get_character_by_name(name) {
            Ok(Some(r)) => r,
            Ok(None) => {
                return KjResult::Err(format!(
                    "kj character retire: no character named '{name}' — \
                     `kj character list` to see who exists"
                ));
            }
            Err(e) => return KjResult::Err(format!("kj character retire: {e}")),
        };
        if row.retired_at.is_some() {
            return KjResult::ok(format!("{name} already retired"));
        }
        let live = match db.contexts_played_by(row.principal_id) {
            Ok(l) => l,
            Err(e) => return KjResult::Err(format!("kj character retire: {e}")),
        };

        // Reuse the archive path `kj context archive` calls
        // (`KernelDb::conclude_context` / `archive_context`) rather than a
        // second archival implementation — conclude first (gives up the
        // ring-0 seat, same ladder `kj context archive` walks), then
        // archive (soft delete + sweep the context's unresolved asks).
        for ctx in &live {
            if let Err(e) = db.conclude_context(ctx.context_id) {
                return KjResult::Err(format!(
                    "kj character retire: failed to conclude {}: {e}",
                    ctx.context_id.short()
                ));
            }
            if let Err(e) = db.archive_context(ctx.context_id) {
                return KjResult::Err(format!(
                    "kj character retire: failed to archive {}: {e}",
                    ctx.context_id.short()
                ));
            }
        }
        let now = kaijutsu_types::now_millis() as i64;
        if let Err(e) = db.retire_character(row.principal_id, now) {
            return KjResult::Err(format!("kj character retire: {e}"));
        }
        drop(db);

        // The in-memory drift router must agree with the row, or an active
        // session could still write a drift op into a context whose
        // character no longer plays it (mirrors `kj context archive`).
        {
            let mut drift = self.drift_router().write();
            for ctx in &live {
                let _ = drift.set_state(ctx.context_id, ContextState::Archived);
            }
        }

        KjResult::ok(format!("retired {name}: {} context(s) archived", live.len()))
    }
}

fn character_to_json(row: &CharacterRow) -> serde_json::Value {
    serde_json::json!({
        "principal_id": row.principal_id.to_hex(),
        "name": row.name,
        "created_at": row.created_at,
        "retired_at": row.retired_at,
        "root": row.root,
        "root_ctx": row.root_ctx.map(|ctx| ctx.to_hex()),
        "context_type": row.context_type,
    })
}

// Verb class: kj/effect.rs
use super::effect::{Classify, Effect};

impl Classify for CharacterArgs {
    fn effect(&self) -> Effect {
        self.command.effect()
    }
}

impl Classify for CharacterCommand {
    fn effect(&self) -> Effect {
        match self {
            Self::List { .. } | Self::Show { .. } => Effect::Read,
            Self::Create { .. } | Self::Set { .. } => Effect::Write,
            Self::Retire { .. } => Effect::Destroy,
            Self::Model(cmd) => cmd.effect(),
            Self::Slot(cmd) => cmd.effect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::{caller_with_context, register_context, test_caller};
    use super::super::KjResult;
    use crate::mcp::ContextToolBinding;
    use kaijutsu_types::PrincipalId;

    fn s(x: &str) -> String {
        x.to_string()
    }

    /// `test_dispatcher()` alone cannot actually bind anything: `.kai`
    /// scripts reach the `kj` builtin only through `self_arc`
    /// (`EmbeddedKaish::for_context` refuses without it), and `kj binding
    /// allow` persists only through the broker's db handle, which
    /// `test_dispatcher()` leaves unset. Any test that needs a root
    /// context's rc `create` lifecycle to really bind something — not just
    /// run without hard-erroring — needs both wired first, the same way
    /// `context::rebind_tests` does for `kj context rebind`.
    async fn rc_capable_dispatcher() -> std::sync::Arc<super::super::KjDispatcher> {
        let d = std::sync::Arc::new(super::super::test_helpers::test_dispatcher().await);
        d.set_self_arc();
        d.kernel().broker().set_db(d.kernel_db().clone()).await;
        d
    }

    #[test]
    fn root_lifecycle_caller_preserves_session_depth_and_owner() {
        let mut caller = test_caller();
        caller.rc_depth = 3;
        let session = caller.session_id;
        let character = PrincipalId::new();
        let context = kaijutsu_types::ContextId::new();

        let derived = super::root_lifecycle_caller(&caller, character, context);

        assert_eq!(derived.principal_id, caller.principal_id);
        assert_eq!(derived.actor_id, character);
        assert_eq!(derived.context_id, Some(context));
        assert_eq!(derived.session_id, session);
        assert_eq!(derived.rc_depth, 3);
        caller.cancel.cancel();
        assert!(derived.cancel.is_cancelled(), "the root lifecycle keeps its invoking owner");
    }

    #[tokio::test]
    async fn cancelled_root_creation_retains_committed_context_and_reports_partial_result() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        caller.cancel.cancel();

        let result = d
            .dispatch(&[s("character"), s("create"), s("cancelled-root"), s("--root")], &caller)
            .await;

        assert!(!result.is_ok(), "cancelled root lifecycle must be reported");
        assert!(result.message().contains("rc create lifecycle"), "{}", result.message());
        let sheet = d.kernel_db().lock()
            .get_character_by_name("cancelled-root").unwrap().expect("character committed");
        let context = sheet.root_ctx.expect("root context committed before cancellation");
        assert!(d.kernel_db().lock().get_context(context).unwrap().is_some());
    }

    /// `create` mints a fresh principal and a sheet row.
    #[tokio::test]
    async fn create_mints_a_character() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        let result = d.dispatch(&[s("character"), s("create"), s("hajime")], &caller).await;
        let KjResult::Ok { data: Some(serde_json::Value::Object(obj)), .. } = result else {
            panic!("expected Ok with an object, got {result:?}");
        };
        assert_eq!(obj["name"], "hajime");
        assert!(obj["principal_id"].as_str().unwrap().len() == 32, "expected a full hex id");
        assert!(obj["retired_at"].is_null());

        let row = d.kernel_db().lock().get_character_by_name("hajime").unwrap();
        assert!(row.is_some(), "the row must actually be persisted");
    }

    /// `create` on an existing name returns the SAME principal id rather
    /// than minting a second one for the same name.
    #[tokio::test]
    async fn create_is_idempotent_on_name() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        let first = d.dispatch(&[s("character"), s("create"), s("hajime")], &caller).await;
        let KjResult::Ok { data: Some(serde_json::Value::Object(first_obj)), .. } = first else {
            panic!("expected Ok, got {first:?}");
        };
        let second = d.dispatch(&[s("character"), s("create"), s("hajime")], &caller).await;
        let KjResult::Ok { data: Some(serde_json::Value::Object(second_obj)), .. } = second else {
            panic!("expected Ok, got {second:?}");
        };
        assert_eq!(
            first_obj["principal_id"], second_obj["principal_id"],
            "a repeated create must return the SAME principal, never mint a second one"
        );

        let all = d.kernel_db().lock().list_characters(true).unwrap();
        assert_eq!(all.iter().filter(|sheet| sheet.name == "hajime").count(), 1, "only one row must exist for the name");
    }

    /// `--type` on create and set stores the type; show prints it and carries
    /// it in `.data.context_type`; `--no-type` clears it.
    ///
    /// Falsified by dropping the column from the read, or by ignoring `--no-type`.
    #[tokio::test]
    async fn the_context_type_is_set_shown_and_cleared() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        let made = d.dispatch(&[s("character"), s("create"), s("worker"), s("--type"), s("coder")], &caller).await;
        assert!(made.is_ok(), "{}", made.message());
        assert_eq!(d.kernel_db().lock().get_character_by_name("worker").unwrap().unwrap().context_type.as_deref(), Some("coder"));

        let shown = d.dispatch(&[s("character"), s("show"), s("worker")], &caller).await;
        let KjResult::Ok { message, data: Some(data), .. } = shown else { panic!("expected Ok, got {shown:?}") };
        assert!(message.contains("Type:") && message.contains("coder"), "{message}");
        assert_eq!(data["context_type"], "coder");

        let set = d.dispatch(&[s("character"), s("set"), s("worker"), s("--type"), s("default")], &caller).await;
        assert!(set.is_ok(), "{}", set.message());
        assert_eq!(d.kernel_db().lock().get_character_by_name("worker").unwrap().unwrap().context_type.as_deref(), Some("default"));

        let cleared = d.dispatch(&[s("character"), s("set"), s("worker"), s("--no-type")], &caller).await;
        assert!(cleared.is_ok(), "{}", cleared.message());
        let shown = d.dispatch(&[s("character"), s("show"), s("worker")], &caller).await;
        let KjResult::Ok { message, data: Some(data), .. } = shown else { panic!("expected Ok, got {shown:?}") };
        assert!(message.contains("Type:") && !message.contains("default"), "{message}");
        assert!(data["context_type"].is_null());
    }

    /// An unknown type or a reserved role is refused as `kj context create
    /// --type` refuses it, and nothing is stored.
    #[tokio::test]
    async fn a_reserved_or_unknown_context_type_is_refused() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        for reserved in ["distill", "gate", "judge"] {
            let made = d.dispatch(&[s("character"), s("create"), s("worker"), s("--type"), s(reserved)], &caller).await;
            assert!(!made.is_ok() && made.message().contains("slot role"), "{}", made.message());
        }
        assert!(d.kernel_db().lock().get_character_by_name("worker").unwrap().is_none(), "a refused create leaves no character");
        let typo = d.dispatch(&[s("character"), s("create"), s("worker"), s("--type"), s("codr")], &caller).await;
        assert!(!typo.is_ok() && typo.message().contains("codr"), "{}", typo.message());
        d.dispatch(&[s("character"), s("create"), s("worker")], &caller).await;
        let set = d.dispatch(&[s("character"), s("set"), s("worker"), s("--type"), s("gate")], &caller).await;
        assert!(!set.is_ok() && set.message().contains("slot role"), "{}", set.message());
        assert_eq!(d.kernel_db().lock().get_character_by_name("worker").unwrap().unwrap().context_type, None);
        // Creating an existing name stays idempotent, whatever its --type says.
        let again = d.dispatch(&[s("character"), s("create"), s("worker"), s("--type"), s("codr")], &caller).await;
        assert!(again.is_ok() && again.message().contains("already exists"), "{}", again.message());
    }

    /// A root has no type, at create, at set --type, and at set --root.
    #[tokio::test]
    async fn a_root_character_refuses_a_context_type() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        let made = d.dispatch(&[s("character"), s("create"), s("boss"), s("--root"), s("--type"), s("coder")], &caller).await;
        assert!(!made.is_ok() && made.message().contains("root"), "{}", made.message());
        assert!(d.kernel_db().lock().get_character_by_name("boss").unwrap().is_none(), "nothing was created");

        d.dispatch(&[s("character"), s("create"), s("typed"), s("--type"), s("coder")], &caller).await;
        let to_root = d.dispatch(&[s("character"), s("set"), s("typed"), s("--root")], &caller).await;
        assert!(!to_root.is_ok() && to_root.message().contains("--no-type"), "{}", to_root.message());
        assert!(!d.kernel_db().lock().get_character_by_name("typed").unwrap().unwrap().root);

        d.dispatch(&[s("character"), s("create"), s("plain")], &caller).await;
        d.dispatch(&[s("character"), s("set"), s("plain"), s("--root")], &caller).await;
        let typed_root = d.dispatch(&[s("character"), s("set"), s("plain"), s("--type"), s("coder")], &caller).await;
        assert!(!typed_root.is_ok() && typed_root.message().contains("root"), "{}", typed_root.message());
    }

    /// `set` names the options it takes when given none.
    #[tokio::test]
    async fn set_with_no_option_names_all_of_them() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        d.dispatch(&[s("character"), s("create"), s("worker")], &caller).await;
        let result = d.dispatch(&[s("character"), s("set"), s("worker")], &caller).await;
        let KjResult::Err(message) = result else { panic!("expected a refusal, got {result:?}") };
        assert!(message.contains("--root") && message.contains("--type") && message.contains("--no-type"), "{message}");
    }

    /// The published help gives the type's default and effect.
    #[tokio::test]
    async fn character_help_describes_the_type() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        for verb in ["create", "set"] {
            let help = d.dispatch(&[s("character"), s(verb), s("--help")], &caller).await;
            assert!(help.is_ok(), "{}", help.message());
            println!("{}", help.message());
            assert!(help.message().contains("--type <CONTEXT_TYPE>"), "{}", help.message());
            assert!(help.message().contains("kj context create"), "{}", help.message());
        }
    }

    /// `list` surfaces only live characters by default, as a full-id array.
    #[tokio::test]
    async fn list_shows_live_characters_as_full_ids() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        d.dispatch(&[s("character"), s("create"), s("amy")], &caller).await;
        let result = d.dispatch(&[s("character"), s("list")], &caller).await;
        let KjResult::Ok { data: Some(serde_json::Value::Array(rows)), .. } = result else {
            panic!("expected a JSON array, got {result:?}");
        };
        assert_eq!(rows.len(), 2, "amy, and the performer fixture the test dispatcher seeds");
        assert!(rows.iter().all(|row| row.as_str().unwrap().len() == 32), "list must carry full ids, never short forms");
    }

    /// `show` on an unknown name fails loudly — no silent fallback to a
    /// default identity or to `system`.
    #[tokio::test]
    async fn show_unknown_name_fails_loudly() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        let result = d.dispatch(&[s("character"), s("show"), s("nobody")], &caller).await;
        assert!(matches!(result, KjResult::Err(_)), "expected Err, got {result:?}");
    }

    /// A non-privileged caller with no `ConfigWrite` grant cannot mint a
    /// character.
    #[tokio::test]
    async fn create_denied_without_config_write() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("no-config-write"), None, principal);
        // Narrow the loadout register_context grants by default.
        d.kernel_db()
            .lock()
            .upsert_context_binding(ctx, &ContextToolBinding::new())
            .unwrap();
        let caller = caller_with_context(ctx);
        let result = d.dispatch(&[s("character"), s("create"), s("intruder")], &caller).await;
        assert!(matches!(result, KjResult::Err(_)), "expected denial, got {result:?}");
        assert!(d.kernel_db().lock().get_character_by_name("intruder").unwrap().is_none());
    }

    /// `retire` latches an unconfirmed call even with no live contexts to
    /// archive — `Destroy` always latches (`kj/effect.rs`, slice
    /// 3), regardless of what the handler would find. Confirmed, it
    /// completes on the next call.
    #[tokio::test]
    async fn retire_with_no_contexts_still_latches_then_completes() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        d.dispatch(&[s("character"), s("create"), s("hajime")], &caller).await;
        let latch = d.dispatch(&[s("character"), s("retire"), s("hajime")], &caller).await;
        assert!(latch.is_latch(), "expected a latch even with no live contexts, got {latch:?}");

        let mut confirmed = caller.clone();
        confirmed.confirmed = true;
        let result =
            d.dispatch(&[s("character"), s("retire"), s("hajime")], &confirmed).await;
        assert!(matches!(result, KjResult::Ok { .. }), "{result:?}");
        let row = d.kernel_db().lock().get_character_by_name("hajime").unwrap().unwrap();
        assert!(row.retired_at.is_some());
    }

    /// Retiring a character with live contexts latches first, then — once
    /// confirmed — concludes and archives every one of them, leaving their
    /// blocks and block authors untouched (no attribution change in this
    /// slice).
    #[tokio::test]
    async fn retire_archives_live_contexts_and_leaves_blocks_untouched() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        d.dispatch(&[s("character"), s("create"), s("kaijutsu-lead")], &caller).await;
        let principal_id = {
            let db = d.kernel_db().lock();
            db.get_character_by_name("kaijutsu-lead").unwrap().unwrap().principal_id
        };

        // A live context played by the character, with a block authored by
        // it — the thing retire must leave untouched.
        let ctx = register_context(&d, Some("lead-ctx"), None, principal_id);
        d.kernel_db().lock().update_played_by(ctx, Some(principal_id)).unwrap();
        d.block_store()
            .create_document(ctx, kaijutsu_types::DocKind::Conversation, None)
            .unwrap();
        d.block_store()
            .insert_block_as(
                ctx,
                None,
                None,
                kaijutsu_types::Role::User,
                kaijutsu_types::BlockKind::Text,
                "hello from the lead",
                kaijutsu_types::Status::Done,
                kaijutsu_types::ContentType::Plain,
                Some(principal_id),
            )
            .unwrap();

        let latch =
            d.dispatch(&[s("character"), s("retire"), s("kaijutsu-lead")], &caller).await;
        assert!(latch.is_latch(), "expected a latch with live contexts pending, got {latch:?}");

        let mut confirmed = caller.clone();
        confirmed.confirmed = true;
        let result =
            d.dispatch(&[s("character"), s("retire"), s("kaijutsu-lead")], &confirmed).await;
        assert!(matches!(result, KjResult::Ok { .. }), "{result:?}");

        let ctx_row = d.kernel_db().lock().get_context(ctx).unwrap().unwrap();
        assert!(ctx_row.archived_at.is_some(), "the character's live context must be archived");
        assert!(ctx_row.concluded_at.is_some(), "and concluded on the way");

        let char_row = d.kernel_db().lock().get_character_by_name("kaijutsu-lead").unwrap().unwrap();
        assert!(char_row.retired_at.is_some());

        // The block and its author are untouched — no attribution change.
        let blocks = d.block_store().block_snapshots(ctx).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].author(), principal_id, "block authorship must not change");
    }

    /// Retire archives every live context the character performs, forks
    /// included, and nothing another character performs, even a context that
    /// answers to it.
    #[tokio::test]
    async fn retire_archives_the_contexts_the_character_performs_only() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        let [lead, coder] = [PrincipalId::new(), PrincipalId::new()];
        for (principal_id, name) in [(lead, "lead"), (coder, "coder")] {
            d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow { context_type: None,
                principal_id, name: name.into(), created_at: 0, retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
            }).unwrap();
        }
        let seat = register_context(&d, Some("lead-seat"), None, lead);
        d.kernel_db().lock().update_played_by(seat, Some(lead)).unwrap();
        let fork = register_context(&d, Some("lead-fork"), Some(seat), lead);
        let lane = register_context(&d, Some("coder-lane"), Some(seat), lead);
        {
            let db = d.kernel_db().lock();
            db.copy_context_character(seat, fork).unwrap().expect("the seat has a context character");
            db.update_played_by(lane, Some(coder)).unwrap();
        }
        let mut confirmed = caller.clone();
        confirmed.confirmed = true;
        let result = d.dispatch(&[s("character"), s("retire"), s("lead")], &confirmed).await;
        assert!(result.is_ok(), "{}", result.message());
        assert!(result.message().contains("2 context(s) archived"), "{}", result.message());
        let db = d.kernel_db().lock();
        assert!(db.get_context(seat).unwrap().unwrap().is_archived());
        assert!(db.get_context(fork).unwrap().unwrap().is_archived(), "a fork of the conversation goes with it");
        assert!(!db.get_context(lane).unwrap().unwrap().is_archived(), "another character's context stays");
    }

    /// `show` lists the contexts the character performs that are live; an
    /// archived context is not listed.
    #[tokio::test]
    async fn show_lists_the_characters_live_contexts() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let coder = PrincipalId::new();
        d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow { context_type: None,
            principal_id: coder, name: "coder".into(), created_at: 0, retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
        }).unwrap();
        let live = register_context(&d, Some("lane-live"), None, coder);
        let gone = register_context(&d, Some("lane-gone"), None, coder);
        {
            let db = d.kernel_db().lock();
            db.update_played_by(live, Some(coder)).unwrap();
            db.update_played_by(gone, Some(coder)).unwrap();
            db.archive_context(gone).unwrap();
        }
        let shown = d.dispatch(&[s("character"), s("show"), s("coder")], &test_caller()).await;
        assert!(shown.is_ok(), "{}", shown.message());
        assert!(shown.message().contains(&format!("Live contexts:   lane-live ({})", live.short())), "{}", shown.message());
        assert!(!shown.message().contains("lane-gone"), "{}", shown.message());
        let KjResult::Ok { data: Some(data), .. } = shown else { panic!("expected data") };
        assert_eq!(data["contexts"].as_array().unwrap().len(), 1);
        assert_eq!(data["contexts"][0]["name"], format!("lane-live ({})", live.short()));
    }

    /// A NULL `played_by` is not a marker some code path special-cases —
    /// `kj context info` on an ordinary context (nobody playing it) must
    /// behave exactly as it did before this column existed.
    #[tokio::test]
    async fn null_played_by_preserves_context_info() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("plain-ctx"), None, principal);
        let caller = caller_with_context(ctx);
        let result = d.dispatch(&[s("context"), s("info")], &caller).await;
        let KjResult::Ok { data: Some(data), .. } = result else {
            panic!("expected Ok, got {result:?}");
        };
        assert_eq!(data["played_by"], serde_json::Value::Null);
        assert_eq!(data["played_by_name"], serde_json::Value::Null);
    }

    /// The emitted help is a product surface: every subcommand and flag
    /// this module publishes is read by a player, so the text is pinned
    /// here rather than left to drift (`docs/writing.md`).
    #[tokio::test]
    async fn published_help_names_root_and_no_sheet_relation() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        let KjResult::Ok { message, .. } = d.dispatch(&[s("character"), s("--help")], &caller).await
        else { panic!("help must render") };
        println!("{message}");
        assert!(!message.contains("accountable"), "the sheet chain is gone: {message}");
        for (argv, expected) in [
            (vec![s("character"), s("create"), s("--help")], "--root"),
            (vec![s("character"), s("set"), s("--help")], "--no-root"),
        ] {
            let KjResult::Ok { message, .. } = d.dispatch(&argv, &caller).await
            else { panic!("help must render for {argv:?}") };
            println!("{message}");
            assert!(message.contains(expected), "{message}");
            assert!(!message.contains("accountable"), "{message}");
        }
    }

    /// `create --root` writes the flag, and `show`/`list --json` both
    /// carry it. An ordinary create is not a root.
    #[tokio::test]
    async fn create_root_flag_reaches_the_sheet_and_every_reader() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        d.dispatch(&[s("character"), s("create"), s("sovereign"), s("--root")], &caller).await;
        d.dispatch(&[s("character"), s("create"), s("coder")], &caller).await;

        assert!(d.kernel_db().lock().get_character_by_name("sovereign").unwrap().unwrap().root);
        assert!(!d.kernel_db().lock().get_character_by_name("coder").unwrap().unwrap().root);

        let KjResult::Ok { message, .. } = d.dispatch(&[s("character"), s("show"), s("sovereign")], &caller).await
        else { panic!("show must succeed") };
        assert!(message.contains("Root:            yes"), "{message}");
        let KjResult::Ok { message, .. } = d.dispatch(&[s("character"), s("show"), s("coder")], &caller).await
        else { panic!("show must succeed") };
        assert!(message.contains("Root:            no"), "{message}");

        let result = d.dispatch(&[s("character"), s("list"), s("--json")], &caller).await;
        let KjResult::Ok { data: Some(serde_json::Value::Array(rows)), .. } = result else {
            panic!("expected a JSON array, got {result:?}");
        };
        let row = |name: &str| rows.iter().find(|r| r["name"] == name).unwrap_or_else(|| panic!("{name} present")).clone();
        assert_eq!(row("sovereign")["root"], serde_json::Value::Bool(true));
        assert_eq!(row("coder")["root"], serde_json::Value::Bool(false));
    }

    /// `create --root` creates the character's root context: type `root`,
    /// labeled with its name, played by it, with no parent, recorded on the
    /// sheet. `tests/rc_role_bindings.rs` checks that its rc bundle binds it.
    #[tokio::test]
    async fn create_root_makes_its_root_context() {
        let d = rc_capable_dispatcher().await;
        let caller = test_caller();
        let created = d.dispatch(&[s("character"), s("create"), s("sovereign"), s("--root")], &caller).await;
        assert!(matches!(created, KjResult::Ok { .. }), "{created:?}");

        let sheet = d.kernel_db().lock().get_character_by_name("sovereign").unwrap().unwrap();
        let root_ctx = sheet.root_ctx.expect("a root character records its root context");
        assert_eq!(
            d.has_usable_loadout(root_ctx),
            Ok(true),
            "creation refuses loudly rather than leaving an inert root behind"
        );
        let row = d.kernel_db().lock().get_context(root_ctx).unwrap().unwrap();
        assert_eq!(row.label.as_deref(), Some("sovereign"));
        assert_eq!(row.context_type, "root");
        assert_eq!(row.performer, Some(sheet.principal_id));
        assert_eq!(row.forked_from, None);
        assert_eq!(row.created_by, caller.principal_id, "the requester stays the creator");
        assert!(row.archived_at.is_none());
        assert_eq!(d.kernel_db().lock().senpai_of(root_ctx).unwrap(), None, "a root console answers to nobody: it confirms itself");
        {
            let db = d.kernel_db().lock();
            let character = db.context_character(root_ctx).unwrap().expect("the root console has a context character");
            assert_eq!((character.principal_id, character.senpai_ctx), (sheet.principal_id, None), "a root console has no senpai");
            assert_eq!(db.root_console(sheet.principal_id).unwrap(), Some(root_ctx));
        }

        let KjResult::Ok { data: Some(data), .. } =
            d.dispatch(&[s("character"), s("show"), s("sovereign")], &caller).await
        else { panic!("show must succeed") };
        assert_eq!(data["root_ctx"], serde_json::Value::String(root_ctx.to_hex()));

        let again = d.dispatch(&[s("character"), s("create"), s("sovereign"), s("--root")], &caller).await;
        assert!(matches!(again, KjResult::Ok { .. }), "{again:?}");
        let live_roots = d.kernel_db().lock().list_all_contexts().unwrap()
            .into_iter().filter(|c| c.context_type == "root" && c.archived_at.is_none()).count();
        assert_eq!(live_roots, 1, "a repeated create must not make a second root context");
    }

    /// The MCP hub of `name`: its live context labeled `mcp/<name>`.
    fn hub_of(d: &super::super::KjDispatcher, name: &str) -> Option<crate::kernel_db::ContextRow> {
        d.kernel_db().lock().find_context_by_label(&format!("mcp/{name}")).unwrap()
    }

    /// `create --root` makes the root's MCP hub beside its console: type
    /// `mcp-hub`, created by the root, no parent, no context character, and
    /// registered with the drift router. Repeating the create makes no second.
    ///
    /// Falsified by dropping the hub from the create path.
    #[tokio::test]
    async fn create_root_makes_its_mcp_hub() {
        let d = rc_capable_dispatcher().await;
        let caller = test_caller();
        let created = d.dispatch(&[s("character"), s("create"), s("sovereign"), s("--root")], &caller).await;
        assert!(matches!(created, KjResult::Ok { .. }), "{created:?}");

        let sheet = d.kernel_db().lock().get_character_by_name("sovereign").unwrap().unwrap();
        let hub = hub_of(&d, "sovereign").expect("a root character has an MCP hub");
        assert_eq!(hub.context_type, "mcp-hub");
        assert_eq!(hub.created_by, sheet.principal_id, "the root creates its own hub");
        assert_eq!(hub.forked_from, None);
        assert!(hub.archived_at.is_none());
        assert!(d.kernel_db().lock().context_character(hub.context_id).unwrap().is_none(), "a hub has no performer");
        assert_eq!(hub.senpai_ctx, sheet.root_ctx, "a hub answers to its root's console");
        assert_eq!(d.kernel_db().lock().lineage_root(hub.context_id).unwrap(), sheet.principal_id);
        assert!(d.drift_router().read().get(hub.context_id).is_some(), "the hub is registered with the drift router");
        assert_ne!(Some(hub.context_id), sheet.root_ctx);

        let again = d.dispatch(&[s("character"), s("create"), s("sovereign"), s("--root")], &caller).await;
        assert!(matches!(again, KjResult::Ok { .. }), "{again:?}");
        let hubs = d.kernel_db().lock().list_all_contexts().unwrap()
            .into_iter().filter(|c| c.context_type == "mcp-hub" && c.archived_at.is_none()).count();
        assert_eq!(hubs, 1, "a repeated create must not make a second hub");
    }

    /// An ordinary character gets no hub.
    #[tokio::test]
    async fn ordinary_create_makes_no_mcp_hub() {
        let d = rc_capable_dispatcher().await;
        d.dispatch(&[s("character"), s("create"), s("worker")], &test_caller()).await;
        assert!(hub_of(&d, "worker").is_none());
    }

    /// `create --root` refuses, naming the context, when a live context of
    /// another type holds the hub's label.
    #[tokio::test]
    async fn create_root_refuses_when_another_context_holds_the_hub_label() {
        let d = rc_capable_dispatcher().await;
        let squatter = super::super::test_helpers::register_context(&d, Some("mcp/sovereign"), None, PrincipalId::new());
        let created = d.dispatch(&[s("character"), s("create"), s("sovereign"), s("--root")], &test_caller()).await;
        let KjResult::Err(message) = created else { panic!("expected a refusal, got {created:?}") };
        assert!(message.contains("mcp/sovereign") && message.contains(&squatter.short()), "{message}");
        assert!(hub_of(&d, "sovereign").is_some_and(|row| row.context_id == squatter), "the squatter is left alone");
    }

    /// At start every live root gets a hub, including one whose console
    /// already exists; a second start makes no second hub; a retired root
    /// gets none.
    ///
    /// Falsified by dropping the hub from `ensure_root_contexts`.
    #[tokio::test]
    async fn ensure_root_contexts_makes_each_live_root_one_hub() {
        let d = rc_capable_dispatcher().await;
        let keeper = PrincipalId::new();
        let retired = PrincipalId::new();
        {
            let db = d.kernel_db().lock();
            for (principal_id, name, retired_at) in [(keeper, "keeper", None), (retired, "old", Some(1))] {
                db.insert_character(&crate::kernel_db::CharacterRow { context_type: None,
                    principal_id, name: name.into(), created_at: 0, retired_at,
                    handoff_ctx: None, root_ctx: None, root: true,
                }).unwrap();
            }
        }
        d.ensure_root_contexts(PrincipalId::system()).await.unwrap();
        let amy_name = d.kernel_db().lock().get_character(super::super::test_helpers::test_reviewer_principal()).unwrap().unwrap().name;
        let hubs: Vec<_> = [amy_name.as_str(), "keeper"].map(|n| hub_of(&d, n).unwrap_or_else(|| panic!("{n} has a hub"))).into();
        assert!(hubs.iter().all(|h| h.context_type == "mcp-hub" && h.forked_from.is_none()));
        assert!(hub_of(&d, "old").is_none(), "a retired root gets no hub");

        d.ensure_root_contexts(PrincipalId::system()).await.unwrap();
        let live_hubs = d.kernel_db().lock().list_all_contexts().unwrap()
            .into_iter().filter(|c| c.context_type == "mcp-hub" && c.archived_at.is_none()).count();
        assert_eq!(live_hubs, 2, "a second start makes no second hub");
        assert_eq!(hub_of(&d, "keeper").unwrap().context_id, hubs[1].context_id, "the hub is reused");
    }

    /// A root whose console exists but has no hub (a kernel from before the
    /// hub) gets one at start.
    #[tokio::test]
    async fn start_makes_the_hub_for_a_root_whose_console_exists() {
        let d = rc_capable_dispatcher().await;
        d.dispatch(&[s("character"), s("create"), s("sovereign"), s("--root")], &test_caller()).await;
        let hub = hub_of(&d, "sovereign").unwrap();
        {
            let db = d.kernel_db().lock();
            db.archive_context(hub.context_id).unwrap();
        }
        assert!(hub_of(&d, "sovereign").is_none(), "precondition: no live hub");
        d.ensure_root_contexts(PrincipalId::system()).await.unwrap();
        let made = hub_of(&d, "sovereign").expect("start makes the missing hub");
        assert_ne!(made.context_id, hub.context_id);
    }

    /// At start a live context of another type on the hub's label is
    /// logged and left alone; boot continues and the other roots still get
    /// their hubs.
    #[tokio::test]
    async fn start_continues_when_another_context_holds_the_hub_label() {
        let d = rc_capable_dispatcher().await;
        let amy = super::super::test_helpers::test_reviewer_principal();
        let amy_name = d.kernel_db().lock().get_character(amy).unwrap().unwrap().name;
        let squatter = super::super::test_helpers::register_context(&d, Some(&format!("mcp/{amy_name}")), None, PrincipalId::new());
        d.ensure_root_contexts(PrincipalId::system()).await.expect("a held hub label must not stop boot");
        assert_eq!(hub_of(&d, &amy_name).unwrap().context_id, squatter);
    }

    /// An ordinary character gets no context at create.
    #[tokio::test]
    async fn ordinary_create_makes_no_context() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        let before = d.kernel_db().lock().list_all_contexts().unwrap().len();
        d.dispatch(&[s("character"), s("create"), s("coder")], &caller).await;
        let sheet = d.kernel_db().lock().get_character_by_name("coder").unwrap().unwrap();
        assert_eq!(sheet.root_ctx, None);
        assert_eq!(d.kernel_db().lock().list_all_contexts().unwrap().len(), before);
    }

    /// `set --root` creates the root context the flag requires, and
    /// `set --no-root` refuses while that context is live, since a root
    /// context is played by a root.
    #[tokio::test]
    async fn set_root_makes_the_root_context_and_no_root_refuses_while_live() {
        let d = rc_capable_dispatcher().await;
        let caller = test_caller();
        d.dispatch(&[s("character"), s("create"), s("laptop")], &caller).await;
        let set = d.dispatch(&[s("character"), s("set"), s("laptop"), s("--root")], &caller).await;
        assert!(matches!(set, KjResult::Ok { .. }), "{set:?}");
        let sheet = d.kernel_db().lock().get_character_by_name("laptop").unwrap().unwrap();
        let root_ctx = sheet.root_ctx.expect("set --root creates the root context");
        assert_eq!(d.kernel_db().lock().get_context(root_ctx).unwrap().unwrap().label.as_deref(), Some("laptop"));

        let cleared = d.dispatch(&[s("character"), s("set"), s("laptop"), s("--no-root")], &caller).await;
        let KjResult::Err(message) = cleared else { panic!("expected a refusal, got {cleared:?}") };
        assert!(message.contains("root context"), "{message}");
        assert!(d.kernel_db().lock().get_character_by_name("laptop").unwrap().unwrap().root);
    }

    /// At start the kernel creates a missing root context for each live
    /// root character, and a second pass creates nothing.
    #[tokio::test]
    async fn ensure_root_contexts_fills_missing_and_is_idempotent() {
        let d = rc_capable_dispatcher().await;
        let keeper = PrincipalId::new();
        let retired = PrincipalId::new();
        {
            let db = d.kernel_db().lock();
            for (principal_id, name, retired_at) in [(keeper, "keeper", None), (retired, "old", Some(1))] {
                db.insert_character(&crate::kernel_db::CharacterRow { context_type: None,
                    principal_id, name: name.into(), created_at: 0, retired_at,
                    handoff_ctx: None, root_ctx: None, root: true,
                }).unwrap();
            }
        }

        let mut created = d.ensure_root_contexts(PrincipalId::system()).await.unwrap();
        let amy = super::super::test_helpers::test_reviewer_principal();
        let live_roots = [keeper, amy].map(|root| d.kernel_db().lock().get_character(root).unwrap().unwrap().root_ctx.expect("a live root gets a context"));
        let mut live_roots = live_roots.to_vec();
        created.sort_by_key(|id| id.to_hex());
        live_roots.sort_by_key(|id| id.to_hex());
        assert_eq!(created, live_roots, "only the live roots get contexts");
        assert_eq!(d.kernel_db().lock().get_character(retired).unwrap().unwrap().root_ctx, None);
        for ctx in &live_roots {
            assert_eq!(
                d.has_usable_loadout(*ctx),
                Ok(true),
                "a freshly created root context must be usable, not left inert"
            );
        }

        // A second pass finds every live root already bound — nothing to
        // create, nothing to rebind.
        assert!(d.ensure_root_contexts(PrincipalId::system()).await.unwrap().is_empty());
    }

    /// The morning's live scenario: a root character's root context already
    /// exists but has no binding (a wiped row, or a create that ran before
    /// its rc bucket existed). Boot rebinds it loudly, the same repair `kj
    /// context rebind` performs — it must never leave it inert, since every
    /// new context parents under a root.
    #[tokio::test]
    async fn boot_rebinds_a_root_context_with_no_binding() {
        let d = rc_capable_dispatcher().await;
        let character = PrincipalId::new();
        {
            let db = d.kernel_db().lock();
            db.insert_character(&crate::kernel_db::CharacterRow { context_type: None,
                principal_id: character, name: "keeper".into(), created_at: 0, retired_at: None,
                handoff_ctx: None, root_ctx: None, root: true,
            }).unwrap();
        }
        let ctx = super::super::test_helpers::register_context(&d, Some("keeper"), None, character);
        {
            let db = d.kernel_db().lock();
            db.update_context_type(ctx, super::ROOT_CONTEXT_TYPE).expect("update_context_type");
            db.update_played_by(ctx, Some(character)).expect("update_played_by");
            db.delete_context_binding(ctx).expect("delete the binding row");
        }
        assert_eq!(d.has_usable_loadout(ctx), Ok(false), "precondition: no binding");

        let created = d.ensure_root_contexts(PrincipalId::system()).await.unwrap();
        assert!(
            !created.contains(&ctx),
            "the context already existed — this is a rebind, not a create: {created:?}"
        );
        assert_eq!(
            d.has_usable_loadout(ctx),
            Ok(true),
            "boot must rebind an existing-but-unbound root context, not leave it inert"
        );
    }

    /// With no `root` rc bucket in the host rc tree, boot cannot bind any
    /// root character's root context. `ensure_root_contexts` refuses
    /// loudly, naming the rc bucket it expected and the fix — never
    /// `kaijutsu-server kj -- context rebind`, since the offline CLI boots
    /// the same kernel and would hit the same refusal.
    #[tokio::test]
    async fn ensure_root_contexts_refuses_with_no_root_bucket() {
        let d = std::sync::Arc::new(
            super::super::test_helpers::test_dispatcher_no_root_bucket().await,
        );
        d.set_self_arc();
        d.kernel().broker().set_db(d.kernel_db().clone()).await;

        // Every live root character already has a root context — this
        // exercises the rebind-refusal path (an existing-but-unbound
        // context with no bucket to repair from), distinct from first
        // creation (`create_root_with_no_root_bucket_errors_loudly`).
        // `amy` (the reviewer fixture `test_dispatcher` seeds) also starts
        // with `root_ctx: None`, so she needs one too, or `ensure_root_contexts`
        // would hit the creation path for her instead.
        let amy = super::super::test_helpers::test_reviewer_principal();
        let ctx = super::super::test_helpers::register_context(&d, Some("amy"), None, amy);
        {
            let db = d.kernel_db().lock();
            db.update_context_type(ctx, super::ROOT_CONTEXT_TYPE).expect("update_context_type");
            db.update_played_by(ctx, Some(amy)).expect("update_played_by");
            db.delete_context_binding(ctx).expect("delete the binding row");
        }

        let err = d
            .ensure_root_contexts(PrincipalId::system())
            .await
            .expect_err("no root bucket to rebind from — boot must refuse rather than serve");
        assert!(err.contains("/config/rc/root/create"), "{err}");
        assert!(err.contains("rc reseed"), "{err}");
        assert!(!err.contains("context rebind"), "{err}");
    }

    /// Creating a new root character with no `root` rc bucket must not
    /// leave an inert root behind silently. The character and context rows
    /// stay committed (the next boot's rebind is the retry path once the
    /// bucket exists), but the call itself errors loudly.
    #[tokio::test]
    async fn create_root_with_no_root_bucket_errors_loudly() {
        let d = std::sync::Arc::new(
            super::super::test_helpers::test_dispatcher_no_root_bucket().await,
        );
        d.set_self_arc();
        d.kernel().broker().set_db(d.kernel_db().clone()).await;
        let caller = test_caller();

        let result = d
            .dispatch(&[s("character"), s("create"), s("sovereign"), s("--root")], &caller)
            .await;
        let KjResult::Err(message) = result else { panic!("expected Err, got {result:?}") };
        assert!(message.contains("/config/rc/root/create"), "{message}");
        assert!(message.contains("rc reseed"), "{message}");

        let sheet = d.kernel_db().lock().get_character_by_name("sovereign").unwrap().unwrap();
        assert!(sheet.root, "the character row stays committed as a root");
        let root_ctx = sheet
            .root_ctx
            .expect("the committed context row is left for the next boot's rebind to repair");
        assert_eq!(
            d.has_usable_loadout(root_ctx),
            Ok(false),
            "still inert — nothing in this rc tree could have bound it"
        );
    }

    /// `set --root` and `set --no-root` round-trip, and `set` with neither
    /// is a usage error rather than a silent no-op.
    #[tokio::test]
    async fn set_root_and_no_root_round_trip() {
        let d = rc_capable_dispatcher().await;
        let caller = test_caller();
        d.dispatch(&[s("character"), s("create"), s("worker")], &caller).await;

        let set = d.dispatch(&[s("character"), s("set"), s("worker"), s("--root")], &caller).await;
        assert!(matches!(set, KjResult::Ok { .. }), "{set:?}");
        assert!(d.kernel_db().lock().get_character_by_name("worker").unwrap().unwrap().root);

        let root_ctx = d.kernel_db().lock().get_character_by_name("worker").unwrap().unwrap().root_ctx.unwrap();
        assert!(d.kernel_db().lock().archive_context(root_ctx).unwrap());
        let cleared = d.dispatch(&[s("character"), s("set"), s("worker"), s("--no-root")], &caller).await;
        assert!(matches!(cleared, KjResult::Ok { .. }), "{cleared:?}");
        assert!(!d.kernel_db().lock().get_character_by_name("worker").unwrap().unwrap().root);

        let neither = d.dispatch(&[s("character"), s("set"), s("worker")], &caller).await;
        assert!(matches!(neither, KjResult::Err(_)), "expected Err, got {neither:?}");
        let both = d.dispatch(&[s("character"), s("set"), s("worker"), s("--root"), s("--no-root")], &caller).await;
        assert!(matches!(both, KjResult::Err(_)), "expected Err, got {both:?}");
    }

    /// `retire` no longer consults any sheet relation: a character with a
    /// live context still retires, taking that context with it.
    #[tokio::test]
    async fn retire_takes_its_contexts_with_it() {
        let d = super::super::test_helpers::test_dispatcher().await;
        let caller = test_caller();
        d.dispatch(&[s("character"), s("create"), s("mentor")], &caller).await;
        let mentor_id = {
            let db = d.kernel_db().lock();
            db.get_character_by_name("mentor").unwrap().unwrap().principal_id
        };
        let ctx = register_context(&d, Some("mentor-ctx"), None, mentor_id);
        d.kernel_db().lock().update_played_by(ctx, Some(mentor_id)).unwrap();

        let mut confirmed = caller.clone();
        confirmed.confirmed = true;
        let result = d.dispatch(&[s("character"), s("retire"), s("mentor")], &confirmed).await;
        assert!(matches!(result, KjResult::Ok { .. }), "expected Ok, got {result:?}");
        let ctx_row = d.kernel_db().lock().get_context(ctx).unwrap().unwrap();
        assert!(ctx_row.archived_at.is_some(), "retirement archives the character's live contexts");
        assert!(d.kernel_db().lock().get_character_by_name("mentor").unwrap().unwrap().retired_at.is_some());
    }
}
