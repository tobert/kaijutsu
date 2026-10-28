//! Context subcommands: list, info, switch, create, set, log, move, archive, retag.

use clap::{Args, Parser, Subcommand};
use kaijutsu_types::{BlockId, ContentType, ContextId, ContextState, PrincipalId};

use crate::kernel_db::{ContextRow, ContextShellRow, DemoteOutcome, PromoteOutcome, KernelDb, KernelDbError, KernelDbResult};

use super::format::{
    ContextBound, format_context_info, format_context_table, format_context_tree, format_fork_lineage,
};
use super::refs::{parse_context_ref, resolve_context_ref};
use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug)]
#[command(
    name = "context",
    visible_alias = "ctx",
    about = "Inspect, navigate, and manage contexts",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct ContextArgs {
    #[command(subcommand)]
    command: ContextCommand,
}

/// Settable context configuration shared by `create` and `set`, flattened into
/// both clap variants. Mirrors [`ContextConfig`] (the internal apply struct).
#[derive(Args, Debug, Default)]
pub(crate) struct ContextConfigArgs {
    /// Stored legacy prompt value; not included in model instructions. Use rc instruction blocks
    #[arg(long = "system-prompt")]
    system_prompt: Option<String>,
    /// Working directory for the context's shell: an existing absolute
    /// directory. `create` defaults to the current context's cwd
    #[arg(long)]
    cwd: Option<String>,
    /// Set an env var as KEY=VALUE. Repeat the flag for more than one
    #[arg(long)]
    env: Vec<String>,
    /// Context type selecting the /config/rc lifecycle bundle
    #[arg(long = "type")]
    type_: Option<String>,
}

impl From<ContextConfigArgs> for ContextConfig {
    fn from(a: ContextConfigArgs) -> Self {
        ContextConfig {
            system_prompt: a.system_prompt,
            cwd_spec: a.cwd,
            env_spec: a.env,
            type_spec: a.type_,
            egress: None,
        }
    }
}

#[derive(Subcommand, Debug)]
enum ContextCommand {
    /// List active contexts (or the fork DAG with --tree).
    #[command(alias = "ls")]
    List {
        /// Render the fork DAG as a tree
        #[arg(long, short = 't')]
        tree: bool,
    },
    /// Show a context's metadata (default: current), including the model its
    /// slot for its type gives it, or why it has none.
    Info {
        /// Context to show. Defaults to the current context. Ids and
        /// labels come from `kj context list`.
        context: Option<String>,
    },
    /// List a context's model turns, newest first: who performed each, which
    /// model answered, how it ended, and the provider's token counts
    /// (default: current).
    Turns {
        /// Context whose turns to list. Defaults to the current context.
        /// Ids and labels come from `kj context list`.
        context: Option<String>,
        /// Show at most this many turns
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Show the system instructions and runtime facts used for this context's
    /// next model turn (default: current).
    // Mirrors `crate::llm::build_system_prompt` (llm_stream.rs's
    // `spawn_llm_for_prompt`) exactly, reading the live DriftRouter handle
    // rather than falling back to the KernelDb row — a resolved target with
    // no drift handle renders label/state/provider/model all absent, the
    // same honest gap a real turn would hit.
    Prompt {
        /// Context whose system prompt to render. Defaults to the current
        /// context. Ids and labels come from `kj context list`.
        context: Option<String>,
    },
    /// Print the current context.
    #[command(alias = "show")]
    Current,
    /// Switch the session to another context.
    #[command(alias = "sw")]
    Switch {
        /// Context to switch to. Ids and labels come from `kj context list`.
        context: String,
    },
    /// Get-or-create the well-known "scratch" context.
    #[command(alias = "self")]
    Scratch,
    /// Re-run the `create` rc lifecycle on a context left with no usable
    /// loadout — the repair for a create-time rc failure (default: current).
    /// Refuses a context that already has a loadout.
    Rebind {
        /// Context to repair. Defaults to the current context. Ids and
        /// labels come from `kj context list`.
        context: Option<String>,
    },
    /// Apply settable config to an existing context (default: current).
    Set {
        /// Context to update. Defaults to the current context. Ids and
        /// labels come from `kj context list`.
        context: Option<String>,
        /// Add a host this context's curl may reach: a DNS name, a loopback
        /// literal (localhost, 127.0.0.1, or the IPv6 loopback address), or
        /// * for every host. Repeat the flag for more than one
        #[arg(long, value_name = "HOST")]
        egress_allow: Vec<String>,
        /// Remove a host from this context's egress list. Repeat the flag
        /// for more than one
        #[arg(long, value_name = "HOST")]
        egress_deny: Vec<String>,
        #[command(flatten)]
        config: ContextConfigArgs,
    },
    /// Remove an env var from a context.
    Unset {
        /// Context to update. Defaults to the current context. Ids and
        /// labels come from `kj context list`.
        context: Option<String>,
        /// Env var key to remove
        #[arg(long)]
        env: Option<String>,
    },
    /// Show fork lineage from a context up to root (default: current).
    Log {
        /// Context to show lineage for. Defaults to the current context.
        /// Ids and labels come from `kj context list`.
        context: Option<String>,
    },
    /// Rename a context — set its label (default: current). Refuses a label
    /// another context already holds; *moving* a label between contexts is
    /// `retag` (latched).
    Rename {
        /// New label
        name: String,
        /// Context to rename (default: current)
        #[arg(long, short = 'c')]
        context: Option<String>,
    },
    /// Replace a context with a fresh successor (default: the current
    /// context). The successor is created from the same parent for the same
    /// character, with slots read fresh from the character's cast and the
    /// predecessor's senpai, and copies the type, system prompt, workspace,
    /// env, and cwd, then runs the `create` rc lifecycle. `ROTATED_FROM`
    /// names the predecessor by id. When the successor has a loadout, it takes
    /// the label and the ring seat, and the predecessor is archived after its
    /// `archive` rc lifecycle runs. The kernel moves no kohai. The senpai's
    /// character, the lineage root, or the context's own character may
    /// rotate. Rotating a character's handoff log points the character at the
    /// successor.
    Rotate {
        /// Context to rotate. Defaults to the current context. Ids and
        /// labels come from `kj context list`.
        context: Option<String>,
    },
    /// Archive one context, retaining its history (latched). Needs the
    /// operator capability, or a person's live root character acting from
    /// any context. Children keep their parent edge and state. Reports to an
    /// archived context and `kj senpai set` onto it refuse, and its forks and
    /// kohai stay live. Restore with `kj context promote <id>`.
    /// The context's `archive` rc lifecycle runs first, while it is still
    /// live; a failing script is recorded and the archive still commits.
    Archive {
        /// Context to archive. Ids and labels come from `kj context list`.
        /// With `--children`, the parent whose children are archived
        /// (default: current).
        context: Option<String>,
        /// Archive the context's live children instead of the context,
        /// each through its own `archive` lifecycle.
        #[arg(long)]
        children: bool,
        /// With `--children`, archive only children of this context type.
        #[arg(long = "type", value_name = "TYPE", requires = "children")]
        context_type: Option<String>,
    },
    /// Conclude a context — mark this work "done" (the time-well's hot→recent
    /// transition). Reversible via fork; not latched. Needs the operator
    /// capability, or a person's live root character, which acts as a human
    /// root from any context. A seat other than ROOT may not conclude its
    /// own context; it asks ROOT. Only a human root concludes the console,
    /// and nobody concludes a human root.
    #[command(alias = "done")]
    Conclude {
        /// Context to conclude. Does not recurse into children. Ids and
        /// labels come from `kj context list`.
        context: String,
    },
    /// Promote a context into ring 0 ("active"). Not latched. Needs the
    /// operator capability, or a person's live root character, which acts
    /// as a human root from any context. A seat other than ROOT (the ROOT
    /// console or a human root) may not promote its own context, a human root, or the
    /// console; it asks ROOT. First promote wins the timestamp —
    /// re-promoting an already-promoted context is a no-op.
    Promote {
        /// Context to promote. An archived context (reachable by full id)
        /// is resurrected rather than refused. Ids and labels come from
        /// `kj context list`.
        context: String,
    },
    /// Push a context outward one step on the demote ladder: promoted →
    /// unpromoted, unpromoted/undemoted → demoted, already demoted →
    /// archived, after its `archive` rc lifecycle runs. The archiving step
    /// is latched like `kj context archive`; restore with
    /// `kj context promote <id>`. Needs the operator capability, or a
    /// person's live root character, which acts as a human root from any
    /// context. A seat other than ROOT may not demote its own context; it asks ROOT. Only a
    /// human root demotes a human root or the console.
    Demote {
        /// Context to push one step down the ladder. Ids and labels come
        /// from `kj context list`.
        context: String,
    },
    // The flag itself is `ContextRow::paused_at`; see its doc for the
    // design intent behind a flag that nothing reads yet.
    /// Set the "suspend activity" flag. Records intent only — nothing gates
    /// behavior on it yet. Clear it with `kj context resume`.
    Pause {
        /// Context to pause. Ids and labels come from `kj context list`.
        context: String,
    },
    /// Clear the "suspend activity" flag.
    Resume {
        /// Context to resume. Ids and labels come from `kj context list`.
        context: String,
    },
    /// Move a label to a different context (latched).
    Retag {
        /// Label to move. Taken from whichever context currently holds it,
        /// if any.
        label: String,
        /// Context to give the label to. Ids and labels come from
        /// `kj context list`.
        context: String,
    },
    /// Set or clear the conversation hydration window — `[0, marker] ∪ last-N`
    /// instead of the whole history (the cost guard for endless musician logs).
    Hydrate {
        /// Context whose hydration window to set or clear. Defaults to the
        /// current context. Ids and labels come from `kj context list`.
        context: Option<String>,
        /// Keep the last N blocks as the sliding tail (with the pinned prefix).
        #[arg(long)]
        window: Option<u32>,
        /// Pin the prefix end at this block key (default: the current tail).
        #[arg(long)]
        mark: Option<String>,
        /// Remove the hydration window — hydrate everything again.
        #[arg(long, conflicts_with_all = ["window", "mark"])]
        clear: bool,
    },
}

/// Settable context configuration shared by `create` and `set`.
///
/// These are the knobs that can be applied to an existing context row:
/// system prompt, working directory, an env var, and the rc-dispatch
/// `context_type`. `create` reuses the same surface so a context can be born
/// fully configured (fork-parity) instead of needing a follow-up
/// `kj context set`.
#[derive(Default)]
struct ContextConfig {
    system_prompt: Option<String>,
    cwd_spec: Option<String>,
    env_spec: Vec<String>,
    type_spec: Option<String>,
    egress: Option<EgressAssignment>,
}

/// A validated, authority-checked `--egress-allow`/`--egress-deny` change,
/// applied inside [`KjDispatcher::apply_context_config`]'s transaction. Hosts
/// are already normalized (`kernel_db::validate_egress_host`) by the time
/// this is built.
struct EgressAssignment {
    allow: Vec<String>,
    deny: Vec<String>,
    caller_actor: kaijutsu_types::PrincipalId,
}

/// How a new context row gets its context character
/// (`docs/senpai-kohai.md`, "Creating, forking, rotating").
#[derive(Clone, Copy, Debug)]
pub(crate) enum CharacterPlacement {
    /// The context has no context character (a handoff log, scratch).
    None,
    /// The context is a new context character of `performer`, under
    /// `senpai`, with slots copied fresh from the character's cast.
    Start { performer: kaijutsu_types::PrincipalId, senpai: Option<ContextId> },
    /// The context is a copy of `source`'s context character with `source` as
    /// its senpai (a judge shadow).
    Fork { source: ContextId },
    /// The context is the successor of `predecessor`: its character, with
    /// slots copied fresh from the cast and the predecessor's senpai.
    Rotate { predecessor: ContextId },
}

/// Insert a new context only while its model approval assignment is valid.
/// The check follows insertion within one database transaction so a retirement
/// or a senpai move cannot leave a visible self-reviewed model context.
fn insert_new_context_checked(db: &KernelDb, row: &ContextRow, placement: CharacterPlacement) -> KernelDbResult<()> {
    db.in_transaction(|db| insert_new_context_rows(db, row, placement))
}

/// The body of [`insert_new_context_checked`], for a caller that already
/// holds a transaction and writes more rows in it. A context that starts or
/// continues a character must be reviewed by someone other than that
/// character, unless it is a root.
pub(crate) fn insert_new_context_rows(db: &KernelDb, row: &ContextRow, placement: CharacterPlacement) -> KernelDbResult<()> {
    let default_ws = db.get_or_create_default_workspace(row.created_by)?;
    let live_character = |performer: kaijutsu_types::PrincipalId| -> KernelDbResult<()> {
        if db.get_character(performer)?.filter(|sheet| sheet.retired_at.is_none()).is_none() {
            return Err(KernelDbError::Validation("performer must name a live character".into()));
        }
        Ok(())
    };
    let performer = match placement {
        CharacterPlacement::Start { performer, senpai } => {
            live_character(performer)?;
            if senpai.is_none() {
                return Err(KernelDbError::Validation(
                    "a new context answers to a senpai: only a root console has none, and `kj character create` makes it".into(),
                ));
            }
            Some(performer)
        }
        CharacterPlacement::Rotate { predecessor } => {
            let performer = db.require_context_character(predecessor)?.principal_id;
            live_character(performer)?;
            Some(performer)
        }
        CharacterPlacement::None | CharacterPlacement::Fork { .. } => None,
    };
    db.insert_context_with_document(row, default_ws)?;
    match placement {
        CharacterPlacement::None => {}
        CharacterPlacement::Start { performer, senpai } => {
            db.write_context_character(row.context_id, performer, senpai, None)?;
        }
        CharacterPlacement::Rotate { predecessor } => {
            let predecessor = db.require_context_character(predecessor)?;
            db.write_context_character(row.context_id, predecessor.principal_id, predecessor.senpai_ctx, predecessor.persona.as_deref())?;
        }
        CharacterPlacement::Fork { source } => {
            db.copy_context_character(source, row.context_id)?;
        }
    }
    if let Some(performer) = performer {
        let root = db.get_character(performer)?.is_some_and(|sheet| sheet.root);
        let reviewer = db.route_review(crate::kernel_db::ReviewQuery::ask(row.context_id, performer))?.reviewer();
        if !root && performer == reviewer {
            return Err(KernelDbError::Validation("the performer cannot review its own work".into()));
        }
    }
    Ok(())
}

/// The model of each listed context, read once for the whole listing from
/// the context slots and nothing else.
fn context_bindings<'a>(
    db: &KernelDb,
    rows: impl Iterator<Item = &'a ContextRow>,
) -> KernelDbResult<std::collections::HashMap<ContextId, ContextBound>> {
    let mut bound = std::collections::HashMap::new();
    for row in rows {
        let found = crate::model_resolution::context_model(db, row.context_id)?;
        bound.insert(
            row.context_id,
            ContextBound {
                model: match &found {
                    crate::model_resolution::ContextModelLookup::Model(m) => Some(format!("{}/{}", m.backend, m.model)),
                    crate::model_resolution::ContextModelLookup::None(_) => None,
                },
            },
        );
    }
    Ok(bound)
}

impl KjDispatcher {
    /// Validate user-supplied config BEFORE any mutation: the `--env
    /// KEY=VALUE` shape, a context type that is not a reserved cast role, and
    /// a `--cwd` the shell enters. No writes, so callers can bail out cleanly
    /// without leaving a half-configured (or orphan) context behind.
    async fn resolve_context_config(
        &self,
        cfg: &ContextConfig,
        caller: &KjCaller,
        cwd_check_context: ContextId,
    ) -> Result<(), String> {
        if let Some(env) = cfg.env_spec.iter().find(|e| !e.contains('=')) {
            return Err(format!("--env requires KEY=VALUE format, got '{env}'"));
        }

        if let Some(context_type) = &cfg.type_spec {
            crate::model_resolution::refuse_reserved_role(context_type)?;
        }

        if let Some(cwd) = &cfg.cwd_spec {
            self.check_cwd(caller, cwd_check_context, cwd).await.map_err(|e| format!("--cwd {e}"))?;
        }

        Ok(())
    }

    /// Check that `cwd` is a directory the shell's `cd` enters, through a
    /// shell built in `context` for `caller`. The error names the path.
    async fn check_cwd(&self, caller: &KjCaller, context: ContextId, cwd: &str) -> Result<(), String> {
        let identity = crate::runtime::context_shell::ShellIdentity {
            requester: caller.principal_id, performer: caller.actor_id, reviewer: caller.reviewer_id,
            context, session: caller.session_id,
        };
        crate::runtime::shell_state::check_shell_cwd(self, identity, std::path::Path::new(cwd)).await
    }

    /// Apply already-validated config to an existing context row and return
    /// the human-readable change list. Assumes [`Self::resolve_context_config`]
    /// has already run, so only DB I/O errors surface.
    async fn apply_context_config(
        &self,
        target_id: ContextId,
        cfg: &ContextConfig,
    ) -> Result<Vec<String>, String> {
        // One transaction: a failure on a later field (an `--env` key is
        // validated at write time) must not leave an earlier one committed.
        let changes = {
            let guard = self.kernel_db().lock();
            let applied = guard.in_transaction(|db| {
                let mut changes = Vec::new();

                if cfg.system_prompt.is_some() {
                    let current = db
                        .get_context(target_id)
                        ?
                        .ok_or_else(|| {
                            crate::kernel_db::KernelDbError::NotFound(format!(
                                "context {}",
                                target_id.short()
                            ))
                        })?;
                    let new_prompt = cfg
                        .system_prompt
                        .as_deref()
                        .or(current.system_prompt.as_deref());
                    db.update_settings(target_id, new_prompt)?;
                    changes.push("system-prompt".to_string());
                }

                if let Some(ref cwd) = cfg.cwd_spec {
                    let shell = ContextShellRow {
                        context_id: target_id,
                        cwd: Some(cwd.clone()),
                        updated_at: kaijutsu_types::now_millis() as i64,
                    };
                    db.upsert_context_shell(&shell)?;
                    changes.push(format!("cwd={cwd}"));
                }

                for env in &cfg.env_spec {
                    // KEY=VALUE shape validated upstream; every pair lands.
                    if let Some((key, value)) = env.split_once('=') {
                        db.set_context_env(target_id, key, value)?;
                        changes.push(format!("env {key}={value}"));
                    }
                }

                if let Some(ref t) = cfg.type_spec {
                    db.update_context_type(target_id, t)?;
                    changes.push(format!("type={t}"));
                }

                if let Some(egress) = &cfg.egress {
                    if db.context_authority(target_id, egress.caller_actor, crate::kernel_db::GovernedAct::Egress)?.is_err() {
                        return Err(crate::kernel_db::KernelDbError::Validation("egress assignment authority changed before commit".into()));
                    }
                    for host in &egress.allow {
                        db.add_context_egress(target_id, host)?;
                        changes.push(format!("egress-allow {host}"));
                    }
                    for host in &egress.deny {
                        db.remove_context_egress(target_id, host)?;
                        changes.push(format!("egress-deny {host}"));
                    }
                }

                Ok(changes)
            });
            applied.map_err(|e| e.to_string())?
        };
        // db lock released here

        Ok(changes)
    }

    pub(crate) async fn dispatch_context(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<ContextArgs>();
        }
        let parsed = match ContextArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj context: {e}"));
            }
        };

        // Mutating or destroying an *existing* context is operator authority.
        // `scratch` is deliberately ungated: it is the bootstrap entry point,
        // needing no parent, so a fresh, unjoined session can still make its
        // first context. Creating any other context is `kj kohai create`.
        // Read/navigation verbs (list/info/current/switch/log) stay ungated
        // too.
        //
        // `rebind` is ungated on that same argument, and must stay that way: it
        // re-runs the rc lifecycle that assigns loadouts, so it grants exactly
        // what birth would have and nothing the caller chose. Gating it on
        // `Operator` would put the repair for a no-loadout context behind a
        // capability that exact context cannot hold — the lockout it exists to
        // undo. Its own guards (no usable loadout, not archived) are in
        // `context_rebind`.
        //
        // `conclude`, `promote`, and `demote` need Operator and more: which
        // seat may act on which target is decided per target
        // (`require_placement_authority`).
        //
        // `archive` takes Operator, or a live root character acting from any
        // context (`docs/character.md`, "ROOT places contexts").
        if matches!(
            parsed.command,
            ContextCommand::Set { .. }
                | ContextCommand::Unset { .. }
                | ContextCommand::Rename { .. }
                | ContextCommand::Archive { .. }
                | ContextCommand::Rotate { .. }
                | ContextCommand::Retag { .. }
                | ContextCommand::Hydrate { .. }
        ) && let Err(denied) =
            self.require_cap(caller, crate::mcp::Capability::Operator, "context")
        {
            let human = matches!(parsed.command, ContextCommand::Archive { .. }) && match self.caller_is_live_root(caller) {
                Ok(human) => human,
                Err(e) => return KjResult::Err(format!("kj context archive: {e}")),
            };
            if !human {
                return denied;
            }
        }

        match parsed.command {
            ContextCommand::List { tree } => self.context_list(tree, caller).await,
            ContextCommand::Info { context } => {
                self.context_info(context.as_deref(), caller).await
            }
            ContextCommand::Turns { context, limit } => {
                self.context_turns(context.as_deref(), limit, caller)
            }
            ContextCommand::Prompt { context } => {
                self.context_prompt(context.as_deref(), caller).await
            }
            ContextCommand::Current => self.context_current(caller).await,
            ContextCommand::Switch { context } => self.context_switch(&context, caller).await,
            ContextCommand::Scratch => self.context_scratch(caller).await,
            ContextCommand::Rebind { context } => {
                self.context_rebind(context.as_deref(), caller).await
            }
            ContextCommand::Set { context, egress_allow, egress_deny, config } => {
                self.context_set(context.as_deref(), &egress_allow, &egress_deny, config.into(), caller).await
            }
            ContextCommand::Unset { context, env } => {
                self.context_unset(context.as_deref(), env.as_deref(), caller)
            }
            ContextCommand::Log { context } => self.context_log(context.as_deref(), caller),
            ContextCommand::Archive { context, children: false, .. } => match context {
                Some(context) => Box::pin(self.context_archive(&context, caller)).await,
                None => KjResult::Err(
                    "kj context archive: name the context to archive, or pass --children".to_string(),
                ),
            },
            ContextCommand::Archive { context, children: true, context_type } => {
                Box::pin(self.context_archive_children(context.as_deref(), context_type.as_deref(), caller)).await
            }
            ContextCommand::Rotate { context } => self.context_rotate(context.as_deref(), caller).await,
            ContextCommand::Conclude { context } => self.context_conclude(&context, caller).await,
            ContextCommand::Promote { context } => self.context_promote(&context, caller).await,
            ContextCommand::Demote { context } => self.context_demote(&context, caller).await,
            ContextCommand::Pause { context } => self.context_pause(&context, caller, true).await,
            ContextCommand::Resume { context } => self.context_pause(&context, caller, false).await,
            ContextCommand::Rename { name, context } => {
                self.context_rename(&name, context.as_deref(), caller).await
            }
            ContextCommand::Retag { label, context } => {
                self.context_retag(&label, &context, caller).await
            }
            ContextCommand::Hydrate {
                context,
                window,
                mark,
                clear,
            } => self.context_hydrate(context.as_deref(), window, mark.as_deref(), clear, caller),
        }
    }

    async fn context_list(&self, tree: bool, caller: &KjCaller) -> KjResult {
        let db = self.kernel_db().lock();
        if tree {
            match db.context_dag() {
                Ok(dag) => {
                    let bound = match context_bindings(&db, dag.iter().map(|(row, _)| row)) {
                        Ok(bound) => bound,
                        Err(e) => return KjResult::Err(format!("kj context list: {e}")),
                    };
                    let text = format_context_tree(&dag, caller.context_id, &bound);
                    let ids = context_handles(dag.iter().map(|(row, _)| row));
                    KjResult::ok_with_data(text, ids)
                }
                Err(e) => KjResult::Err(format!("kj context list: {e}")),
            }
        } else {
            match db.list_active_contexts() {
                Ok(contexts) => {
                    let bound = match context_bindings(&db, contexts.iter()) {
                        Ok(bound) => bound,
                        Err(e) => return KjResult::Err(format!("kj context list: {e}")),
                    };
                    let text = format_context_table(&contexts, caller.context_id, &bound);
                    let ids = context_handles(contexts.iter());
                    KjResult::ok_with_data(text, ids)
                }
                Err(e) => KjResult::Err(format!("kj context list: {e}")),
            }
        }
    }

    async fn context_info(&self, context: Option<&str>, caller: &KjCaller) -> KjResult {
        // All KernelDb reads happen in this block, which ends before the
        // `.await` below — `parking_lot::MutexGuard` is `!Send` in this
        // workspace (the `send_guard` feature isn't enabled), so it must not
        // be held across an await point.
        let (
            row,
            children_count,
            drift_from,
            drift_to,
            is_current,
            trace_id,
            shell,
            env_vars,
            egress_hosts,
            usage,
            turn_totals,
            workspace_paths,
            workspace_label,
            bound_model,
            played_by_name,
        ) = {
            let db = self.kernel_db().lock();

            // Resolve target context (default: current)
            let target_id = match super::refs::resolve_context_arg(context, caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj context info: {e}")),
            };

            let row = match db.get_context(target_id) {
                Ok(Some(r)) => r,
                Ok(None) => return KjResult::Err("kj context info: not found".to_string()),
                Err(e) => return KjResult::Err(format!("kj context info: {e}")),
            };

            // Count live children: the contexts forked from this one
            let children_count = db
                .fork_children(target_id)
                .map(|children| children.len())
                .unwrap_or(0);

            // Count drift edges (both directions)
            let drift_from = db
                .edges_from(target_id)
                .map(|edges| edges.len())
                .unwrap_or(0);
            let drift_to = db
                .edges_to(target_id)
                .map(|edges| edges.len())
                .unwrap_or(0);

            let is_current = Some(target_id) == caller.context_id;
            // Long-running OTel trace id lives on the in-memory drift handle, not
            // the persisted row — look it up so the umbrella trace is pasteable.
            let trace_id = self.drift_router().read().trace_id_for_context(target_id);

            // Shell config — captured into the structured record below as well.
            let shell = db.get_context_shell(target_id).ok().flatten();

            // Env vars
            let env_vars = db.get_context_env(target_id).unwrap_or_default();

            // Egress hosts (docs/egress.md). A read failure is an error, never
            // "none": an empty list claims this context reaches nothing.
            let egress_hosts = match db.list_context_egress(target_id) {
                Ok(hosts) => hosts,
                Err(e) => return KjResult::Err(format!("kj context info: {e}")),
            };

            // Token usage — a SNAPSHOT of the last completed LLM call ("how
            // full is this context right now"), not a running total; see
            // `ContextUsageRow`. `None` for a context that has never
            // completed a call — shown as absence, never a fabricated zero.
            let usage = db.get_context_usage(target_id).ok().flatten();

            // Spend: the provider's raw counts summed over every turn. A
            // failed read is an error, never zero.
            let turn_totals = match db.context_turn_totals(target_id) {
                Ok(totals) => totals,
                Err(e) => return KjResult::Err(format!("kj context info: turn totals: {e}")),
            };

            // Workspace paths
            let workspace_paths = db.context_workspace_paths(target_id).ok().flatten();
            let workspace_label = row
                .workspace_id
                .and_then(|wsid| db.get_workspace(wsid).ok().flatten())
                .map(|ws| ws.label);

            // The model the context's binding gives its type, or why it has
            // none. A binding that cannot be read is an error, not "none".
            let bound_model = match crate::model_resolution::context_model(&db, target_id) {
                Ok(found) => found,
                Err(e) => return KjResult::Err(format!("kj context info: model binding: {e}")),
            };

            // Resolve the performer's name for live and archived contexts.
            // Retirement preserves both the sheet and this relationship.
            let played_by_name = row
                .played_by
                .and_then(|pid| db.get_character(pid).ok().flatten())
                .map(|c| c.name);

            (
                row,
                children_count,
                drift_from,
                drift_to,
                is_current,
                trace_id,
                shell,
                env_vars,
                egress_hosts,
                usage,
                turn_totals,
                workspace_paths,
                workspace_label,
                bound_model,
                played_by_name,
            )
        };

        // Context-window denominator: the context slot's window, resolved
        // kernel-side (never shipped to the client as raw inputs to derive).
        // `None` when the slot holds none: the two fields below MUST both be
        // `null` in that case, never a guessed denominator standing in for
        // one. The percentage itself comes from `context_used_pct`, the ONE
        // percentage-math site, shared with the wire's `contextUsedPct`
        // (`ContextHandleInfo` / `kaijutsu-server/src/rpc.rs::list_contexts`)
        // so the two surfaces can never disagree.
        let (resolved_backend, resolved_model, bound_window) = match &bound_model {
            crate::model_resolution::ContextModelLookup::Model(m) => {
                (Some(m.backend.clone()), Some(m.model.clone()), m.context_window.map(|w| (m.backend.clone(), m.model.clone(), w)))
            }
            crate::model_resolution::ContextModelLookup::None(_) => (None, None, None),
        };
        let (context_window, context_used_pct) = match &usage {
            Some(u) => {
                // The window belongs to the bound model: usage recorded on
                // another model has no window here.
                let window = bound_window
                    .as_ref()
                    .filter(|(backend, model, _)| *backend == u.provider && *model == u.model)
                    .map(|(_, _, w)| *w);
                let pct = crate::kernel_db::context_used_pct(u, window);
                (window, pct)
            }
            None => (None, None),
        };

        let mut info = format_context_info(
            &row,
            children_count,
            drift_from + drift_to,
            is_current,
            trace_id,
        );

        match &bound_model {
            crate::model_resolution::ContextModelLookup::Model(m) => {
                info.push_str(&format!("\nModel:    {}/{}", m.backend, m.model));
            }
            crate::model_resolution::ContextModelLookup::None(none) => {
                info.push_str(&format!("\nModel:    {}", none.line()));
            }
        }

        // The document version a client hydrates against (`docs/change-feed.md`).
        // Absent when the document is not resident in this kernel.
        let version = self.blocks.version(row.context_id).ok();
        if let Some(v) = version {
            info.push_str(&format!("\nVersion: {v}"));
        }

        if let Some(ref s) = shell
            && let Some(cwd) = &s.cwd
        {
            info.push_str(&format!("\nCwd:     {cwd}"));
        }

        if !env_vars.is_empty() {
            info.push_str("\nEnv:");
            for v in &env_vars {
                info.push_str(&format!("\n  {}={}", v.key, v.value));
            }
        }

        // Always shown, unlike Env above — an explicit "none" tells the
        // reader this context reaches nothing rather than leaving them to
        // wonder whether the field was simply omitted (`docs/egress.md`).
        if egress_hosts.is_empty() {
            info.push_str("\nEgress:  none");
        } else {
            info.push_str("\nEgress:");
            for h in &egress_hosts {
                info.push_str(&format!("\n  {h}"));
            }
        }

        if let Some(ref u) = usage {
            info.push_str(&format!(
                "\nTokens:  {} in + {} out = {} (cache read {}, write {}; {}/{})",
                u.input_tokens,
                u.output_tokens,
                u.input_tokens + u.output_tokens,
                u.cache_read_tokens,
                u.cache_write_tokens,
                u.provider,
                u.model,
            ));
            match (context_window, context_used_pct) {
                (Some(w), Some(pct)) => {
                    info.push_str(&format!(" — {w} window, {pct:.1}% used"));
                }
                _ => info.push_str(" — window unknown"),
            }
        }

        info.push_str(&format!(
            "\nTurns:   {} ({} calls): {} in, {} out, cache read {}, write {}, reasoning {}",
            turn_totals.turns,
            turn_totals.llm_calls,
            turn_totals.input_tokens,
            turn_totals.output_tokens,
            turn_totals.cache_read_tokens,
            turn_totals.cache_write_tokens,
            turn_totals.reasoning_tokens,
        ));

        if let Some(paths) = workspace_paths.as_ref().filter(|p| !p.is_empty()) {
            let ws_label = workspace_label.clone().unwrap_or_else(|| "?".into());
            info.push_str(&format!("\nWorkspace: {ws_label}"));
            for p in paths {
                let ro = if p.read_only { " (ro)" } else { "" };
                info.push_str(&format!("\n  {}{ro}", p.path));
            }
        }

        if let Some(ref name) = played_by_name {
            info.push_str(&format!("\nPlayed by: {name}"));
        }
        let (effective_review, reviewer_error) = match self.kernel().resolve_context_review(row.context_id).await {
            Ok(review) => (Some(review), None),
            Err(error) => {
                info.push_str(&format!("\nReviewer error: {error}"));
                (None, Some(error))
            }
        };
        let reviewer_name = effective_review.as_ref().map(|review| review.reviewer.name.clone());
        if let Some(review) = effective_review.as_ref() {
            info.push_str(&format!("\nReviewer: {} ({})", review.reviewer.name, review.source.as_str()));
        }
        // The context this one answers to, with its character. A context with
        // no context character, or a root console, has none.
        let senpai_view = {
            let db = self.kernel_db().lock();
            let found = (|| -> Result<_, String> {
                let Some(character) = db.context_character(row.context_id).map_err(|e| e.to_string())? else {
                    return Ok(None);
                };
                let Some(senpai) = character.senpai_ctx else { return Ok(None) };
                let senpai_character = db.context_character(senpai).map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("senpai {} has no context character", db.context_name(senpai)))?;
                Ok(Some((senpai, db.context_name(senpai), db.name_for(senpai_character.principal_id))))
            })();
            match found {
                Ok(view) => view,
                Err(error) => return KjResult::Err(format!("kj context info: senpai lookup: {error}")),
            }
        };
        if let Some((_, senpai_name, senpai_character)) = &senpai_view {
            info.push_str(&format!("\nSenpai: {senpai_name}, {senpai_character}"));
        }

        // Structured record: full ids and the same fields the text view
        // surfaces, so `kaish-last` round-trips and per-field jq queries work.
        let mut record = serde_json::json!({
            "context_id": row.context_id.to_hex(),
            "label": row.label,
            "context_state": format!("{:?}", row.context_state),
            "context_type": row.context_type,
            // Advisory: the registering client's self-reported hostname
            // (`null` when unknown — old client, or a creation path with
            // nothing to report). See `ContextRow::origin_host`'s doc
            // comment.
            "origin_host": row.origin_host,
            "trace_id": trace_id.map(crate::kj::format::hex32),
            "forked_from": row.forked_from.map(|id| id.to_hex()),
            "fork_kind": row.fork_kind.as_ref().map(|k| format!("{k:?}")),
            "children_count": children_count,
            "version": version,
            "drift_count": drift_from + drift_to,
            "is_current": is_current,
            "workspace_id": row.workspace_id.map(|id| id.to_hex()),
            "workspace_label": workspace_label,
            "preset_id": row.preset_id.map(|id| id.to_hex()),
            // The character whose performance this context is, and its
            // kernel-owned name — `null` together when nobody plays it (a
            // score context, a file document, a pre-character row). See
            // `ContextRow::played_by`'s doc comment.
            "played_by": row.played_by.map(|id| id.to_hex()),
            "played_by_name": played_by_name,
            // The model the context's slot gives its type. Both
            // `resolved_backend` and `resolved_model` are `null` together
            // when the context has no slot for the type; `model_absent`
            // then says why. An honest absence, never a fallback.
            "resolved_backend": resolved_backend,
            "resolved_model": resolved_model,
            "model_absent": match &bound_model {
                crate::model_resolution::ContextModelLookup::None(none) => Some(none.line()),
                _ => None,
            },
            "cwd": shell.as_ref().and_then(|s| s.cwd.clone()),
            "env": env_vars.iter()
                .map(|v| (v.key.clone(), serde_json::Value::String(v.value.clone())))
                .collect::<serde_json::Map<_, _>>(),
            // `docs/egress.md` — the hosts this context's curl may reach.
            // Always present (an empty array is the honest "none", never
            // omitted).
            "egress": egress_hosts,
            // `null` when this context has never completed an LLM call —
            // honest absence, not a fabricated zero. `context_window` /
            // `context_used_pct` are resolved kernel-side (never derived
            // client-side, per the thin-client convention) via
            // the context slot's window; both are `null` together whenever
            // the slot holds none — never a fabricated denominator standing
            // in for an unknown one.
            "usage": usage.as_ref().map(|u| serde_json::json!({
                "provider": u.provider,
                "model": u.model,
                "input_tokens": u.input_tokens,
                "output_tokens": u.output_tokens,
                "total_tokens": u.input_tokens + u.output_tokens,
                "cache_read_tokens": u.cache_read_tokens,
                "cache_write_tokens": u.cache_write_tokens,
                "reasoning_tokens": u.reasoning_tokens,
                "updated_at_ms": u.updated_at,
                "context_window": context_window,
                "context_used_pct": context_used_pct,
            })),
        });
        record["turns"] = serde_json::json!({
            "turns": turn_totals.turns,
            "llm_calls": turn_totals.llm_calls,
            "input_tokens": turn_totals.input_tokens,
            "output_tokens": turn_totals.output_tokens,
            "cache_read_tokens": turn_totals.cache_read_tokens,
            "cache_write_tokens": turn_totals.cache_write_tokens,
            "reasoning_tokens": turn_totals.reasoning_tokens,
        });
        record["reviewer_id"] = serde_json::json!(effective_review.as_ref().map(|review| review.reviewer.principal_id.to_hex()));
        record["reviewer_name"] = serde_json::json!(reviewer_name);
        record["review_source"] = serde_json::json!(effective_review.as_ref().map(|review| review.source.as_str()));
        record["reviewer_error"] = serde_json::json!(reviewer_error);
        record["senpai"] = serde_json::json!(senpai_view.as_ref().map(
            |(id, name, character)| serde_json::json!({ "id": id.to_hex(), "name": name, "character": character })
        ));

        KjResult::ok_with_data(info, record)
    }

    /// `kj context turns`: a context's recorded model turns, newest first.
    fn context_turns(&self, context: Option<&str>, limit: usize, caller: &KjCaller) -> KjResult {
        let db = self.kernel_db().lock();
        let target_id = match super::refs::resolve_context_arg(context, caller, &db) {
            Ok(id) => id,
            Err(e) => return KjResult::Err(format!("kj context turns: {e}")),
        };
        let turns = match db.list_turns(target_id, limit) {
            Ok(turns) => turns,
            Err(e) => return KjResult::Err(format!("kj context turns: {e}")),
        };
        let names = |principal: kaijutsu_types::PrincipalId| {
            db.get_character(principal).ok().flatten().map(|c| c.name).unwrap_or_else(|| principal.short())
        };
        let mut text = String::new();
        let mut records = Vec::new();
        for turn in &turns {
            let start = &turn.start;
            let performer = names(start.performer);
            let ended = turn.stop_reason.as_deref().unwrap_or("running");
            text.push_str(&format!(
                "{} {} {performer} {}/{} (role {}) {ended}: {} calls, {} in, {} out, cache read {}, write {}, reasoning {}\n",
                start.turn_id.short(),
                start.started_at,
                start.provider,
                start.model,
                start.role,
                turn.llm_calls,
                turn.input_tokens,
                turn.output_tokens,
                turn.cache_read_tokens,
                turn.cache_write_tokens,
                turn.reasoning_tokens,
            ));
            records.push(serde_json::json!({
                "turn_id": start.turn_id.to_hex(),
                "context_id": start.context_id.to_hex(),
                "performer": start.performer.to_hex(),
                "performer_name": performer,
                "role": start.role,
                "provider": start.provider,
                "model": start.model,
                "started_at": start.started_at,
                "ended_at": turn.ended_at,
                "stop_reason": turn.stop_reason,
                "llm_calls": turn.llm_calls,
                "input_tokens": turn.input_tokens,
                "output_tokens": turn.output_tokens,
                "cache_read_tokens": turn.cache_read_tokens,
                "cache_write_tokens": turn.cache_write_tokens,
                "reasoning_tokens": turn.reasoning_tokens,
            }));
        }
        if turns.is_empty() {
            text.push_str("no turns recorded\n");
        }
        KjResult::ok_with_data(text.trim_end().to_string(), serde_json::Value::Array(records))
    }

    /// Render the system prompt this context actually gets: the exact
    /// `rc sections → <situation>` assembly `spawn_llm_for_prompt`
    /// builds for a real turn (`crates/kaijutsu-server/src/llm_stream.rs`),
    /// reusing the same pure pieces (`build_system_prompt`,
    /// `extract_system_prompt_sections`, `context_model`) so this
    /// view cannot drift from what actually ships to the model — it is not
    /// a second implementation of the assembly, only a read-only caller of
    /// the same one.
    ///
    /// Deliberately more permissive than the turn path in one respect: an
    /// unresolved model backend is not fatal here (`spawn_llm_for_prompt`
    /// refuses to run a turn without one) — the `<model>` addendum fields
    /// are simply omitted. Previewing a prompt is exactly the thing you
    /// want to do while a context's model is still unconfigured.
    ///
    /// Model source and the staging guard both mirror the turn path
    /// exactly:
    ///
    /// - **Model source.** Both sides call the same `context_model`, which
    ///   reads the context's slots, so a preview can't show a `<model>`
    ///   the turn would not pick.
    /// - **Staging.** A `Staging` context is refused here exactly as
    ///   `spawn_llm_for_prompt` refuses it, instead of rendering a prompt
    ///   that could not actually run.
    async fn context_prompt(&self, context: Option<&str>, caller: &KjCaller) -> KjResult {
        // Same !Send guard-scoping constraint as context_info: KernelDb's
        // parking_lot::MutexGuard must not cross an `.await` below.
        let (target_id, row, bound_model, performer, _) = {
            let db = self.kernel_db().lock();

            // Resolve target context (default: current)
            let target_id = match super::refs::resolve_context_arg(context, caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj context prompt: {e}")),
            };

            let row = match db.get_context(target_id) {
                Ok(Some(r)) => r,
                Ok(None) => return KjResult::Err("kj context prompt: not found".to_string()),
                Err(e) => return KjResult::Err(format!("kj context prompt: {e}")),
            };

            let bound_model = match crate::model_resolution::context_model(&db, target_id) {
                Ok(found) => found,
                Err(e) => return KjResult::Err(format!("kj context prompt: model binding: {e}")),
            };

            let character = |principal_id: PrincipalId| -> Result<crate::llm::CharacterIdentity, KjResult> {
                let sheet = db.get_character(principal_id)
                    .map_err(|e| KjResult::Err(format!("kj context prompt: character lookup: {e}")))?
                    .ok_or_else(|| KjResult::Err(format!("kj context prompt: assigned character {principal_id} has no sheet")))?;
                if sheet.retired_at.is_some() {
                    return Err(KjResult::Err(format!("kj context prompt: assigned character {} is retired", sheet.name)));
                }
                Ok(crate::llm::CharacterIdentity { principal_id, name: sheet.name })
            };
            let performer = match row.played_by.map(character).transpose() {
                Ok(value) => value,
                Err(error) => return error,
            };
            (target_id, row, bound_model, performer, ())
        };
        // The senpai this context answers to, the same fact the turn path
        // reads (llm_stream.rs).
        let (character, senpai) = match crate::llm::system_prompt::character_facts(&self.kernel_db().lock(), target_id) {
            Ok(facts) => facts,
            Err(error) => return KjResult::Err(format!("kj context prompt: context character: {error}")),
        };

        // Situational label and state come from the live DriftRouter
        // handle, NOT the KernelDb row — mirroring exactly what
        // `spawn_llm_for_prompt` reads for a real turn (llm_stream.rs). A
        // resolved target_id with no drift handle renders both absent, same
        // as the turn path would.
        let (ctx_label, ctx_state) = {
            let drift = self.drift_router().read();
            match drift.get(target_id) {
                Some(h) => (h.label.clone(), Some(h.state)),
                None => (None, None),
            }
        };

        // Mirror the turn path's staging guard (llm_stream.rs's
        // `spawn_llm_for_prompt`) exactly: a staged context refuses a turn,
        // so this preview refuses too rather than rendering a prompt that
        // could not actually run.
        if ctx_state == Some(ContextState::Staging) {
            return KjResult::Err(
                "kj context prompt: context is in staging mode — commit to enable LLM prompts"
                    .to_string(),
            );
        }

        // The model the slots give the context's type. Unlike the turn
        // path, a context with no model does not error here: previewing a
        // prompt is what you do before fixing the cast.
        let (resolved_backend, resolved_model) = match &bound_model {
            crate::model_resolution::ContextModelLookup::Model(m) => (Some(m.backend.clone()), Some(m.model.clone())),
            crate::model_resolution::ContextModelLookup::None(_) => (None, None),
        };

        // rc sections: the (Role::System, BlockKind::Text) blocks the rc
        // create/fork lifecycle dropped into this context's block store —
        // the SAME extractor the turn path calls, so this can't drift from
        // what's really sent.
        let rc_sections = match crate::llm::read_system_prompt_sections(self.block_store(), target_id) {
            Ok(sections) => sections,
            Err(e) => return KjResult::Err(format!(
                "kj context prompt: could not read this context's instruction blocks: {e}"
            )),
        };

        let situational = crate::llm::SituationalContext {
            context_id: Some(target_id),
            context_label: ctx_label.clone(),
            context_state: ctx_state,
            provider: resolved_backend.clone(),
            model: resolved_model.clone(),
            performer,
            character,
            senpai,
        };
        let prompt = crate::llm::build_system_prompt(&situational, &rc_sections);

        // The <situation> tail, sliced out of the already-assembled prompt
        // rather than re-derived from `situational` — guarantees
        // `data.situation` can never disagree with what actually landed in
        // `data.prompt` (empty when situational was empty and no
        // <situation> block was emitted).
        let situation = prompt
            .find("<situation>")
            .map(|idx| prompt[idx..].to_string())
            .unwrap_or_default();

        let label_display = ctx_label
            .or(row.label.clone())
            .unwrap_or_else(|| target_id.short());
        let char_count = prompt.chars().count();
        let message =
            format!("Context: {label_display} ({})\nChars:   {char_count}\n\n{prompt}", target_id.short());

        // Structured record: the assembled string plus the individual
        // layers, so a caller can diff which selected section changed (or just
        // the live tool list) without re-running the
        // assembly itself.
        let record = serde_json::json!({
            "context_id": target_id.to_hex(),
            "label": label_display,
            "char_count": char_count,
            "resolved_backend": resolved_backend,
            "resolved_model": resolved_model,
            "prompt": prompt,
            "rc_sections": rc_sections,
            "situation": situation,
        });

        KjResult::ok_with_data(message, record)
    }

    async fn context_current(&self, caller: &KjCaller) -> KjResult {
        let Some(ctx_id) = caller.context_id else {
            return KjResult::ok("No active context joined. Use 'kj context switch' to join one.");
        };

        let router = self.drift_router().read();
        let label = router
            .get(ctx_id)
            .and_then(|h| h.label.clone())
            .unwrap_or_else(|| "(unlabeled)".into());

        KjResult::ok(format!(
            "Current context: {} [{}]",
            label,
            ctx_id.to_hex()
        ))
    }

    async fn context_switch(&self, query: &str, caller: &KjCaller) -> KjResult {
        let ctx_ref = parse_context_ref(query);

        // Resolve using DriftRouter for live state (not just DB)
        let resolved = {
            let db = self.kernel_db().lock();
            resolve_context_ref(&ctx_ref, caller, &db)
        };

        match resolved {
            Ok(target_id) => {
                if Some(target_id) == caller.context_id {
                    return KjResult::ok("already in that context".to_string());
                }
                // Get label for display
                let label = {
                    let router = self.drift_router().read();
                    router
                        .get(target_id)
                        .and_then(|h| h.label.clone())
                        .unwrap_or_else(|| target_id.short())
                };
                KjResult::Switch(target_id, format!("switched to {}", label))
            }
            Err(e) => KjResult::Err(format!("kj context switch: {e}")),
        }
    }

    /// `kj kohai create` (`docs/senpai-kohai.md`, "Creating, forking,
    /// rotating"): makes a new context performed by `character`. Its senpai
    /// is the caller's context, or the context `senpai` names when the
    /// caller's character is that context's character or its lineage root.
    /// `parent` is provenance only
    /// and defaults to the caller's context. The caller must be a live
    /// character.
    pub(crate) async fn kohai_create(
        &self,
        label: Option<&str>,
        parent: Option<&str>,
        senpai: Option<&str>,
        character: &str,
        cwd_optional: bool,
        config: ContextConfigArgs,
        caller: &KjCaller,
    ) -> KjResult {
        let mut cfg: ContextConfig = config.into();
        // Label comes from --name/-n (fork-parity) or the first positional
        // argument (resolved in the dispatcher as `name.or(label)`).
        let label = match label {
            Some(l) => l,
            None => {
                return KjResult::Err(
                    "kj kohai create: requires a label (positional or --name)".to_string(),
                );
            }
        };

        // A new context answers to its creator's context, so only a live
        // character may create one.
        match self.kernel_db().lock().get_character(caller.actor_id) {
            Ok(Some(sheet)) if sheet.retired_at.is_none() => {}
            Ok(Some(sheet)) => return KjResult::Err(format!(
                "kj kohai create: {} is retired and cannot create a context", sheet.name
            )),
            Ok(None) => return KjResult::Err(
                "kj kohai create: the caller has no character sheet; a new context answers to its creator, \
                 so only a live character may create one"
                    .to_string(),
            ),
            Err(e) => return KjResult::Err(format!("kj kohai create: {e}")),
        }

        // `--parent` names the provenance explicitly, or it defaults to the
        // caller's current context. A caller with no current context and no
        // `--parent` refuses rather than minting a stray root context —
        // create never mints a parentless row; boot's `ensure_root_contexts`,
        // `kj context scratch`, and a few other internal paths do.
        let parent_id = {
            let db = self.kernel_db().lock();
            match super::refs::resolve_context_arg(parent, caller, &db) {
                Ok(id) => Some(id),
                Err(_) if parent.is_none() => {
                    return KjResult::Err(
                        "kj kohai create: no current context and no --parent given; \
                         name one with --parent <context>"
                            .to_string(),
                    );
                }
                Err(e) => return KjResult::Err(format!("kj kohai create: {e}")),
            }
        };

        // The senpai: the context `--senpai` names, which the caller's
        // character must be the character of or the lineage root of, or else
        // the caller's own context. A caller with no context has nothing to
        // answer to, and the kernel does not guess one.
        let senpai_ctx = {
            let db = self.kernel_db().lock();
            let (context, via_flag) = match senpai {
                Some(senpai) => match super::refs::resolve_context_arg(Some(senpai), caller, &db) {
                    Ok(id) => (id, true),
                    Err(e) => return KjResult::Err(format!("kj kohai create: --senpai: {e}")),
                },
                None => match caller.context_id {
                    Some(id) => (id, false),
                    None => return KjResult::Err(
                        "kj kohai create: the new context's senpai is the context it is created from, \
                         and the caller has no current context; run this from the context that should \
                         supervise it, or name one with --senpai"
                            .to_string(),
                    ),
                },
            };
            match db.context_character(context) {
                Ok(Some(_)) => {}
                Ok(None) => return KjResult::Err(format!(
                    "kj kohai create: context {} has no context character, so nothing can answer to it; \
                     name a context that has one with --senpai",
                    db.context_name(context)
                )),
                Err(e) => return KjResult::Err(format!("kj kohai create: {e}")),
            }
            // By default, anyone may start a kohai in a root's own console
            // (the gate's ask to the root is the control there), and a
            // context's own rc lifecycle may start one under itself, as it
            // may archive its own kohai. Every other context, and any
            // explicit --senpai, follows the placing rule.
            let in_rc_lifecycle = caller.privileged && caller.rc_depth > 0;
            let exempt = !via_flag && (in_rc_lifecycle || match db.is_root_console(context) {
                Ok(root_console) => root_console,
                Err(e) => return KjResult::Err(format!("kj kohai create: {e}")),
            });
            if !exempt {
                match db.may_place_under(context, caller.actor_id) {
                    Ok(Ok(_)) => {}
                    Ok(Err(refusal)) => return KjResult::Err(format!("kj kohai create: {refusal}")),
                    Err(e) => return KjResult::Err(format!("kj kohai create: {e}")),
                }
            }
            context
        };

        // `--type` is pulled out here so it lands on the row up front (the rc
        // create-lifecycle dispatches on context_type); the rest is applied
        // after the context exists.
        // --type <context_type> selects which rc scripts run for this
        // context. Default is "default" — runs scripts under
        // /config/rc/default/<verb>/.
        let context_type = cfg.type_spec.take().unwrap_or_else(|| "default".to_string());

        // Refuse an unlisted type before any mutation — a typo'd --type must
        // not silently create a context with no rc bucket to run.
        if let Err(e) = super::rc::check_context_type(self.kernel().vfs(), &context_type).await {
            return KjResult::Err(format!("kj kohai create: {e}"));
        }

        // Refuse a type whose `create` bucket is missing or holds no
        // candidate script, also before any mutation. `load_scripts` reads
        // an absent directory as zero scripts, not a failure — legitimate
        // for verbs a type has no work at, but `create` is the lifecycle
        // that binds a context's loadout. Left unchecked, this type would
        // commit a context whose create lifecycle runs nothing: an inert
        // row (deny-by-default, no loadout) with no error to explain it.
        match crate::rc::create_bucket_has_candidates(self, &context_type).await {
            Ok(true) => {}
            Ok(false) => {
                return KjResult::Err(format!(
                    "kj kohai create: context type '{context_type}' has no rc create \
                     scripts at {}; a context of this type would be committed inert \
                     (deny-by-default, no loadout) with no create lifecycle to bind one. \
                     Add a script there, or run `kaijutsu-server rc reseed` to restore the \
                     shipped bucket.",
                    kaijutsu_types::paths::rc_dir(&context_type, crate::rc::VERB_CREATE),
                ));
            }
            Err(e) => {
                return KjResult::Err(format!(
                    "kj kohai create: could not check the '{context_type}' create bucket \
                     for scripts: {e}"
                ));
            }
        }

        // The beat is no longer a Rust special-case here: arming a musician is an
        // rc step (`musician/create/S20-arm.kai` runs `kj transport arm`), so a
        // context_type is a beat participant exactly when its `create/` rc arms it
        // — no `context_type == "musician"` branch, and new beat-bearing roles
        // (funkMusician, …) need no kernel edit. `docs/chameleon.md`,
        // "context_type is an rc bundle of features".

        // `--cwd-optional` drops a `--cwd` the kernel cannot use, and the
        // create continues as if it had not been given.
        let mut cwd_refused = None;
        if cwd_optional
            && let Some(cwd) = cfg.cwd_spec.clone()
            && let Some(check_in) = parent_id.or(caller.context_id)
            && let Err(e) = self.check_cwd(caller, check_in, &cwd).await
        {
            cwd_refused = Some(format!("--cwd {e}"));
            cfg.cwd_spec = None;
        }

        // Without `--cwd`, the new context starts in the caller's cwd, as
        // `kj fork` does. A cwd that no longer resolves refuses the create.
        if cfg.cwd_spec.is_none()
            && let Some(current) = caller.context_id
        {
            let inherited = match crate::runtime::shell_state::read_context_cwd(&self.kernel_db().lock(), current) {
                Ok(cwd) => cwd,
                Err(e) => return KjResult::Err(format!("kj kohai create: {e}")),
            };
            if let Some(cwd) = inherited {
                let cwd = cwd.to_string_lossy().into_owned();
                if let Err(e) = self.check_cwd(caller, current, &cwd).await {
                    return KjResult::Err(format!(
                        "kj kohai create: the current context's cwd {e}; name the new \
                         context's directory with --cwd DIR"
                    ));
                }
                cfg.cwd_spec = Some(cwd);
            }
        }

        // Validate the rest before any mutation so a typo'd --env or --cwd
        // can't leave an orphan context behind.
        let Some(cwd_check_context) = parent_id.or(caller.context_id) else {
            return KjResult::Err("kj kohai create: no parent context".into());
        };
        if let Err(e) = self.resolve_context_config(&cfg, caller, cwd_check_context).await {
            return KjResult::Err(format!("kj kohai create: {e}"));
        }
        let new_id = ContextId::new();

        // Resolve the performer before validating its model assignment. The
        // final insertion repeats the liveness and reviewer checks under its
        // transaction because either can change while that async validation runs.
        // The requester remains the context creator; rc output belongs to the
        // invoking performer.
        let admission = {
            let performer = {
                let db = self.kernel_db().lock();
                match db.get_character_by_name(character) {
                    Ok(Some(row)) if row.retired_at.is_some() => return KjResult::Err(format!(
                        "kj kohai create: character '{character}' is retired; choose a live character with `kj character list`"
                    )),
                    Ok(Some(row)) => row.principal_id,
                    Ok(None) => return KjResult::Err(format!(
                        "kj kohai create: no character named '{character}'; use `kj character list` or `kj character create <name>`"
                    )),
                    Err(e) => return KjResult::Err(format!("kj kohai create: {e}")),
                }
            };
            // The context does not exist yet, so routing starts at the senpai
            // it will name.
            if let Err(error) = self.kernel().validate_model_review_assignment(crate::kernel_db::ReviewFrom::Senpai(senpai_ctx), performer).await {
                return KjResult::Err(format!("kj kohai create: {error}"));
            }
            let row = ContextRow {
                context_id: new_id,
                label: Some(label.to_string()),
                system_prompt: None,
                context_state: ContextState::Live,
                context_type,
                created_at: kaijutsu_types::now_millis() as i64,
                created_by: caller.principal_id,
                forked_from: parent_id,
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
                played_by: None,
            };
            let db = self.kernel_db().lock();
            if let Err(e) = insert_new_context_checked(&db, &row, CharacterPlacement::Start { performer, senpai: Some(senpai_ctx) }) {
                return KjResult::Err(format!("kj kohai create: {e}"));
            }
            crate::runtime::admission::ContextAdmission::for_inserted(&row)
        };

        // Register in DriftRouter
        {
            let mut drift = self.drift_router().write();
            if let Err(e) = drift.register(new_id, Some(label), parent_id, caller.principal_id) {
                return KjResult::Err(format!("kj kohai create: {e}"));
            }
        }

        // Apply settable config (system-prompt, cwd, env) now that the row
        // and drift handle exist. Validated above; only DB I/O errors surface
        // here.
        let config_changes = match self.apply_context_config(new_id, &cfg).await {
            Ok(changes) => changes,
            Err(e) => return KjResult::Err(format!("kj kohai create: {e}")),
        };

        // Run rc create-lifecycle scripts after committing the context. Script
        // failures retain their Error blocks. Lifecycle bookkeeping faults
        // report the committed state; retained settlement retries without
        // running source. Rebind only repairs a missing loadout.
        let lifecycle = crate::rc::run(
            self,
            crate::rc::RcInvocation {
                parent: parent_id,
                ..crate::rc::RcInvocation::new("create", &admission, &caller.cancel)
            },
            caller,
        )
            .await;

        if caller.cancel.is_cancelled() {
            return KjResult::Err(format!(
                "kj kohai create: context '{}' ({}) was committed, but its rc \
                 create lifecycle was cancelled",
                label,
                new_id.short()
            ));
        }
        if let Err(e) = lifecycle {
            return KjResult::Err(format!(
                "kj kohai create: context '{}' ({}) was committed, but its rc create lifecycle could not settle: {e}",
                label, new_id.short()
            ));
        }

        let mut msg = format!("created context '{}' ({})", label, new_id.short());
        if !config_changes.is_empty() {
            msg.push_str(&format!(" [{}]", config_changes.join(", ")));
        }
        if let Some(line) = cfg.cwd_spec.as_deref().and_then(|cwd| crate::runtime::shell_state::root_cwd_line(new_id, cwd)) {
            msg.push('\n');
            msg.push_str(&line);
        }
        // Say so when the new context has no model for its type.
        let no_model = match crate::model_resolution::context_model(&self.kernel_db().lock(), new_id) {
            Ok(crate::model_resolution::ContextModelLookup::None(none)) => Some(none.line()),
            Ok(_) => None,
            Err(e) => Some(format!("could not read the context slots: {e}")),
        };
        if let Some(line) = &no_model {
            msg.push('\n');
            msg.push_str(line);
        }
        if let Some(refused) = &cwd_refused {
            msg.push_str(&format!("\n{refused}; created without it"));
        }
        // Report whether the retained context received a usable loadout.
        match self.has_usable_loadout(new_id) {
            Ok(true) => {}
            Ok(false) => msg.push_str(
                " — WARNING: its rc create lifecycle left it with no loadout, so it \
                 is inert (deny-by-default) and cannot widen itself. \
                 Read its Error blocks for why, then `kj context rebind` to repair.",
            ),
            Err(e) => msg.push_str(&format!(
                " — WARNING: could not read back its loadout to confirm the rc create \
                 lifecycle bound it: {e}"
            )),
        }
        KjResult::ok_with_data(msg, serde_json::json!({
            "context_id": new_id.to_hex(),
            "no_model": no_model,
            "cwd_refused": cwd_refused,
        }))
    }

    /// `kj context rebind [<ctx>]` — re-run the `create` rc lifecycle against a
    /// context that has no usable loadout. Context creation retains the context
    /// and its Error blocks when lifecycle policy fails; rebind repairs only
    /// that missing-loadout state.
    ///
    /// **Ungated on the same argument that leaves `create` ungated** (see
    /// `dispatch_context`): the loadout comes from the rc lifecycle, not from
    /// the caller, so a rebind grants exactly what birth would have granted and
    /// nothing the caller picked. The guards below are what keep it a *repair*
    /// rather than a general "re-run rc" button:
    ///
    /// * **no usable loadout** — gating on the outcome, not on which script
    ///   failed, catches both a binding step that errored and one that ran and
    ///   bound nothing. It also stops a warm context from re-running `create`
    ///   scripts, which are not idempotent (`S00-stance` appends a stance,
    ///   `S20-arm` arms a beat).
    /// * **not archived** — archived contexts are retained work, inert by
    ///   design; unarchive first if a repair is really wanted.
    ///
    /// A context that fails the read-back afterwards is reported as a failure,
    /// not as a cheerful success: the binding step is still broken and the
    /// operator needs to know that before they try to use the context.
    async fn context_rebind(&self, target_arg: Option<&str>, caller: &KjCaller) -> KjResult {
        let (row, admission) = {
            let db = self.kernel_db().lock();
            let target_id = match super::refs::resolve_context_arg(target_arg, caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj context rebind: {e}")),
            };
            let row = match db.get_context(target_id) {
                Ok(Some(row)) => row,
                Ok(None) => {
                    return KjResult::Err(format!(
                        "kj context rebind: context {} not found",
                        target_id.short()
                    ));
                }
                Err(e) => return KjResult::Err(format!("kj context rebind: {e}")),
            };
            let admission = match crate::runtime::admission::ContextAdmission::acquire(&db, target_id) {
                Ok(admission) => admission,
                Err(e) => return KjResult::Err(format!("kj context rebind: {e}")),
            };
            (row, admission)
        };
        let target_id = row.context_id;

        if row.is_archived() {
            return KjResult::Err(format!(
                "kj context rebind: context {} is archived — archived contexts are \
                 retained work, inert by design; unarchive it first if it really \
                 needs a loadout",
                target_id.short()
            ));
        }

        match self.has_usable_loadout(target_id) {
            Ok(false) => {}
            Ok(true) => {
                return KjResult::Err(format!(
                    "kj context rebind: context {} already has a loadout — nothing to \
                     repair. Rebind is not a way to re-run `create` scripts on a warm \
                     context; they append stances and arm beats, and are not idempotent.",
                    target_id.short()
                ));
            }
            Err(e) => {
                return KjResult::Err(format!(
                    "kj context rebind: could not read context {}'s loadout, so there is \
                     no way to tell a repair from a re-run — refusing rather than \
                     guessing: {e}",
                    target_id.short()
                ));
            }
        }

        match self.rebind_loadout(target_id, row.forked_from, &admission, caller).await {
            Ok(true) => KjResult::ok(format!(
                "rebound context {} — the rc create lifecycle assigned it a loadout",
                target_id.short()
            )),
            Ok(false) => KjResult::Err(format!(
                "kj context rebind: the rc create lifecycle ran but context {} still has \
                 no loadout — its binding step is still failing. Read the context's \
                 Error blocks for the reason.",
                target_id.short()
            )),
            Err(e) => KjResult::Err(format!("kj context rebind: {e}")),
        }
    }

    /// Re-run the rc `create` lifecycle against a context with no usable
    /// loadout, then report whether it has one afterward. Shared by `kj
    /// context rebind` and boot's `ensure_root_contexts`
    /// (`crate::kj::character`), which repair the same missing-loadout state
    /// through the same path — a rebind grants exactly what the `create`
    /// lifecycle would have granted, never something the caller picks.
    ///
    /// Individual script failures land as Error blocks rather than an `Err`
    /// from the lifecycle, so the read-back after `rc::run` is what actually
    /// says whether the repair took — an `Ok(false)` here means the lifecycle
    /// ran to completion and still bound nothing, not that it failed to run.
    pub(crate) async fn rebind_loadout(
        &self,
        context_id: ContextId,
        forked_from: Option<ContextId>,
        admission: &crate::runtime::admission::ContextAdmission,
        caller: &KjCaller,
    ) -> Result<bool, String> {
        crate::rc::run(
            self,
            crate::rc::RcInvocation {
                parent: forked_from,
                ..crate::rc::RcInvocation::new("create", admission, &caller.cancel)
            },
            caller,
        )
        .await
        .map_err(|e| format!("rc create lifecycle: {e}"))?;

        self.has_usable_loadout(context_id).map_err(|e| {
            format!(
                "lifecycle ran, but reading context {}'s loadout back failed, so the \
                 repair is unconfirmed: {e}",
                context_id.short()
            )
        })
    }

    /// `kj context scratch` — get-or-create the well-known "scratch"
    /// context (the DM-yourself pattern, M5-F7). Idempotent: returns
    /// the existing context if labeled "scratch" already exists.
    async fn context_scratch(&self, caller: &KjCaller) -> KjResult {
        const SCRATCH_LABEL: &str = "scratch";

        // Resolve the label first; if found, return its id.
        {
            let db = self.kernel_db().lock();
            if let Ok(id) = db.resolve_context(SCRATCH_LABEL) {
                return KjResult::ok(format!(
                    "scratch context exists: {} ({})",
                    SCRATCH_LABEL,
                    id.short()
                ));
            }
        }

        // Otherwise create it.
        let new_id = ContextId::new();
        {
            let db = self.kernel_db().lock();
            let default_ws = match db.get_or_create_default_workspace(caller.principal_id) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj context scratch: {e}")),
            };
            let row = ContextRow {
                context_id: new_id,
                label: Some(SCRATCH_LABEL.to_string()),
                system_prompt: None,
                context_state: ContextState::Live,
                context_type: "default".to_string(),
                created_at: kaijutsu_types::now_millis() as i64,
                created_by: caller.principal_id,
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
                played_by: None,
            };
            if let Err(e) = db.insert_context_with_document(&row, default_ws) {
                return KjResult::Err(format!("kj context scratch: {e}"));
            }
        }
        {
            let mut drift = self.drift_router().write();
            if let Err(e) = drift.register(new_id, Some(SCRATCH_LABEL), None, caller.principal_id) {
                return KjResult::Err(format!("kj context scratch: {e}"));
            }
        }
        KjResult::ok(format!(
            "created scratch context: {} ({})",
            SCRATCH_LABEL,
            new_id.short()
        ))
    }

    /// The one authority rule for `act` on `target`, as a refusal message
    /// (`docs/senpai-kohai.md`, "Authority").
    pub(crate) fn require_authority(
        &self,
        target: ContextId,
        caller: &KjCaller,
        act: crate::kernel_db::GovernedAct,
    ) -> Result<crate::kernel_db::Authority, String> {
        match self.kernel_db().lock().context_authority(target, caller.actor_id, act) {
            Ok(rule) => rule,
            Err(e) => Err(format!("could not check authority to {}: {e}", act.as_str())),
        }
    }

    /// The authority rule for a verb nested in an rc lifecycle, with the
    /// context whose lifecycle runs as the actor: passes only when the
    /// target's senpai is that context (its kohai, such as a judge shadow).
    /// A lifecycle has no more power than its context, so the context's
    /// character lends it nothing. A lifecycle with no context refuses.
    pub(crate) fn require_lifecycle_authority(
        &self,
        target: ContextId,
        caller: &KjCaller,
        act: crate::kernel_db::GovernedAct,
    ) -> Result<(), String> {
        let db = self.kernel_db().lock();
        let checked = (|| -> crate::kernel_db::KernelDbResult<Result<(), String>> {
            let Some(seat) = caller.context_id else {
                return Ok(Err(format!("an rc lifecycle with no context may not {}", act.as_str())));
            };
            let row = db.get_context(target)?.ok_or_else(|| crate::kernel_db::KernelDbError::NotFound(format!(
                "context {} has no row to check authority over", target.short()
            )))?;
            if db.context_character(target)?.is_some_and(|character| character.senpai_ctx == Some(seat)) {
                return Ok(Ok(()));
            }
            let seat_label = db.get_context(seat)?.and_then(|r| r.label).unwrap_or_else(|| seat.short());
            let target_label = row.label.unwrap_or_else(|| target.short());
            Ok(Err(format!(
                "{seat_label}'s rc lifecycle may {} only its own kohai, and {target_label} is not one",
                act.as_str()
            )))
        })();
        match checked {
            Ok(rule) => rule,
            Err(e) => Err(format!("could not check authority to {}: {e}", act.as_str())),
        }
    }

    /// `kj context set <ctx> [--system-prompt text] [--cwd path] [--env KEY=VALUE] [--type t]`
    async fn context_set(
        &self,
        target_arg: Option<&str>,
        egress_allow: &[String],
        egress_deny: &[String],
        mut cfg: ContextConfig,
        caller: &KjCaller,
    ) -> KjResult {
        // Resolve target (brief lock; resolver borrows the db).
        let target_id = {
            let db = self.kernel_db().lock();
            match super::refs::resolve_context_arg(target_arg, caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj context set: {e}")),
            }
        };

        // Validate the env and cwd before touching the DB.
        if let Err(e) = self.resolve_context_config(&cfg, caller, target_id).await {
            return KjResult::Err(format!("kj context set: {e}"));
        }

        // `--egress-allow`/`--egress-deny` need authority over the target
        // (`docs/egress.md`, "Who changes the list").
        if !egress_allow.is_empty() || !egress_deny.is_empty() {
            let mut allow = Vec::with_capacity(egress_allow.len());
            for host in egress_allow {
                match crate::kernel_db::validate_egress_host(host) {
                    Ok(normalized) => allow.push(normalized),
                    Err(e) => return KjResult::Err(format!("kj context set: {e}")),
                }
            }
            let mut deny = Vec::with_capacity(egress_deny.len());
            for host in egress_deny {
                match crate::kernel_db::validate_egress_host(host) {
                    Ok(normalized) => deny.push(normalized),
                    Err(e) => return KjResult::Err(format!("kj context set: {e}")),
                }
            }
            if let Err(refusal) = self.require_authority(target_id, caller, crate::kernel_db::GovernedAct::Egress) {
                return KjResult::Err(format!("kj context set: {refusal}"));
            }
            cfg.egress = Some(EgressAssignment { allow, deny, caller_actor: caller.actor_id });
        }

        match self.apply_context_config(target_id, &cfg).await {
            Ok(changes) if changes.is_empty() => {
                KjResult::ok("no changes specified".to_string())
            }
            Ok(changes) => {
                let mut msg = format!("updated: {}", changes.join(", "));
                if let Some(line) = cfg.cwd_spec.as_deref().and_then(|cwd| crate::runtime::shell_state::root_cwd_line(target_id, cwd)) {
                    msg.push('\n');
                    msg.push_str(&line);
                }
                KjResult::ok(msg)
            }
            Err(e) => KjResult::Err(format!("kj context set: {e}")),
        }
    }

    /// `kj context unset [<ctx>] --env KEY` — remove an env var from a context.
    fn context_unset(
        &self,
        target_arg: Option<&str>,
        env_key: Option<&str>,
        caller: &KjCaller,
    ) -> KjResult {
        let db = self.kernel_db().lock();
        let target_id = match super::refs::resolve_context_arg(target_arg, caller, &db) {
            Ok(id) => id,
            Err(e) => return KjResult::Err(format!("kj context unset: {e}")),
        };

        if let Some(key) = env_key {
            match db.delete_context_env(target_id, key) {
                Ok(true) => KjResult::ok(format!("unset env {key}")),
                Ok(false) => KjResult::Err(format!("kj context unset: env var '{}' not set", key)),
                Err(e) => KjResult::Err(format!("kj context unset: {e}")),
            }
        } else {
            KjResult::Err("kj context unset: requires --env KEY".to_string())
        }
    }

    /// `kj context hydrate [<ctx>] --window <N> [--mark <block>]` / `--clear` —
    /// set or clear the conversation hydration window.
    ///
    /// With a window the context hydrates only `[0, marker] ∪ last-N` instead of
    /// its whole history — the cost guard for endless musician logs (design:
    /// `docs/chameleon.md`, the hydration marker). The prefix marker defaults to
    /// the context's current tail (pin everything so far, slide a window over what
    /// comes next); a musician's `create` rc sets this once. `--clear` reverts to
    /// hydrating everything. Advancing the marker on a durable revision is the
    /// same call again — an in-place upsert, not a per-turn write (the tail slides
    /// in memory).
    fn context_hydrate(
        &self,
        target_arg: Option<&str>,
        window: Option<u32>,
        mark: Option<&str>,
        clear: bool,
        caller: &KjCaller,
    ) -> KjResult {
        let target_id = {
            let db = self.kernel_db().lock();
            match super::refs::resolve_context_arg(target_arg, caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj context hydrate: {e}")),
            }
        };

        if clear {
            return match self.kernel_db().lock().clear_hydration_policy(target_id) {
                Ok(0) => KjResult::ok("hydration window already unset".to_string()),
                Ok(_) => {
                    KjResult::ok("hydration window cleared — hydrating everything".to_string())
                }
                Err(e) => KjResult::Err(format!("kj context hydrate: {e}")),
            };
        }

        let Some(window) = window else {
            return KjResult::Err(
                "kj context hydrate: --window <N> is required (or --clear)".to_string(),
            );
        };

        // window 0 → prefix-only → the just-inserted user prompt (which lives in
        // the tail) never reaches the wire; the turn answers a prompt the model
        // can't see, or 400s on an assistant-final / empty messages array. The
        // sliding tail must keep at least the triggering turn.
        if window == 0 {
            return KjResult::Err(
                "kj context hydrate: --window must be ≥ 1 (0 would drop the current turn \
                 from the wire); use --clear to hydrate everything"
                    .to_string(),
            );
        }

        // The prefix marker: an explicit `--mark` block key, or the context's
        // current tail (pin everything up to now).
        let marker = match mark {
            Some(key) => match BlockId::from_key(key) {
                Some(id) => {
                    // A parseable but non-existent marker would persist durably
                    // and then fail-safe to the whole log every turn — the cost
                    // guard silently OFF forever. Validate it lives in THIS
                    // context before persisting.
                    match self.block_store().get_block_snapshot(target_id, &id) {
                        Ok(Some(_)) => id,
                        Ok(None) => {
                            return KjResult::Err(format!(
                                "kj context hydrate: --mark block '{key}' is not in this context"
                            ));
                        }
                        Err(e) => {
                            return KjResult::Err(format!(
                                "kj context hydrate: could not verify --mark block '{key}': {e}"
                            ));
                        }
                    }
                }
                None => {
                    return KjResult::Err(format!(
                        "kj context hydrate: invalid --mark block id '{key}'"
                    ));
                }
            },
            None => match self.block_store().last_block_id(target_id) {
                Some(id) => id,
                None => {
                    return KjResult::Err(
                        "kj context hydrate: context has no blocks to anchor the prefix marker"
                            .to_string(),
                    );
                }
            },
        };

        match self
            .kernel_db()
            .lock()
            .set_hydration_policy(target_id, marker, window)
        {
            Ok(()) => KjResult::ok_with_data(
                format!("hydration window set — prefix ≤ {marker}, tail {window} blocks"),
                serde_json::json!({
                    "context_id": target_id.to_hex(),
                    "marker": marker.to_key(),
                    "window": window,
                }),
            ),
            Err(e) => KjResult::Err(format!("kj context hydrate: {e}")),
        }
    }

    /// `kj context log [<ctx>]` — show fork lineage from context up to root.
    fn context_log(&self, target_arg: Option<&str>, caller: &KjCaller) -> KjResult {
        let db = self.kernel_db().lock();

        let target_id = match super::refs::resolve_context_arg(target_arg, caller, &db) {
            Ok(id) => id,
            Err(e) => return KjResult::Err(format!("kj context log: {e}")),
        };

        match db.fork_lineage(target_id) {
            Ok(lineage) => {
                let bound = match context_bindings(&db, lineage.iter().map(|(row, _)| row)) {
                    Ok(bound) => bound,
                    Err(e) => return KjResult::Err(format!("kj context log: {e}")),
                };
                let text = format_fork_lineage(&lineage, caller.context_id, &bound);
                let handles = context_handles(lineage.iter().map(|(row, _)| row));
                KjResult::ok_with_data(text, handles)
            }
            Err(e) => KjResult::Err(format!("kj context log: {e}")),
        }
    }

    /// `kj context archive <ctx>` — retain one context outside the active set. `Destroy`-classed
    /// (`kj/effect.rs`); the dispatcher latches an unconfirmed call
    /// before this handler runs.
    ///
    /// Never recurses. Children keep their `forked_from` and their own
    /// state, so a lineage stays walkable after most of it is archived
    /// (`docs/issues.md`, "Managing roots").
    async fn context_archive(&self, ctx_ref: &str, caller: &KjCaller) -> KjResult {
        let (target_id, target_label) = {
            let db = self.kernel_db().lock();
            let target_id =
                match super::refs::resolve_context_arg(Some(ctx_ref), caller, &db) {
                    Ok(id) => id,
                    Err(e) => return KjResult::Err(format!("kj context archive: {e}")),
                };
            let label = match db.get_context(target_id) {
                Ok(Some(row)) => row.label.unwrap_or_else(|| target_id.short()),
                Ok(None) => {
                    return KjResult::Err(format!(
                        "kj context archive: context {} not found",
                        target_id.short()
                    ));
                }
                Err(error) => {
                    return KjResult::Err(format!(
                        "kj context archive: could not read context {}: {error}",
                        target_id.short()
                    ));
                }
            };
            (target_id, label)
        };
        match self.kernel_db().lock().is_root_console(target_id) {
            Ok(false) => {}
            Ok(true) => return KjResult::Err(format!(
                "kj context archive: {target_label} is a root console, and archiving it would leave its \
                 character with none; `kj context rotate {target_label}` replaces it"
            )),
            Err(e) => return KjResult::Err(format!("kj context archive: {e}")),
        }
        // A nested archive from an rc lifecycle (a coder's archive bucket
        // retiring its judge shadows) is checked with the seat — the
        // context whose lifecycle runs — as the actor, not whoever started
        // the lifecycle: the seat may archive only its own kohai.
        let in_rc_lifecycle = caller.privileged && caller.rc_depth > 0;
        let authority = if in_rc_lifecycle {
            self.require_lifecycle_authority(target_id, caller, crate::kernel_db::GovernedAct::Archive)
        } else {
            self.require_authority(target_id, caller, crate::kernel_db::GovernedAct::Archive).map(|_| ())
        };
        if let Err(refusal) = authority {
            return KjResult::Err(format!("kj context archive: {refusal}"));
        }

        match self.kernel_db().lock().held_drift_refusal(target_id) {
            Ok(None) => {}
            Ok(Some(why)) => return KjResult::Err(format!("kj context archive: {why}")),
            Err(e) => return KjResult::Err(format!("kj context archive: {e}")),
        }
        if let Err(e) = self.run_archive_lifecycle(target_id, caller).await {
            return KjResult::Err(format!("kj context archive: {target_label} stays live: {e}"));
        }

        let archived = {
            let db = self.kernel_db().lock();
            match db.archive_context(target_id) {
                Ok(v) => v,
                Err(e) => return KjResult::Err(format!("kj context archive: {e}")),
            }
        };

        // The in-memory drift router must agree with the row, or an active
        // session can write a drift op that resurrects the context.
        {
            let mut drift = self.drift_router().write();
            let _ = drift.set_state(target_id, ContextState::Archived);
        }

        if archived {
            KjResult::ok(format!("archived {target_label}"))
        } else {
            KjResult::ok(format!("{target_label} already archived"))
        }
    }

    /// Run `target`'s `archive` rc lifecycle while it is still live. An
    /// already archived context admits no work and runs nothing. Script
    /// failures are recorded by the lifecycle and are not errors here;
    /// cancellation and a lifecycle that cannot settle are, and the caller
    /// leaves the context live.
    pub(crate) async fn run_archive_lifecycle(&self, target: ContextId, caller: &KjCaller) -> Result<(), String> {
        let Ok(admission) = self.kernel().admit_context(target) else { return Ok(()) };
        // Boxed: an inline lifecycle future would grow every `kj context`
        // dispatch future past the default test-thread stack.
        let lifecycle = Box::pin(crate::rc::run(
            self,
            crate::rc::RcInvocation::new(crate::rc::VERB_ARCHIVE, &admission, &caller.cancel),
            caller,
        ))
        .await;
        if caller.cancel.is_cancelled() {
            return Err("its archive lifecycle was cancelled".to_string());
        }
        lifecycle.map_err(|e| format!("its archive lifecycle could not settle: {e}"))
    }

    /// `kj context archive --children [<parent>] [--type <type>]`: archive
    /// each live child of the parent (a context forked from it), optionally of one type,
    /// each through [`Self::context_archive`].
    async fn context_archive_children(&self, parent_ref: Option<&str>, context_type: Option<&str>, caller: &KjCaller) -> KjResult {
        let children = {
            let db = self.kernel_db().lock();
            let parent = match super::refs::resolve_context_arg(parent_ref, caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj context archive --children: {e}")),
            };
            match db.fork_children(parent) {
                Ok(children) => children,
                Err(e) => return KjResult::Err(format!("kj context archive --children: {e}")),
            }
        };
        let mut archived = Vec::new();
        for child in children.into_iter().filter(|c| context_type.is_none_or(|t| c.context_type == t)) {
            let result = Box::pin(self.context_archive(&child.context_id.to_hex(), caller)).await;
            if !result.is_ok() {
                return result;
            }
            archived.push(child.label.unwrap_or_else(|| child.context_id.short()));
        }
        if archived.is_empty() {
            return KjResult::ok("no live children to archive".to_string());
        }
        KjResult::ok(format!("archived {}", archived.join(", ")))
    }

    /// `kj context rotate [<ctx>]` — replace a context with a successor that
    /// carries its configuration (`docs/character.md`, "Roots and
    /// rotation"; `docs/senpai-kohai.md`, "Creating, forking, rotating").
    ///
    /// `ctx` names the predecessor. The successor row, its context character
    /// (the same character, slots read fresh from the character's static
    /// settings, the predecessor's senpai), env, and cwd land in one
    /// transaction, unlabeled. The `create` rc lifecycle runs next and cannot
    /// share a transaction. Only a successor with a usable loadout takes over:
    /// one more transaction archives the predecessor and moves its label, ring
    /// seat and `handoff_ctx` and `root_ctx` pointers, after the predecessor's
    /// `archive` rc lifecycle runs. The kernel moves no kohai. Otherwise the
    /// predecessor stays live and the successor stays unlabeled for
    /// inspection. The hydration window is not copied: its marker names a
    /// block in the predecessor.
    ///
    /// A character's handoff log has no context character: its rotation
    /// points `characters.handoff_ctx` at the successor and checks no loadout.
    /// Any other context with no context character refuses.
    async fn context_rotate(&self, ctx_ref: Option<&str>, caller: &KjCaller) -> KjResult {
        let (predecessor, character, handoff_owner) = {
            let db = self.kernel_db().lock();
            let id = match super::refs::resolve_context_arg(ctx_ref, caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj context rotate: {e}")),
            };
            let named = match db.get_context(id) {
                Ok(Some(row)) => row,
                Ok(None) => return KjResult::Err(format!("kj context rotate: context {} not found", id.short())),
                Err(e) => return KjResult::Err(format!("kj context rotate: {e}")),
            };
            let character = match db.context_character(id) {
                Ok(character) => character,
                Err(e) => return KjResult::Err(format!("kj context rotate: {e}")),
            };
            let handoff_owner = match db.list_characters(true) {
                Ok(sheets) => sheets.into_iter().find(|sheet| sheet.handoff_ctx == Some(id)),
                Err(e) => return KjResult::Err(format!("kj context rotate: {e}")),
            };
            (named, character, handoff_owner)
        };
        let name = predecessor.label.clone().unwrap_or_else(|| predecessor.context_id.short());
        if predecessor.is_archived() {
            return KjResult::Err(format!("kj context rotate: {name} is archived; rotate a live context"));
        }
        // Rotation archives the predecessor at its last step; refuse before
        // the successor and its lifecycle run.
        match self.kernel_db().lock().held_drift_refusal(predecessor.context_id) {
            Ok(None) => {}
            Ok(Some(why)) => return KjResult::Err(format!("kj context rotate: {why}")),
            Err(e) => return KjResult::Err(format!("kj context rotate: {e}")),
        }
        let placement = match (&character, &handoff_owner) {
            (Some(character), _) => {
                // The one authority rule, and the context's own character
                // (`docs/senpai-kohai.md`, "Authority").
                if character.principal_id != caller.actor_id
                    && let Err(refusal) = self.require_authority(predecessor.context_id, caller, crate::kernel_db::GovernedAct::Rotate)
                {
                    return KjResult::Err(format!("kj context rotate: {refusal}; its performer may also rotate it"));
                }
                CharacterPlacement::Rotate { predecessor: predecessor.context_id }
            }
            (None, Some(owner)) => {
                // A fresh handoff log: its character, or a live root character, may start one.
                if owner.principal_id != caller.actor_id {
                    let root = self.kernel_db().lock().get_character(caller.actor_id);
                    match root {
                        Ok(Some(sheet)) if sheet.root && sheet.retired_at.is_none() => {}
                        Ok(_) => return KjResult::Err(format!(
                            "kj context rotate: only {} or a live root character may rotate its handoff log", owner.name
                        )),
                        Err(e) => return KjResult::Err(format!("kj context rotate: {e}")),
                    }
                }
                CharacterPlacement::None
            }
            (None, None) => return KjResult::Err(format!(
                "kj context rotate: {name} has no context character and is no character's handoff log, \
                 so there is no character to give a successor to"
            )),
        };
        let is_handoff_log = matches!(placement, CharacterPlacement::None);

        let successor_id = ContextId::new();
        let parent = predecessor.forked_from;
        let successor = ContextRow {
            context_id: successor_id,
            label: None,
            context_state: ContextState::Live,
            created_at: kaijutsu_types::now_millis() as i64,
            created_by: caller.principal_id,
            fork_kind: None,
            archived_at: None,
            concluded_at: None,
            last_activity_at: None,
            promoted_at: None,
            demoted_at: None,
            paused_at: None,
            played_by: None,
            ..predecessor.clone()
        };
        let admission = {
            let db = self.kernel_db().lock();
            let inserted = db.in_transaction(|db| {
                insert_new_context_rows(db, &successor, placement)?;
                db.copy_context_env(predecessor.context_id, successor_id)?;
                db.copy_context_egress(predecessor.context_id, successor_id)?;
                db.set_context_env(successor_id, "ROTATED_FROM", &predecessor.context_id.to_hex())?;
                if let Some(shell) = db.get_context_shell(predecessor.context_id)? {
                    db.upsert_context_shell(&ContextShellRow {
                        context_id: successor_id,
                        updated_at: kaijutsu_types::now_millis() as i64,
                        ..shell
                    })?;
                }
                Ok(())
            });
            if let Err(e) = inserted {
                return KjResult::Err(format!("kj context rotate: {e}"));
            }
            crate::runtime::admission::ContextAdmission::for_inserted(&successor)
        };
        {
            let mut drift = self.drift_router().write();
            if let Err(e) = drift.register(successor_id, None, parent, caller.principal_id) {
                return KjResult::Err(format!("kj context rotate: {e}"));
            }
        }

        let lifecycle = crate::rc::run(
            self,
            crate::rc::RcInvocation { parent, ..crate::rc::RcInvocation::new("create", &admission, &caller.cancel) },
            caller,
        )
        .await;
        if caller.cancel.is_cancelled() {
            return KjResult::Err(format!(
                "kj context rotate: owner cancelled after creating successor {successor_id}; \
                 {name} stays live"
            ));
        }
        if let Err(e) = lifecycle {
            return KjResult::Err(format!(
                "kj context rotate: successor {successor_id} was created, but its rc create lifecycle could not settle; {name} stays live: {e}"
            ));
        }
        // A handoff log runs no tools, so it has no loadout to check.
        if !is_handoff_log {
            match self.has_usable_loadout(successor_id) {
                Ok(true) => {}
                Ok(false) => return KjResult::Err(format!(
                    "kj context rotate: the successor {successor_id} has no loadout after its create lifecycle, so {name} stays live. \
                     Read the successor's Error blocks, then archive it with `kj context archive {successor_id}`"
                )),
                Err(e) => return KjResult::Err(format!(
                    "kj context rotate: could not read the successor's loadout, so {name} stays live: {e}"
                )),
            }
        }

        // The predecessor's archive lifecycle runs while it is still live,
        // as `kj context archive` runs it; the rotation commits after it.
        if let Err(e) = self.run_archive_lifecycle(predecessor.context_id, caller).await {
            return KjResult::Err(format!(
                "kj context rotate: {name} stays live and the successor {successor_id} is unlabeled: {e}. \
                 Scripts its archive lifecycle already ran keep their effects, and a retry runs it again"
            ));
        }
        if caller.cancel.is_cancelled() {
            return KjResult::Err(format!(
                "kj context rotate: owner cancelled before committing successor {successor_id}; \
                 {name} stays live, and its archive lifecycle already ran"
            ));
        }
        let moved = match self.kernel_db().lock().commit_rotation(predecessor.context_id, successor_id) {
            Ok(moved) => moved,
            Err(e) => return KjResult::Err(format!(
                "kj context rotate: {e}; the successor {successor_id} is unlabeled, and {name}'s \
                 archive lifecycle already ran"
            )),
        };
        {
            let mut drift = self.drift_router().write();
            let _ = drift.set_state(predecessor.context_id, ContextState::Archived);
            if let Some(label) = &predecessor.label
                && let Err(e) = drift.rename(successor_id, Some(label))
            {
                return KjResult::Err(format!(
                    "kj context rotate: rotated in the database but the live label index failed to update ({e}); a kernel restart will converge"
                ));
            }
        }

        let mut msg = format!("rotated {name}: {} archived, {} replaces it", predecessor.context_id.short(), successor_id.short());
        if moved > 0 {
            msg.push_str(" (the character's handoff log now points at it)");
        }
        // Say so when the successor has no model for its type.
        let no_model = if is_handoff_log {
            None
        } else {
            match crate::model_resolution::context_model(&self.kernel_db().lock(), successor_id) {
                Ok(crate::model_resolution::ContextModelLookup::None(none)) => Some(none.line()),
                Ok(_) => None,
                Err(e) => Some(format!("could not read the context slots: {e}")),
            }
        };
        if let Some(line) = &no_model {
            msg.push('\n');
            msg.push_str(line);
        }
        KjResult::ok_with_data(msg, serde_json::json!({
            "context_id": successor_id.to_hex(),
            "rotated_from": predecessor.context_id.to_hex(),
            "no_model": no_model,
        }))
    }

    /// Whether the caller's actor is a live root character: a person's
    /// hands, which carry ROOT privilege into any context.
    fn caller_is_live_root(&self, caller: &KjCaller) -> Result<bool, String> {
        let sheet = self.kernel_db().lock().get_character(caller.actor_id)
            .map_err(|e| format!("could not read the caller's character sheet: {e}"))?;
        Ok(sheet.is_some_and(|sheet| sheet.root && sheet.retired_at.is_none()))
    }

    /// Authority to conclude a context or change its ring placement
    /// (`docs/character.md`, "ROOT places contexts").
    ///
    /// - A live root character, acting from any context, is a human root
    ///   and needs no capability. Every other caller needs Operator.
    /// - A human root (a live root character, or a `root` context) may act
    ///   on any context, except that nobody concludes a human root.
    /// - The ROOT console (the `console` context labeled `ROOT`) may act on
    ///   any context, but only a human root demotes or concludes a human
    ///   root or a console.
    /// - Any other seat may act on any context that is not a human root, a
    ///   console, or its own. Its own is the context the call runs in, or
    ///   any context its character plays.
    fn require_placement_authority(&self, verb: &str, target_id: ContextId, caller: &KjCaller) -> Result<(), KjResult> {
        use kaijutsu_types::{CONSOLE_CONTEXT_TYPE, ROOT_CONTEXT_TYPE};
        let (row, caller_row) = {
            let db = self.kernel_db().lock();
            let row = match db.get_context(target_id) {
                Ok(Some(row)) => row,
                Ok(None) => return Err(KjResult::Err(format!("kj context {verb}: context {} not found", target_id.short()))),
                Err(e) => return Err(KjResult::Err(format!("kj context {verb}: {e}"))),
            };
            let caller_row = match caller.context_id.map(|id| db.get_context(id)).transpose() {
                Ok(caller_row) => caller_row.flatten(),
                Err(e) => return Err(KjResult::Err(format!("kj context {verb}: could not read the caller's context: {e}"))),
            };
            (row, caller_row)
        };
        let name = row.label.clone().unwrap_or_else(|| target_id.short());
        let refuse = |why: String| Err(KjResult::Err(format!("kj context {verb}: refused — {why}")));
        let human = match self.caller_is_live_root(caller) {
            Ok(human) => human,
            Err(e) => return refuse(e),
        };
        let human_root = human || caller_row.as_ref().is_some_and(|c| c.context_type == ROOT_CONTEXT_TYPE);
        let console = caller_row.as_ref().is_some_and(|c| c.is_root_console());
        let target_is_root = row.context_type == ROOT_CONTEXT_TYPE;
        let target_is_console = row.context_type == CONSOLE_CONTEXT_TYPE;

        if !human_root && !console {
            if target_is_root || target_is_console {
                return refuse(format!(
                    "{name} is a '{}' context, and only ROOT (the ROOT console or a human root) may {verb} it",
                    row.context_type
                ));
            }
            if caller.context_id == Some(target_id) || row.played_by == Some(caller.actor_id) {
                return refuse(format!(
                    "{name} is this seat's own context, and no seat may {verb} itself. \
                     To have it done, ask ROOT, for example: kj drift push ROOT \"please {verb} {name}\""
                ));
            }
        }
        if !human {
            self.require_cap(caller, crate::mcp::Capability::Operator, &format!("context {verb}")).map_err(|denied| {
                KjResult::Err(format!(
                    "{}\nWithout the 'operator' capability a seat may not {verb} any context. \
                     Ask your director, or ask ROOT.",
                    denied.message()
                ))
            })?;
        }
        if verb == "conclude" && target_is_root {
            return refuse(format!(
                "{name} is a human root. Conclusion has no undo, and a concluded context \
                 refuses drifts. To start its history fresh, run kj context rotate {name}"
            ));
        }
        if console && !human_root && verb != "promote" && (target_is_root || target_is_console) {
            return refuse(format!("only a human root may {verb} {name}, a '{}' context", row.context_type));
        }
        Ok(())
    }

    /// `kj context conclude <ctx>` — the explicit "this work is done" act.
    ///
    /// Sets `concluded_at` + the `Concluded` lifecycle state (KernelDb first as
    /// the authoritative stamp, then the in-memory drift router so `listContexts`
    /// reflects it immediately). Single context — unlike archive it does NOT
    /// recurse into children. Reversible by forking; idempotent (re-concluding a
    /// concluded context is a no-op success). Authority:
    /// [`Self::require_placement_authority`].
    async fn context_conclude(&self, ctx_ref: &str, caller: &KjCaller) -> KjResult {
        let target_id = {
            let db = self.kernel_db().lock();
            match super::refs::resolve_context_arg(Some(ctx_ref), caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj context conclude: {e}")),
            }
        };
        if let Err(denied) = self.require_placement_authority("conclude", target_id, caller) {
            return denied;
        }

        let newly = {
            let db = self.kernel_db().lock();
            match db.conclude_context(target_id) {
                Ok(v) => v,
                Err(e) => return KjResult::Err(format!("kj context conclude: {e}")),
            }
        };

        // Reflect in the drift router (Live/Staging → Concluded). Harmless if the
        // context was already concluded.
        {
            let mut drift = self.drift_router().write();
            let _ = drift.set_state(target_id, ContextState::Concluded);
        }

        if newly {
            KjResult::ok(format!("concluded {}", target_id.short()))
        } else {
            KjResult::ok(format!("{} already concluded", target_id.short()))
        }
    }

    /// `kj context promote <ctx>` — bring a context into ring 0 ("active").
    /// Not latched; authority as for conclude
    /// ([`Self::require_placement_authority`]). First-write-wins
    /// on the timestamp — re-promoting an already-promoted context is a no-op
    /// success. Promoting an ARCHIVED context (reachable by full id only)
    /// resurrects it — promote is the resurrection door; the archive is
    /// memory to drift back from, not trash. Errors loudly when the active
    /// ring is full ([`crate::kernel_db::ACTIVE_RING_CAPACITY`] seats) —
    /// ring 0 is a hand-curated row, so seats never appear or vanish without
    /// an explicit act.
    async fn context_promote(&self, ctx_ref: &str, caller: &KjCaller) -> KjResult {
        let target_id = {
            let db = self.kernel_db().lock();
            match super::refs::resolve_context_arg(Some(ctx_ref), caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj context promote: {e}")),
            }
        };
        if let Err(denied) = self.require_placement_authority("promote", target_id, caller) {
            return denied;
        }

        let outcome = {
            let db = self.kernel_db().lock();
            match db.promote_context(target_id) {
                Ok(v) => v,
                Err(e) => return KjResult::Err(format!("kj context promote: {e}")),
            }
        };

        match outcome {
            PromoteOutcome::Promoted => KjResult::ok(format!("promoted {}", target_id.short())),
            PromoteOutcome::AlreadyPromoted => {
                KjResult::ok(format!("{} already promoted", target_id.short()))
            }
            PromoteOutcome::Resurrected { concluded } => {
                // Mirror archive's DriftRouter sync in reverse: the router
                // still holds Archived and would keep the context dead to
                // sessions. Conclusion is orthogonal to placement — a
                // resurrected concluded context comes back Concluded.
                let state = if concluded {
                    ContextState::Concluded
                } else {
                    ContextState::Live
                };
                {
                    let mut drift = self.drift_router().write();
                    let _ = drift.set_state(target_id, state);
                }
                KjResult::ok(format!(
                    "resurrected {} from the archive — promoted",
                    target_id.short()
                ))
            }
        }
    }

    /// `kj context demote <ctx>` — push a context outward one step on the
    /// demote ladder (see [`crate::kernel_db::KernelDb::demote_context`]).
    /// Authority as for conclude ([`Self::require_placement_authority`]).
    /// Only the ladder's terminal step (archive) latches without
    /// `--confirm` and touches the drift router.
    async fn context_demote(&self, ctx_ref: &str, caller: &KjCaller) -> KjResult {
        let target_id = {
            let db = self.kernel_db().lock();
            match super::refs::resolve_context_arg(Some(ctx_ref), caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj context demote: {e}")),
            }
        };
        if let Err(denied) = self.require_placement_authority("demote", target_id, caller) {
            return denied;
        }

        // The ladder's next step archives an already demoted context. That
        // step latches as `kj context archive` does, and its archive
        // lifecycle runs first, as `kj context archive` runs it.
        let archives = |row: &Option<ContextRow>| {
            matches!(row, Some(row) if row.demoted_at.is_some() && !row.is_archived())
        };
        let latch = || KjResult::Latch {
            command: "kj context demote".to_string(),
            target: ctx_ref.to_string(),
            message: format!(
                "kj context demote {ctx_ref} archives it, because it is already demoted; re-run with --confirm"
            ),
        };
        let archiving = match self.kernel_db().lock().get_context(target_id) {
            Ok(row) => archives(&row),
            Err(e) => return KjResult::Err(format!("kj context demote: {e}")),
        };
        if archiving {
            match self.kernel_db().lock().held_drift_refusal(target_id) {
                Ok(None) => {}
                Ok(Some(why)) => return KjResult::Err(format!("kj context demote: {why}")),
                Err(e) => return KjResult::Err(format!("kj context demote: {e}")),
            }
        }
        if archiving && !caller.confirmed {
            return latch();
        }
        if archiving && let Err(e) = self.run_archive_lifecycle(target_id, caller).await {
            return KjResult::Err(format!("kj context demote: {} stays live: {e}", target_id.short()));
        }

        let outcome = {
            let db = self.kernel_db().lock();
            // Another demote may have landed after the read above; an
            // unconfirmed step still must not archive.
            if !caller.confirmed {
                match db.get_context(target_id) {
                    Ok(row) if archives(&row) => return latch(),
                    Ok(_) => {}
                    Err(e) => return KjResult::Err(format!("kj context demote: {e}")),
                }
            }
            match db.demote_context(target_id) {
                Ok(v) => v,
                Err(e) => return KjResult::Err(format!("kj context demote: {e}")),
            }
        };

        match outcome {
            DemoteOutcome::Unpromoted => KjResult::ok(format!(
                "{} unpromoted (automatic placement)",
                target_id.short()
            )),
            DemoteOutcome::Demoted => KjResult::ok(format!("demoted {}", target_id.short())),
            DemoteOutcome::Archived => {
                let mut drift = self.drift_router().write();
                let _ = drift.set_state(target_id, ContextState::Archived);
                drop(drift);
                KjResult::ok(format!(
                    "{} demoted past the rim — archived",
                    target_id.short()
                ))
            }
        }
    }

    /// `kj context pause <ctx>` / `resume` — set or clear the "suspend
    /// activity" flag. Design-only: no behavioral gating is wired yet (see
    /// the doc on `ContextRow::paused_at`). Not latched, not
    /// capability-gated. Unlike promote/demote this is a plain on/off flag,
    /// not append-ordered — every call unconditionally overwrites.
    async fn context_pause(&self, ctx_ref: &str, caller: &KjCaller, paused: bool) -> KjResult {
        let verb = if paused { "pause" } else { "resume" };
        let target_id = {
            let db = self.kernel_db().lock();
            match super::refs::resolve_context_arg(Some(ctx_ref), caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj context {verb}: {e}")),
            }
        };

        {
            let db = self.kernel_db().lock();
            if let Err(e) = db.set_context_paused(target_id, paused) {
                return KjResult::Err(format!("kj context {verb}: {e}"));
            }
        }

        if paused {
            KjResult::ok(format!("paused {}", target_id.short()))
        } else {
            KjResult::ok(format!("resumed {}", target_id.short()))
        }
    }

    /// `kj context rename <name> [--context <ref>]` — set a context's label.
    /// Unlike `kj context retag` below, this is not latched: it renames the
    /// resolved context's own label rather than moving a label between
    /// contexts.
    ///
    /// DB-first (the UNIQUE constraint on `contexts.label` is the uniqueness
    /// authority — `update_label` maps its violation to a typed "already in
    /// use" error), then the drift router's live label index mirrors it. A
    /// mirror failure after a successful persist is reported loudly rather
    /// than swallowed; a kernel restart converges from the DB.
    async fn context_rename(
        &self,
        name: &str,
        ctx_ref: Option<&str>,
        caller: &KjCaller,
    ) -> KjResult {
        let id = {
            let db = self.kernel_db().lock();
            match super::refs::resolve_context_arg(ctx_ref, caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj context rename: {e}")),
            }
        };

        let old_label = {
            let db = self.kernel_db().lock();
            let old = db
                .get_context(id)
                .ok()
                .flatten()
                .and_then(|row| row.label);
            if let Err(e) = db.update_label(id, Some(name)) {
                return KjResult::Err(format!("kj context rename: {e}"));
            }
            old
        };

        if let Err(e) = self.drift_router().write().rename(id, Some(name)) {
            return KjResult::Err(format!(
                "kj context rename: persisted to DB but the live label index \
                 failed to update ({e}); a kernel restart will converge"
            ));
        }

        let from = old_label.unwrap_or_else(|| id.short());
        KjResult::ok(format!("renamed {from} → '{name}' ({})", id.short()))
    }

    /// `kj context retag <label> <ctx>` — move a label to a different
    /// context. `Destroy`-classed (`kj/effect.rs`); the dispatcher
    /// latches an unconfirmed call before this handler runs.
    async fn context_retag(&self, label: &str, ctx_ref: &str, caller: &KjCaller) -> KjResult {
        // Resolve the new holder and find old holder (single lock scope)
        let (new_holder_id, old_holder) = {
            let db = self.kernel_db().lock();
            let new_holder_id =
                match super::refs::resolve_context_arg(Some(ctx_ref), caller, &db) {
                    Ok(id) => id,
                    Err(e) => return KjResult::Err(format!("kj context retag: {e}")),
                };
            let old_holder = db.find_context_by_label(label).ok().flatten();
            (new_holder_id, old_holder)
        };

        // Apply label changes (single lock scope, no await)
        {
            let db = self.kernel_db().lock();
            if let Some(ref old) = old_holder
                && let Err(e) = db.update_label(old.context_id, None)
            {
                return KjResult::Err(format!("kj context retag: failed to clear old label: {e}"));
            }
            if let Err(e) = db.update_label(new_holder_id, Some(label)) {
                return KjResult::Err(format!("kj context retag: {e}"));
            }
        }

        // Update DriftRouter labels (no db lock held)
        let mut drift = self.drift_router().write();
        if let Some(ref old) = old_holder {
            let _ = drift.rename(old.context_id, None);
        }
        let _ = drift.rename(new_holder_id, Some(label));

        KjResult::ok(format!("retagged '{}' → {}", label, new_holder_id.short()))
    }
}

/// Build the iteration payload for `kj context list`: a JSON array of
/// resolver-friendly handles (label when set, else the **full** context_id
/// hex). These are exactly the strings other kj subcommands accept as
/// `<ctx>`, so `for c in $(kj context list); do kj context info $c; done`
/// round-trips. The rule: `.data` carries full ids; text rendering may
/// truncate to a short prefix for readability.
fn context_handles<'a, I>(rows: I) -> serde_json::Value
where
    I: IntoIterator<Item = &'a ContextRow>,
{
    serde_json::Value::Array(
        rows.into_iter()
            .map(|row| {
                let handle = row
                    .label
                    .clone()
                    .unwrap_or_else(|| row.context_id.to_hex());
                serde_json::Value::String(handle)
            })
            .collect(),
    )
}

// Verb class: kj/effect.rs
use super::effect::{Classify, Effect};

impl Classify for ContextArgs {
    fn effect(&self) -> Effect {
        self.command.effect()
    }
}

impl Classify for ContextConfigArgs {
    fn effect(&self) -> Effect {
        // Only ever flattened into `create`/`set`, both of which write.
        Effect::Write
    }
}

impl Classify for ContextCommand {
    fn effect(&self) -> Effect {
        match self {
            Self::List { .. }
            | Self::Info { .. }
            | Self::Turns { .. }
            | Self::Prompt { .. }
            | Self::Current
            | Self::Log { .. } => Effect::Read,
            Self::Switch { .. }
            | Self::Scratch
            | Self::Rebind { .. }
            | Self::Rotate { .. }
            | Self::Set { .. }
            | Self::Unset { .. }
            | Self::Rename { .. }
            | Self::Conclude { .. }
            | Self::Promote { .. }
            | Self::Demote { .. }
            | Self::Pause { .. }
            | Self::Resume { .. }
            | Self::Hydrate { .. } => Effect::Write,
            Self::Archive { .. } | Self::Retag { .. } => Effect::Destroy,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{insert_new_context_checked, ContextConfig, EgressAssignment, CharacterPlacement};
    #[allow(unused_imports)]
    use crate::kj::KjResult;
    use crate::kj::test_helpers::*;
    use kaijutsu_types::{ContextId, ContextState, PrincipalId};

    fn s(v: &str) -> String {
        v.to_string()
    }

    #[tokio::test]
    async fn context_list_empty() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d.dispatch(&[s("context"), s("list")], &c).await;
        assert!(result.is_ok());
        assert_eq!(result.message(), "(no contexts)");
    }

    #[tokio::test]
    async fn context_list_shows_contexts() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("default"), None, principal);
        let _ = register_context(&d, Some("alt"), None, principal);

        let c = caller_with_context(ctx_id);
        let result = d.dispatch(&[s("context"), s("list")], &c).await;
        assert!(result.is_ok());
        let msg = result.message();
        assert!(msg.contains("default"), "output: {msg}");
        assert!(msg.contains("alt"), "output: {msg}");
        // Current context should be marked
        assert!(msg.contains("*"), "output: {msg}");
    }

    #[tokio::test]
    async fn context_list_tree() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let root = register_context(&d, Some("root"), None, principal);

        let _child = register_context(&d, Some("child"), Some(root), principal);

        let c = caller_with_context(root);
        let result = d
            .dispatch(&[s("context"), s("list"), s("--tree")], &c)
            .await;
        assert!(result.is_ok());
        let msg = result.message();
        assert!(msg.contains("root"), "output: {msg}");
        assert!(msg.contains("child"), "output: {msg}");
    }

    /// A context created after a sibling that already has children lists
    /// after that sibling's whole subtree, so every child sits under its
    /// real parent.
    #[tokio::test]
    async fn context_list_tree_keeps_children_under_their_parent() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let pause = || tokio::time::sleep(std::time::Duration::from_millis(3));
        let root = register_context(&d, Some("amy"), None, principal);
        pause().await;
        let parent = register_context(&d, Some("parent"), Some(root), principal);
        pause().await;
        let _kid_a = register_context(&d, Some("kid-a"), Some(parent), principal);
        pause().await;
        let _kid_b = register_context(&d, Some("kid-b"), Some(parent), principal);
        pause().await;
        let newcomer = register_context(&d, Some("newcomer"), Some(root), principal);

        let result = d
            .dispatch(&[s("context"), s("list"), s("--tree")], &caller_with_context(newcomer))
            .await;
        assert!(result.is_ok(), "{}", result.message());
        let lines: Vec<(usize, &str)> = result
            .message()
            .lines()
            .map(|l| (l.find("└─").map_or(0, |at| at), l))
            .collect();
        let labels: Vec<&str> = lines
            .iter()
            .map(|(_, l)| ["newcomer", "kid-a", "kid-b", "parent", "amy"].into_iter().find(|n| l.contains(n)).unwrap())
            .collect();
        assert_eq!(labels, ["amy", "parent", "kid-a", "kid-b", "newcomer"], "{}", result.message());
        let indent = |label: &str| lines[labels.iter().position(|l| *l == label).unwrap()].0;
        assert!(indent("kid-a") > indent("parent"), "{}", result.message());
        assert_eq!(indent("kid-a"), indent("kid-b"), "{}", result.message());
        assert_eq!(indent("newcomer"), indent("parent"), "{}", result.message());
        assert!(lines[4].1.starts_with('*'), "the caller's row is marked: {}", result.message());
    }

    #[tokio::test]
    async fn context_info_current() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("myctx"), None, principal);
        // `register_context` writes the rows only; a resident document is
        // what carries a version.
        d.blocks
            .create_document(ctx_id, crate::block_store::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(ctx_id);
        let result = d.dispatch(&[s("context"), s("info")], &c).await;
        assert!(result.is_ok());
        let msg = result.message();
        assert!(msg.contains("myctx *"), "output: {msg}");
        assert!(msg.contains("\nVersion: "), "the hydration anchor is shown: {msg}");
        let KjResult::Ok { data: Some(data), .. } = &result else {
            panic!("info carries a record: {result:?}");
        };
        assert!(data["version"].is_u64(), "version rides the record: {data}");
    }

    #[tokio::test]
    async fn context_scratch_creates_then_idempotent() {
        // M5-F7: `kj context scratch` creates the well-known "scratch"
        // context the first time and is a read on subsequent calls.
        let d = test_dispatcher().await;
        let c = test_caller();

        let first = d.dispatch(&[s("context"), s("scratch")], &c).await;
        assert!(first.is_ok(), "first call failed: {}", first.message());
        assert!(
            first.message().contains("created scratch"),
            "first call should report creation, got: {}",
            first.message()
        );

        // Second call must not re-create — db.resolve_context("scratch")
        // returns the existing id.
        let second = d.dispatch(&[s("context"), s("scratch")], &c).await;
        assert!(second.is_ok(), "second call failed: {}", second.message());
        assert!(
            second.message().contains("scratch context exists"),
            "second call should report existing, got: {}",
            second.message()
        );
        let scratch = d.kernel_db().lock().find_context_by_label("scratch").unwrap().unwrap();
        assert_eq!(d.kernel_db().lock().senpai_of(scratch.context_id).unwrap(), None, "scratch is a system context with no parent and no senpai");
    }

    #[tokio::test]
    async fn context_switch_by_label() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx_a = register_context(&d, Some("alpha"), None, principal);
        let _ctx_b = register_context(&d, Some("beta"), None, principal);

        let c = caller_with_context(ctx_a);
        let result = d
            .dispatch(&[s("context"), s("switch"), s("beta")], &c)
            .await;
        match &result {
            KjResult::Switch(id, msg) => {
                assert!(msg.contains("switched to beta"), "msg: {msg}");
                assert_ne!(*id, ctx_a);
            }
            other => panic!("expected Switch, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn context_switch_already_current() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("only"), None, principal);

        let c = caller_with_context(ctx);
        let result = d
            .dispatch(&[s("context"), s("switch"), s("only")], &c)
            .await;
        assert!(result.is_ok());
        assert!(result.message().contains("already"));
    }

    #[tokio::test]
    async fn the_senpai_holder_can_change_the_egress_list_when_the_lineage_root_is_retired() {
        let d = test_dispatcher().await;
        let amy = test_reviewer_principal();
        let lead = PrincipalId::new();
        let coder = PrincipalId::new();
        let context = crate::kj::test_helpers::register_rooted_context(&d, Some("explicit-reviewer"), coder);
        {
            let db = d.kernel_db().lock();
            for (principal_id, name) in [(lead, "lead"), (coder, "coder")] {
                db.insert_character(&crate::kernel_db::CharacterRow {
                    principal_id,
                    name: name.into(),
                    created_at: 0,
                    retired_at: None,
                    handoff_ctx: None, root_ctx: None, root: false,
                }).unwrap();
            }
            db.assign_test_review(context, Some(coder), Some(lead)).unwrap();
            db.conn_for_ledger().execute(
                "UPDATE characters SET retired_at = 1 WHERE principal_id = ?1",
                [amy.as_bytes().as_slice()],
            ).unwrap();
        }
        let lead_caller = crate::kj::KjCaller {
            principal_id: lead,
            actor_id: lead,
            reviewer_id: None,
            context_id: Some(context),
            session_id: kaijutsu_types::SessionId::new(),
            confirmed: false,
            rc_depth: 0,
            privileged: false,
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        let assign = d.dispatch(&[s("context"), s("set"), s("."), s("--egress-allow"), s("crates.io")], &lead_caller).await;
        assert!(assign.is_ok(), "{}", assign.message());
        assert_eq!(d.kernel_db().lock().list_context_egress(context).unwrap(), vec!["crates.io".to_string()]);
    }

    /// The hard cut: the reviewer override and the director column have no
    /// flags left on `kj context set`.
    #[tokio::test]
    async fn context_set_has_no_routing_override_flags() {
        let d = test_dispatcher().await;
        let context = crate::kj::test_helpers::register_rooted_context(&d, Some("no-overrides"), test_reviewer_principal());
        let caller = crate::kj::test_helpers::caller_with_context(context);
        for flag in [vec![s("--reviewer"), s("amy")], vec![s("--clear-reviewer")], vec![s("--director"), s("amy")]] {
            let mut argv = vec![s("context"), s("set"), s(".")];
            argv.extend(flag.clone());
            let result = d.dispatch(&argv, &caller).await;
            assert!(!result.is_ok(), "{flag:?}");
            assert!(result.message().contains("unexpected argument"), "{flag:?}: {}", result.message());
        }
    }

    #[tokio::test]
    async fn context_info_keeps_metadata_when_the_senpai_holder_is_retired() {
        let d = test_dispatcher().await;
        let owner = PrincipalId::new();
        let retired = PrincipalId::new();
        let context = register_context(&d, Some("broken-review"), None, owner);
        {
            let db = d.kernel_db().lock();
            db.insert_character(&crate::kernel_db::CharacterRow { principal_id: retired, name: "retired".into(), created_at: 0, retired_at: Some(1), handoff_ctx: None, root_ctx: None, root: false }).unwrap();
            db.assign_test_review(context, Some(owner), Some(retired)).unwrap();
        }
        let caller = crate::kj::KjCaller { principal_id: owner, actor_id: owner, reviewer_id: None, context_id: Some(context), session_id: kaijutsu_types::SessionId::new(), confirmed: false, rc_depth: 0, privileged: false, cancel: tokio_util::sync::CancellationToken::new() };
        let result = d.dispatch(&[s("context"), s("info"), s(".")], &caller).await;
        assert!(result.is_ok(), "{}", result.message());
        assert!(result.message().contains("Reviewer error:"));
        let KjResult::Ok { data: Some(data), .. } = result else { panic!("context info has data") };
        assert_eq!(data["reviewer_id"], serde_json::Value::Null);
        assert!(data["reviewer_error"].as_str().unwrap().contains("retired"));
        assert_eq!(data["played_by"], owner.to_hex());
    }

    /// `docs/egress.md`, "Who changes the list": the caller must hold the
    /// authority the one rule names. A context's own
    /// model-played performer cannot widen its own egress list; the
    /// context's effective reviewer and its lineage root both can.
    #[tokio::test]
    async fn context_set_egress_allow_requires_the_one_authority_rule() {
        let d = test_dispatcher().await;
        let amy_id = test_reviewer_principal();
        let amy = crate::kj::KjCaller {
            principal_id: amy_id,
            actor_id: amy_id,
            reviewer_id: None,
            context_id: None,
            session_id: kaijutsu_types::SessionId::new(),
            confirmed: false,
            rc_depth: 0,
            privileged: true,
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        let coder = PrincipalId::new();
        let lead = PrincipalId::new();
        for (principal_id, name) in [(coder, "coder"), (lead, "lead")] {
            d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
                principal_id, name: name.into(), created_at: 0, retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
            }).unwrap();
        }
        let context = crate::kj::test_helpers::register_rooted_context(&d, Some("egress-authority"), coder);
        let amy = crate::kj::KjCaller { context_id: Some(context), ..amy };
        d.kernel_db().lock().assign_test_review(context, Some(coder), Some(lead)).unwrap();

        // The played-by character cannot widen its own egress list.
        let coder_call = amy.clone().with_actor(coder, Some(lead));
        let refused = d.dispatch(&[s("context"), s("set"), s("."), s("--egress-allow"), s("crates.io")], &coder_call).await;
        assert!(!refused.is_ok());
        assert!(refused.message().contains("may change its egress list"), "{}", refused.message());
        assert!(d.kernel_db().lock().list_context_egress(context).unwrap().is_empty());

        // The context's effective reviewer (lead) may.
        let lead_call = amy.clone().with_actor(lead, None);
        let allowed = d.dispatch(&[s("context"), s("set"), s("."), s("--egress-allow"), s("crates.io")], &lead_call).await;
        assert!(allowed.is_ok(), "{}", allowed.message());
        assert_eq!(d.kernel_db().lock().list_context_egress(context).unwrap(), vec!["crates.io".to_string()]);

        // The lineage root (amy) may too.
        let allowed = d.dispatch(&[s("context"), s("set"), s("."), s("--egress-allow"), s("example.org")], &amy).await;
        assert!(allowed.is_ok(), "{}", allowed.message());
        assert_eq!(
            d.kernel_db().lock().list_context_egress(context).unwrap(),
            vec!["crates.io".to_string(), "example.org".to_string()],
        );
    }

    /// The holder of a context's senpai may widen its egress list.
    #[tokio::test]
    async fn context_set_egress_allow_is_permitted_for_the_senpai_holder() {
        let d = test_dispatcher().await;
        let amy_id = test_reviewer_principal();
        let director = PrincipalId::new();
        d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
            principal_id: director, name: "banto".into(), created_at: 0, retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
        }).unwrap();
        let context = crate::kj::test_helpers::register_rooted_context(&d, Some("egress-directed"), amy_id);
        d.kernel_db().lock().assign_test_review(context, None, Some(director)).unwrap();
        let director_caller = crate::kj::KjCaller {
            principal_id: director, actor_id: director, reviewer_id: None, context_id: Some(context),
            session_id: kaijutsu_types::SessionId::new(), confirmed: false, rc_depth: 0, privileged: true,
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        let result = d.dispatch(&[s("context"), s("set"), s("."), s("--egress-allow"), s("crates.io")], &director_caller).await;
        assert!(result.is_ok(), "{}", result.message());
        assert_eq!(d.kernel_db().lock().list_context_egress(context).unwrap(), vec!["crates.io".to_string()]);
    }

    /// Mirrors `final_assignment_rejects_reviewer_revoked_after_precheck`:
    /// `apply_context_config`'s transaction re-checks the egress authority
    /// under its own lock rather than trusting the precheck, so a revoke
    /// racing the write is refused instead of landing.
    #[tokio::test]
    async fn final_egress_assignment_rejects_authority_revoked_after_precheck() {
        let d = test_dispatcher().await;
        let amy = test_reviewer_principal();
        let lead = PrincipalId::new();
        let coder = PrincipalId::new();
        let root = crate::kj::test_helpers::register_root_context(&d);
        let context = register_context(&d, Some("stale-egress-reviewer"), Some(root), amy);
        {
            let db = d.kernel_db().lock();
            for (principal_id, name) in [(lead, "lead"), (coder, "coder")] {
                db.insert_character(&crate::kernel_db::CharacterRow { principal_id, name: name.into(), created_at: 0, retired_at: None, handoff_ctx: None, root_ctx: None, root: false }).unwrap();
            }
            db.assign_test_review(context, Some(coder), Some(lead)).unwrap();
        }
        let expected = d.kernel().resolve_context_review(context).await.unwrap();
        assert_eq!(expected.reviewer.principal_id, lead);
        d.kernel_db().lock().set_context_senpai(context, Some(root)).unwrap();
        let cfg = ContextConfig {
            egress: Some(EgressAssignment {
                allow: vec!["crates.io".to_string()],
                deny: vec![],
                caller_actor: lead,
            }),
            ..Default::default()
        };
        let error = d.apply_context_config(context, &cfg).await.unwrap_err();
        assert!(error.contains("authority changed"), "{error}");
        assert!(d.kernel_db().lock().list_context_egress(context).unwrap().is_empty());
    }

    /// Fixture for the authority rule: amy's console → banto's seat → a lane
    /// played by coder and a sibling lane played by sibling. Returns the
    /// lane and a caller factory (Operator, in the lane).
    fn authority_fixture(d: &crate::kj::KjDispatcher) -> (ContextId, [PrincipalId; 4], impl Fn(PrincipalId) -> crate::kj::KjCaller) {
        let amy = test_reviewer_principal();
        let [banto, coder, sibling] = [PrincipalId::new(), PrincipalId::new(), PrincipalId::new()];
        for (principal_id, name) in [(banto, "banto"), (coder, "coder"), (sibling, "sibling"), (PrincipalId::new(), "judge")] {
            d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
                principal_id, name: name.into(), created_at: 0, retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
            }).unwrap();
        }
        let seat = crate::kj::test_helpers::register_rooted_context(d, Some("banto-seat"), banto);
        let lane = register_context(d, Some("lane"), Some(seat), amy);
        let other = register_context(d, Some("other-lane"), Some(seat), amy);
        {
            let db = d.kernel_db().lock();
            db.update_played_by(lane, Some(coder)).unwrap();
            db.update_played_by(other, Some(sibling)).unwrap();
        }
        let caller = move |actor| crate::kj::KjCaller {
            principal_id: actor, actor_id: actor, reviewer_id: None, context_id: Some(lane),
            session_id: kaijutsu_types::SessionId::new(), confirmed: false, rc_depth: 0, privileged: true,
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        (lane, [amy, banto, coder, sibling], caller)
    }

    /// `kj context set --egress-allow` follows the one rule: the lane's
    /// senpai holder (banto) and its lineage root (amy) may; the lane's own
    /// performer and a sibling may not, and the refusal names the rule.
    #[tokio::test]
    async fn context_set_egress_follows_the_one_authority_rule() {
        let d = test_dispatcher().await;
        let (lane, [amy, banto, coder, sibling], caller) = authority_fixture(&d);
        let lane_ref = lane.to_hex();
        for refused in [coder, sibling] {
            for (argv, act) in [
                (vec![s("context"), s("set"), lane_ref.clone(), s("--egress-allow"), s("crates.io")], "change its egress list"),
            ] {
                let result = d.dispatch(&argv, &caller(refused)).await;
                assert!(!result.is_ok(), "{argv:?}");
                assert_eq!(result.message(), format!(
                    "kj context set: only lane ({})'s senpai (banto) or its lineage root (amy) may {act}", lane.short()
                ));
            }
        }
        let row = d.kernel_db().lock().get_context(lane).unwrap().unwrap();
        assert_eq!(row.played_by, Some(coder));
        assert!(d.kernel_db().lock().list_context_egress(lane).unwrap().is_empty());

        for (allowed, host) in [(banto, "crates.io"), (amy, "example.org")] {
            let egress = d.dispatch(&[s("context"), s("set"), lane_ref.clone(), s("--egress-allow"), s(host)], &caller(allowed)).await;
            assert!(egress.is_ok(), "{}", egress.message());
        }
        assert_eq!(d.kernel_db().lock().list_context_egress(lane).unwrap(), vec!["crates.io".to_string(), "example.org".to_string()]);
    }

    /// Rotate and archive follow the same rule. A sibling may do neither;
    /// the lane's own performer may not archive it; the senpai's holder may.
    #[tokio::test]
    async fn rotate_and_archive_follow_the_one_authority_rule() {
        let d = test_dispatcher().await;
        let (lane, [_amy, banto, coder, sibling], caller) = authority_fixture(&d);
        let confirmed = |actor| crate::kj::KjCaller { confirmed: true, ..caller(actor) };
        let rule = |act: &str| format!("only lane ({})'s senpai (banto) or its lineage root (amy) may {act}", lane.short());
        let rotate = d.dispatch(&[s("context"), s("rotate"), lane.to_hex()], &caller(sibling)).await;
        assert!(!rotate.is_ok());
        assert_eq!(rotate.message(), format!("kj context rotate: {}; its performer may also rotate it", rule("rotate it")));
        for refused in [sibling, coder] {
            let archive = d.dispatch(&[s("context"), s("archive"), lane.to_hex()], &confirmed(refused)).await;
            assert!(!archive.is_ok());
            assert_eq!(archive.message(), format!("kj context archive: {}", rule("archive it")));
        }
        assert!(!d.kernel_db().lock().get_context(lane).unwrap().unwrap().is_archived());
        let archive = d.dispatch(&[s("context"), s("archive"), lane.to_hex()], &confirmed(banto)).await;
        assert!(archive.is_ok(), "{}", archive.message());
        assert!(d.kernel_db().lock().get_context(lane).unwrap().unwrap().is_archived());
    }

    #[tokio::test]
    async fn context_create_basic() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);

        let c = caller_with_context(parent);

        crate::kj::test_helpers::give_character(&d, &c);
        let result = d
            .dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("child-ctx")], &c)
            .await;
        assert!(result.is_ok());
        assert!(
            result.message().contains("child-ctx"),
            "msg: {}",
            result.message()
        );
        let KjResult::Ok { data: Some(data), .. } = &result else {
            panic!("context create returns its id in structured data: {result:?}");
        };
        let created_id = data["context_id"]
            .as_str()
            .and_then(|id| kaijutsu_types::ContextId::parse(id).ok());
        assert!(created_id.is_some(), "context_id: {data}");

        // Verify it's in the DB
        let db = d.kernel_db().lock();
        let contexts = db.list_active_contexts().unwrap();
        assert!(
            contexts
                .iter()
                .any(|r| r.label.as_deref() == Some("child-ctx"))
        );
    }

    /// With neither `--parent` nor `--top`, a created context's parent is
    /// the caller's own current context (`docs/fork-and-create.md`, "Where
    /// a created context sits").
    #[tokio::test]
    async fn context_create_defaults_parent_to_the_callers_current_context() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("default-parent"), None, principal);
        let c = caller_with_context(parent);
        crate::kj::test_helpers::give_character(&d, &c);

        let result = d
            .dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("default-child")], &c)
            .await;
        assert!(result.is_ok(), "{}", result.message());

        let db = d.kernel_db().lock();
        let child = db.find_context_by_label("default-child").unwrap().unwrap();
        assert_eq!(child.forked_from, Some(parent));
    }

    /// `--top` is gone (spec answer 2): `--senpai <ctx>` names any other
    /// senpai the caller has authority over.
    #[tokio::test]
    async fn kohai_create_has_no_top_flag() {
        let d = test_dispatcher().await;
        let parent = register_context(&d, Some("no-top-parent"), None, PrincipalId::new());
        let result = d.dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("top-child"), s("--top")], &caller_with_context(parent)).await;
        assert!(!result.is_ok());
        assert!(result.message().contains("unexpected argument '--top'"), "{}", result.message());
        assert!(d.kernel_db().lock().find_context_by_label("top-child").unwrap().is_none());
    }

    /// The new context's senpai is the context it is created from, whatever
    /// `--parent` names: `--parent` is provenance only.
    #[tokio::test]
    async fn context_create_makes_the_callers_context_the_senpai() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let caller_ctx = register_context(&d, Some("senpai-caller"), None, principal);
        let elsewhere = register_context(&d, Some("senpai-elsewhere"), None, principal);
        let c = caller_with_context(caller_ctx);
        crate::kj::test_helpers::give_character(&d, &c);

        let default = d.dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("kohai-default")], &c).await;
        assert!(default.is_ok(), "{}", default.message());
        let named = d.dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("kohai-parented"), s("--parent"), s("senpai-elsewhere")], &c).await;
        assert!(named.is_ok(), "{}", named.message());

        let db = d.kernel_db().lock();
        let default = db.find_context_by_label("kohai-default").unwrap().unwrap();
        assert_eq!((default.forked_from, db.senpai_of(default.context_id).unwrap()), (Some(caller_ctx), Some(caller_ctx)));
        let named = db.find_context_by_label("kohai-parented").unwrap().unwrap();
        assert_eq!(named.forked_from, Some(elsewhere), "--parent still sets provenance");
        assert_eq!(db.senpai_of(named.context_id).unwrap(), Some(caller_ctx), "but the senpai is the context the create ran from");
    }

    /// A create from no context has no senpai to name, and the kernel does
    /// not guess one: it refuses and commits nothing.
    #[tokio::test]
    async fn context_create_with_no_calling_context_refuses_instead_of_guessing_a_senpai() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        register_context(&d, Some("only-parent"), None, principal);
        let caller = crate::kj::KjCaller { context_id: None, ..test_caller() };
        crate::kj::test_helpers::give_character(&d, &caller);
        let result = d.dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("stray"), s("--parent"), s("only-parent")], &caller).await;
        assert!(!result.is_ok(), "{}", result.message());
        assert!(result.message().contains("senpai"), "{}", result.message());
        assert!(d.kernel_db().lock().find_context_by_label("stray").unwrap().is_none());
    }

    /// A create with no current context and no `--parent` refuses
    /// instead of minting a stray root context — only `ensure_root_contexts`
    /// does that, at boot.
    #[tokio::test]
    async fn context_create_refuses_with_no_context_and_no_parent() {
        let d = test_dispatcher().await;
        let caller = crate::kj::KjCaller {
            context_id: None,
            ..test_caller()
        };
        crate::kj::test_helpers::give_character(&d, &caller);

        let result = d
            .dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("orphan")], &caller)
            .await;
        assert!(!result.is_ok(), "a parentless create must refuse");
        assert!(
            result.message().contains("--parent"),
            "the refusal must name --parent: {}",
            result.message()
        );
        assert!(
            d.kernel_db().lock().find_context_by_label("orphan").unwrap().is_none(),
            "a refused create must not commit a context"
        );
    }

    /// An empty `--parent ""` is treated as `.` (current) by
    /// `resolve_context_arg`, so it only refuses when the caller also has
    /// no current context to resolve to.
    #[tokio::test]
    async fn context_create_refuses_with_empty_parent_and_no_current_context() {
        let d = test_dispatcher().await;
        let caller = crate::kj::KjCaller {
            context_id: None,
            ..test_caller()
        };

        let result = d
            .dispatch(
                &[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("empty-parent"), s("--parent"), s("")],
                &caller,
            )
            .await;
        assert!(!result.is_ok(), "--parent \"\" with no current context must refuse");
        assert!(
            d.kernel_db().lock().find_context_by_label("empty-parent").unwrap().is_none(),
            "a refused create must not commit a context"
        );
    }

    #[tokio::test]
    async fn cancelled_context_create_reports_the_committed_context() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("cancelled-create-parent"), None, principal);
        let c = caller_with_context(parent);
        crate::kj::test_helpers::give_character(&d, &c);
        c.cancel.cancel();

        let result = d
            .dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("cancelled-create-child")], &c)
            .await;

        assert!(!result.is_ok(), "cancelled create must report partial completion");
        assert!(result.message().contains("was committed"), "{}", result.message());
        assert!(result.message().contains("lifecycle was cancelled"), "{}", result.message());
        assert!(
            d.kernel_db().lock().find_context_by_label("cancelled-create-child").unwrap().is_some(),
            "the context remains committed for diagnosis"
        );
    }

    #[tokio::test]
    async fn context_create_reports_rc_admission_write_failure_after_committing_context() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("rc-admission-fault-parent"), None, principal);
        let c = caller_with_context(parent);
        crate::kj::test_helpers::give_character(&d, &c);
        {
            let db = d.kernel_db().lock();
            db.conn_for_ledger().authorizer(Some(|auth: AuthContext<'_>| match auth.action {
                AuthAction::Insert { table_name: "rc_runs" } => Authorization::Deny,
                _ => Authorization::Allow,
            })).unwrap();
        }

        let result = d.dispatch(
            &[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("rc-admission-fault-child")], &c,
        ).await;
        d.kernel_db().lock().conn_for_ledger().authorizer(None::<fn(AuthContext<'_>) -> Authorization>).unwrap();

        assert!(!result.is_ok(), "rc admission fault must not report a clean create");
        assert!(result.message().contains("was committed"), "{}", result.message());
        assert!(result.message().contains("could not settle"), "{}", result.message());
        assert!(
            d.kernel_db().lock().find_context_by_label("rc-admission-fault-child").unwrap().is_some(),
            "the already committed context remains available for diagnosis and rebind"
        );
    }

    /// A context's character is fixed by its instance, so `kj context set`
    /// has no `--as`: the flag is unknown, and nothing changes.
    #[tokio::test]
    async fn context_set_has_no_as_flag() {
        let d = test_dispatcher().await;
        let coder = PrincipalId::new();
        d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
            principal_id: coder, name: "coder".into(), created_at: 0, retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
        }).unwrap();
        let context = crate::kj::test_helpers::register_rooted_context(&d, Some("review-work"), coder);
        let caller = crate::kj::test_helpers::confirmed_caller(context);
        let refused = d.dispatch(&[s("context"), s("set"), s("."), s("--as"), s("coder")], &caller).await;
        assert!(!refused.is_ok() && refused.message().contains("--as"), "{}", refused.message());
        assert!(refused.message().contains("unexpected argument"), "{}", refused.message());
        let help = d.dispatch(&[s("context"), s("set"), s("--help")], &caller).await;
        assert!(!help.message().contains("--as"), "{}", help.message());
        assert_eq!(d.kernel_db().lock().get_context(context).unwrap().unwrap().played_by, Some(coder));
    }

    /// Another live root, outside the target's lineage, has no authority
    /// over it: neither half of the rule names it.
    #[tokio::test]
    async fn a_root_outside_the_lineage_cannot_change_the_egress_list() {
        let d = test_dispatcher().await;
        let amy = test_reviewer_principal();
        let bob = PrincipalId::new();
        d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
            principal_id: bob, name: "bob".into(), created_at: 0, retired_at: None, handoff_ctx: None, root_ctx: None, root: true,
        }).unwrap();
        let context = crate::kj::test_helpers::register_rooted_context(&d, Some("routed"), amy);
        let caller = |actor| crate::kj::KjCaller {
            principal_id: actor, actor_id: actor, reviewer_id: None, context_id: Some(context),
            session_id: kaijutsu_types::SessionId::new(), confirmed: false, rc_depth: 0, privileged: true,
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        let allow = [s("context"), s("set"), s("."), s("--egress-allow"), s("crates.io")];
        let refused = d.dispatch(&allow, &caller(bob)).await;
        assert!(!refused.is_ok() && refused.message().contains("or its lineage root (amy) may change its egress list"), "{}", refused.message());
        assert!(d.kernel_db().lock().list_context_egress(context).unwrap().is_empty());
        let by_root = d.dispatch(&allow, &caller(amy)).await;
        assert!(by_root.is_ok(), "{}", by_root.message());
        assert_eq!(d.kernel_db().lock().list_context_egress(context).unwrap(), vec!["crates.io".to_string()]);
    }

    /// A root character is a model context: a senpai can cast it as the
    /// character of a kohai it creates, and the instance records it.
    #[tokio::test]
    async fn a_root_character_can_be_cast_as_a_performer() {
        let d = test_dispatcher().await;
        let banto = PrincipalId::new();
        let sovereign = PrincipalId::new();
        for (principal_id, name) in [(banto, "banto"), (sovereign, "sovereign")] {
            d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
                principal_id, name: name.into(), created_at: 0, retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
            }).unwrap();
        }
        d.kernel_db().lock().update_character_root(sovereign, true).unwrap();
        let context = crate::kj::test_helpers::register_rooted_context(&d, Some("banto-directed-2"), banto);
        let banto_caller = crate::kj::KjCaller {
            principal_id: banto, actor_id: banto, reviewer_id: None, context_id: Some(context),
            session_id: kaijutsu_types::SessionId::new(), confirmed: false, rc_depth: 0, privileged: true,
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        let refused = d.dispatch(&[s("kohai"), s("create"), s("--as"), s("sovereign"), s("sovereign-lane")], &banto_caller).await;
        assert!(refused.is_ok(), "a root can be cast: {}", refused.message());
        let lane = d.kernel_db().lock().find_context_by_label("sovereign-lane").unwrap()
            .expect("the kohai exists");
        assert_eq!(
            d.kernel_db().lock().context_character(lane.context_id).unwrap().unwrap().principal_id,
            sovereign,
            "the instance's character is the root"
        );
    }

    /// A root character does not perform below its own root: the climb from
    /// the new context reaches the root itself, and a performer never reviews
    /// its own work. A root's own context comes from `kj character create`,
    /// and a model plays the ROOT console above it.
    #[tokio::test]
    async fn a_root_character_cannot_perform_a_kohai_under_its_own_root() {
        let d = test_dispatcher().await;
        let amy = PrincipalId::new();
        d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
            principal_id: amy, name: "owner".into(), created_at: 0, retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
        }).unwrap();
        d.kernel_db().lock().update_character_root(amy, true).unwrap();
        let context = register_context(&d, Some("owner-root"), None, amy);
        d.kernel_db().lock().update_played_by(context, Some(amy)).unwrap();
        let caller = crate::kj::KjCaller {
            principal_id: amy, actor_id: amy, reviewer_id: None, context_id: Some(context),
            session_id: kaijutsu_types::SessionId::new(), confirmed: false, rc_depth: 0, privileged: true,
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        let refused = d.dispatch(&[s("kohai"), s("create"), s("--as"), s("owner"), s("owner-lane")], &caller).await;
        assert!(!refused.is_ok(), "a root would review its own work");
        assert!(refused.message().contains("cannot review its own work"), "{}", refused.message());
        assert!(d.kernel_db().lock().find_context_by_label("owner-lane").unwrap().is_none(), "nothing was written");
    }

    #[tokio::test]
    async fn kohai_create_help_describes_performer_and_senpai_selection() {
        let d = test_dispatcher().await;
        let result = d.dispatch(&[s("kohai"), s("create"), s("--help")], &test_caller()).await;
        assert!(result.is_ok(), "{}", result.message());
        let help = result.message();
        println!("{help}");
        assert!(help.contains("--as <CHARACTER>"));
        assert!(help.contains("Its senpai's character reviews its asks"), "{help}");
        assert!(help.contains("--senpai <SENPAI>") && help.contains("--parent <PARENT>"), "{help}");
        assert!(!help.contains("--top"), "{help}");
    }

    /// `kj context set --help` publishes the two egress flags with a plain
    /// description of what they take (`docs/writing.md`).
    #[tokio::test]
    async fn context_set_help_describes_egress_flags() {
        let d = test_dispatcher().await;
        let result = d.dispatch(&[s("context"), s("set"), s("--help")], &test_caller()).await;
        assert!(result.is_ok(), "{}", result.message());
        let help = result.message();
        println!("{help}");
        assert!(help.contains("--egress-allow <HOST>"), "{help}");
        assert!(help.contains("--egress-deny <HOST>"), "{help}");
        assert!(help.contains("loopback literal"), "{help}");
    }

    /// `--consent` is retired: the turn loop's only reader of consent mode
    /// (the iteration cap) is gone, and nothing else ever branched on it.
    /// `kj context set` must refuse the flag as unknown clap input, not
    /// silently accept and discard it.
    #[tokio::test]
    async fn context_set_rejects_the_retired_consent_flag() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("consent-gone"), None, principal);
        let c = caller_with_context(ctx);
        let result = d
            .dispatch(&[s("context"), s("set"), s("."), s("--consent"), s("autonomous")], &c)
            .await;
        assert!(!result.is_ok(), "{}", result.message());
        assert!(
            result.message().to_lowercase().contains("unexpected argument")
                || result.message().contains("--consent"),
            "expected an unknown-argument refusal, got: {}",
            result.message()
        );
    }

    /// `kj context info` shows the egress list — an explicit "none" when
    /// empty, then the granted hosts after `--egress-allow`. The caller
    /// must be the context's lineage root to grant it, so this runs as
    /// `amy`, `register_rooted_context`'s root character.
    #[tokio::test]
    async fn context_info_shows_the_egress_list() {
        let d = test_dispatcher().await;
        let amy_id = test_reviewer_principal();
        let context = crate::kj::test_helpers::register_rooted_context(&d, Some("egress-info"), amy_id);
        let caller = crate::kj::KjCaller {
            principal_id: amy_id, actor_id: amy_id, reviewer_id: None, context_id: Some(context),
            session_id: kaijutsu_types::SessionId::new(), confirmed: false, rc_depth: 0, privileged: true,
            cancel: tokio_util::sync::CancellationToken::new(),
        };

        let result = d.dispatch(&[s("context"), s("info")], &caller).await;
        assert!(result.is_ok(), "{}", result.message());
        assert!(result.message().contains("Egress:  none"), "{}", result.message());
        let KjResult::Ok { data: Some(data), .. } = &result else { panic!("context info has data") };
        assert_eq!(data["egress"], serde_json::json!([]));

        let set = d.dispatch(&[s("context"), s("set"), s("."), s("--egress-allow"), s("crates.io")], &caller).await;
        assert!(set.is_ok(), "{}", set.message());
        let result = d.dispatch(&[s("context"), s("info")], &caller).await;
        assert!(result.is_ok(), "{}", result.message());
        assert!(result.message().contains("Egress:\n  crates.io"), "{}", result.message());
        let KjResult::Ok { data: Some(data), .. } = &result else { panic!("context info has data") };
        assert_eq!(data["egress"], serde_json::json!(["crates.io"]));
    }

    /// amy (a root) plays her root context; banto plays a `default` seat
    /// created from it, configured in every way rotation carries.
    struct RotationFixture {
        d: std::sync::Arc<crate::kj::KjDispatcher>,
        amy: PrincipalId,
        banto: PrincipalId,
        coder: PrincipalId,
        root: ContextId,
        seat: ContextId,
    }

    async fn rotation_fixture() -> RotationFixture {
        rotation_fixture_persisting_bindings(true).await
    }

    /// `persist_bindings: false` leaves the broker's bindings in memory, so
    /// every successor reads back with no loadout.
    async fn rotation_fixture_persisting_bindings(persist_bindings: bool) -> RotationFixture {
        let d = std::sync::Arc::new(test_dispatcher_rc().await);
        d.set_self_arc();
        if persist_bindings {
            d.kernel().broker().set_db(d.kernel_db().clone()).await;
        }
        d.kernel().mount("/tmp/seat", crate::vfs::backends::MemoryBackend::new()).await;
        let [amy, banto, coder, judge] = [PrincipalId::new(), PrincipalId::new(), PrincipalId::new(), PrincipalId::new()];
        let db = d.kernel_db().lock();
        for (principal_id, name, root) in [(amy, "amy", true), (banto, "banto", false), (coder, "coder", false), (judge, "judge", false)] {
            db.insert_character(&crate::kernel_db::CharacterRow {
                principal_id, name: s(name), created_at: 1, retired_at: None, handoff_ctx: None, root_ctx: None, root,
            }).unwrap();
        }
        drop(db);
        let root = register_context(&d, Some("amy"), None, amy);
        let seat = register_context(&d, Some("banto"), Some(root), amy);
        let db = d.kernel_db().lock();
        db.update_played_by(root, Some(amy)).unwrap();
        db.seed_test_cast(banto, "mock", "mock-banto", &["default"]).unwrap();
        db.update_played_by(seat, Some(banto)).unwrap();
        db.update_settings(seat, Some("seat prompt")).unwrap();
        db.set_context_env(seat, "SEAT_VAR", "kept").unwrap();
        db.set_context_env(seat, "ROTATED_FROM", "an-older-seat").unwrap();
        db.add_context_egress(seat, "api.example.com").unwrap();
        db.upsert_context_shell(&crate::kernel_db::ContextShellRow { context_id: seat, cwd: Some(s("/tmp/seat")), updated_at: 1 }).unwrap();
        db.promote_context(seat).unwrap();
        drop(db);
        RotationFixture { d, amy, banto, coder, root, seat }
    }

    fn rotation_caller(context: ContextId, actor: PrincipalId) -> crate::kj::KjCaller {
        crate::kj::KjCaller {
            principal_id: actor, actor_id: actor, reviewer_id: None, context_id: Some(context),
            session_id: kaijutsu_types::SessionId::new(), confirmed: false, rc_depth: 0, privileged: true,
            cancel: tokio_util::sync::CancellationToken::new(),
        }
    }

    /// `kj context rotate` replaces a seat with a successor of the same
    /// character with slots copied fresh, carrying its whole configuration,
    /// takes its label and ring seat, names the predecessor in
    /// `ROTATED_FROM`, and archives it.
    async fn rotate_carries_the_seat_to_its_successor_body() {
        let f = rotation_fixture().await;
        let before = f.d.kernel_db().lock().get_context(f.seat).unwrap().unwrap();
        // The character's cast moves to another model after the seat was bound:
        // the running seat keeps its slots, the successor copies the new ones.
        f.d.kernel_db().lock().seed_test_cast(f.banto, "mock", "mock-banto-next", &["default"]).unwrap();
        let rotated = f.d.dispatch(&[s("context"), s("rotate")], &rotation_caller(f.seat, f.banto)).await;
        assert!(rotated.is_ok(), "{}", rotated.message());
        let KjResult::Ok { data: Some(data), .. } = &rotated else { panic!("rotate returns ids: {rotated:?}") };
        let successor = ContextId::parse(data["context_id"].as_str().unwrap()).unwrap();
        assert_eq!(data["rotated_from"], f.seat.to_hex());

        let db = f.d.kernel_db().lock();
        let old = db.get_context(f.seat).unwrap().unwrap();
        assert!(old.archived_at.is_some(), "the predecessor is archived");
        assert_eq!(old.promoted_at, None);
        let new = db.get_context(successor).unwrap().unwrap();
        assert_eq!(new.label.as_deref(), Some("banto"), "the label follows the live holder");
        assert_eq!(db.resolve_context("banto").unwrap(), successor);
        assert!(new.archived_at.is_none());
        assert!(new.promoted_at.is_some(), "the successor takes the ring seat");
        assert_eq!(new.forked_from, Some(f.root), "created from the predecessor's own parent");
        assert_eq!(new.context_type, before.context_type);
        assert_eq!(new.played_by, before.played_by);
        assert_eq!(db.senpai_of(successor).unwrap(), db.senpai_of(before.context_id).unwrap());
        assert_eq!(new.system_prompt.as_deref(), Some("seat prompt"));
        assert_eq!(new.workspace_id, before.workspace_id);
        let env: std::collections::HashMap<_, _> = db.get_context_env(successor).unwrap().into_iter().map(|e| (e.key, e.value)).collect();
        assert_eq!(env.get("SEAT_VAR").map(String::as_str), Some("kept"));
        assert_eq!(env.get("ROTATED_FROM"), Some(&f.seat.to_hex()), "names the predecessor it replaced, by id");
        assert_eq!(db.get_context_shell(successor).unwrap().and_then(|shell| shell.cwd).as_deref(), Some("/tmp/seat"));
        let model_of = |ctx| match crate::model_resolution::context_model(&db, ctx).unwrap() {
            crate::model_resolution::ContextModelLookup::Model(m) => m.model,
            other => panic!("expected a model, got {other:?}"),
        };
        assert_eq!(model_of(f.seat), "mock-banto", "the predecessor keeps the slots it was made with");
        assert_eq!(model_of(successor), "mock-banto-next", "the successor's slots are copied fresh from the character's cast");
        assert_eq!(
            db.context_character(successor).unwrap().unwrap().principal_id,
            db.context_character(f.seat).unwrap().unwrap().principal_id,
            "the successor is the same character"
        );
        assert_eq!(db.list_context_egress(successor).unwrap(), [s("api.example.com")], "egress hosts carry over");
        assert_eq!(db.get_character(f.amy).unwrap().unwrap().root_ctx, Some(f.root), "other consoles stay");
        drop(db);
        assert!(f.d.has_usable_loadout(successor).unwrap(), "the create lifecycle ran on the successor");
        let drift = f.d.drift_router().read();
        assert_eq!(drift.get(successor).and_then(|h| h.label.clone()).as_deref(), Some("banto"));
        assert_eq!(drift.get(f.seat).map(|h| h.state), Some(ContextState::Archived));
    }

    /// Only the performer rotating itself or the lineage root may rotate;
    /// a refusal creates nothing. An archived context cannot be rotated.
    async fn rotate_authority_and_liveness_body() {
        let f = rotation_fixture().await;
        let count = || f.d.kernel_db().lock().list_documents().unwrap().len();
        let before = count();
        let refused = f.d.dispatch(&[s("context"), s("rotate"), s("banto")], &rotation_caller(f.root, f.coder)).await;
        assert!(!refused.is_ok(), "coder neither plays the seat nor roots it");
        assert!(refused.message().contains("lineage root"), "{}", refused.message());
        assert_eq!(count(), before, "a refusal creates nothing");
        assert!(f.d.kernel_db().lock().get_context(f.seat).unwrap().unwrap().archived_at.is_none());

        let by_root = f.d.dispatch(&[s("context"), s("rotate"), s("banto")], &rotation_caller(f.root, f.amy)).await;
        assert!(by_root.is_ok(), "the lineage root may rotate: {}", by_root.message());

        // The rotation archived the old seat, so rotating it again refuses.
        let archived = f.d.dispatch(&[s("context"), s("rotate"), f.seat.to_hex()], &rotation_caller(f.root, f.amy)).await;
        assert!(!archived.is_ok() && archived.message().contains("archived"), "{}", archived.message());
    }

    /// Whether `ctx` has a recorded run of the `archive` rc lifecycle.
    fn archive_lifecycle_ran(d: &crate::kj::KjDispatcher, ctx: ContextId) -> bool {
        let db = d.kernel_db().lock();
        approval_ledger::rc_runs::list_runs(db.conn_for_ledger())
            .unwrap()
            .into_iter()
            .any(|r| r.context_id == ctx.as_bytes().to_vec() && r.verb == crate::rc::VERB_ARCHIVE)
    }

    /// Install an `archive` script for `ctx`'s context type.
    async fn install_archive_script(d: &crate::kj::KjDispatcher, ctx: ContextId, body: &str) {
        let context_type = d.kernel_db().lock().get_context(ctx).unwrap().unwrap().context_type;
        install_rc_script_file(d, &format!("/config/rc/{context_type}/archive/S10-test.kai"), body).await;
    }

    /// Rotation archives the predecessor through its `archive` lifecycle,
    /// so its scripts (a coder's judge shadows) run as they do for
    /// `kj context archive`.
    ///
    /// Falsified by archiving the predecessor inside the rotation
    /// transaction alone: no archive run is recorded for it.
    async fn rotate_runs_the_predecessor_s_archive_lifecycle_body() {
        let f = rotation_fixture().await;
        install_archive_script(&f.d, f.seat, "true").await;
        let rotated = f.d.dispatch(&[s("context"), s("rotate")], &rotation_caller(f.seat, f.banto)).await;
        assert!(rotated.is_ok(), "{}", rotated.message());
        assert!(archive_lifecycle_ran(&f.d, f.seat), "the predecessor's archive lifecycle ran");
        assert!(f.d.kernel_db().lock().get_context(f.seat).unwrap().unwrap().archived_at.is_some());
    }

    /// A root context rotates too: a parentless successor, and the root
    /// character's console is the successor.
    async fn rotate_a_root_context_body() {
        let f = rotation_fixture().await;
        let rotated = f.d.dispatch(&[s("context"), s("rotate"), s("amy")], &rotation_caller(f.root, f.amy)).await;
        assert!(rotated.is_ok(), "{}", rotated.message());
        let db = f.d.kernel_db().lock();
        let successor = db.get_character(f.amy).unwrap().unwrap().root_ctx.unwrap();
        assert_ne!(successor, f.root);
        let row = db.get_context(successor).unwrap().unwrap();
        assert_eq!((row.forked_from, row.label.as_deref(), row.played_by), (None, Some("amy"), Some(f.amy)));
        assert_eq!(db.lineage_root(f.seat).unwrap(), f.amy, "children of the archived root keep their lineage root");
    }

    /// A successor with no loadout does not take over: the predecessor keeps
    /// its label and seat and stays live, and the error names the successor.
    async fn rotate_without_a_successor_loadout_leaves_the_predecessor_live_body() {
        let f = rotation_fixture_persisting_bindings(false).await;
        let refused = f.d.dispatch(&[s("context"), s("rotate")], &rotation_caller(f.seat, f.banto)).await;
        assert!(!refused.is_ok() && refused.message().contains("no loadout"), "{}", refused.message());
        let db = f.d.kernel_db().lock();
        let seat = db.get_context(f.seat).unwrap().unwrap();
        assert!(seat.archived_at.is_none() && seat.promoted_at.is_some());
        assert_eq!(db.resolve_context("banto").unwrap(), f.seat);
    }

    #[tokio::test]
    async fn cancelled_rotation_retains_successor_without_replacing_predecessor() {
        let f = rotation_fixture().await;
        let caller = rotation_caller(f.seat, f.banto);
        caller.cancel.cancel();

        let result = f.d.dispatch(&[s("context"), s("rotate")], &caller).await;

        assert!(!result.is_ok(), "cancelled rotation must report partial completion");
        assert!(result.message().contains("owner cancelled"), "{}", result.message());
        assert!(result.message().contains("stays live"), "{}", result.message());
        let db = f.d.kernel_db().lock();
        let predecessor = db.get_context(f.seat).unwrap().unwrap();
        assert!(predecessor.archived_at.is_none(), "the predecessor remains live");
        assert_eq!(db.resolve_context("banto").unwrap(), f.seat, "the predecessor keeps its label");
        assert!(
            db.list_all_contexts().unwrap().iter().any(|row| {
                row.context_id != f.seat
                    && row.context_id != f.root
                    && row.forked_from == Some(f.root)
                    && row.archived_at.is_none()
            }),
            "the committed successor remains available for diagnosis"
        );
    }

    #[tokio::test]
    async fn rc_admission_fault_rotation_leaves_predecessor_live() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

        let f = rotation_fixture().await;
        let caller = rotation_caller(f.seat, f.banto);
        f.d.kernel_db().lock().conn_for_ledger().authorizer(Some(|auth: AuthContext<'_>| match auth.action {
            AuthAction::Insert { table_name: "rc_runs" } => Authorization::Deny,
            _ => Authorization::Allow,
        })).unwrap();
        let result = f.d.dispatch(&[s("context"), s("rotate")], &caller).await;
        f.d.kernel_db().lock().conn_for_ledger().authorizer(None::<fn(AuthContext<'_>) -> Authorization>).unwrap();

        assert!(!result.is_ok(), "rc admission fault must report a partial rotation");
        assert!(result.message().contains("stays live"), "{}", result.message());
        let db = f.d.kernel_db().lock();
        assert!(db.get_context(f.seat).unwrap().unwrap().archived_at.is_none());
        assert_eq!(db.resolve_context("banto").unwrap(), f.seat);
    }

    /// A character's handoff log has no context character. Rotating it points
    /// `characters.handoff_ctx` at the successor and checks no loadout; its
    /// character or a live root may do it, nobody else; any other context
    /// with no context character refuses. A note to an archived log refuses.
    async fn rotating_a_handoff_log_repoints_the_pointer_body() {
        let f = rotation_fixture().await;
        let noted = f.d.dispatch(&[s("handoff"), s("note"), s("first note")], &rotation_caller(f.root, f.banto)).await;
        assert!(noted.is_ok(), "{}", noted.message());
        let log = f.d.kernel_db().lock().get_character(f.banto).unwrap().unwrap().handoff_ctx.expect("the note minted the log");
        assert_eq!(f.d.kernel_db().lock().context_character(log).unwrap(), None, "a handoff log has no context character");

        let stranger = f.d.dispatch(&[s("context"), s("rotate"), log.to_hex()], &rotation_caller(f.root, f.coder)).await;
        assert!(!stranger.is_ok() && stranger.message().contains("handoff log"), "{}", stranger.message());
        let bare = register_context(&f.d, Some("bare-no-context-character"), None, f.amy);
        let refused = f.d.dispatch(&[s("context"), s("rotate"), bare.to_hex()], &rotation_caller(f.root, f.banto)).await;
        assert!(!refused.is_ok(), "{}", refused.message());
        assert!(refused.message().contains("has no context character") && refused.message().contains("handoff log"), "{}", refused.message());
        assert!(f.d.kernel_db().lock().get_context(bare).unwrap().unwrap().archived_at.is_none());

        let rotated = f.d.dispatch(&[s("context"), s("rotate"), log.to_hex()], &rotation_caller(f.root, f.banto)).await;
        assert!(rotated.is_ok(), "{}", rotated.message());
        let KjResult::Ok { data: Some(data), .. } = &rotated else { panic!("rotate returns ids: {rotated:?}") };
        let successor = ContextId::parse(data["context_id"].as_str().unwrap()).unwrap();
        {
            let db = f.d.kernel_db().lock();
            assert_eq!(db.get_character(f.banto).unwrap().unwrap().handoff_ctx, Some(successor), "the pointer moves to the successor");
            assert!(db.get_context(log).unwrap().unwrap().archived_at.is_some());
            assert_eq!(db.context_character(successor).unwrap(), None, "the successor has no context character either");
        }

        // A note to a log that is archived refuses.
        f.d.kernel_db().lock().archive_context(successor).unwrap();
        let late = f.d.dispatch(&[s("handoff"), s("note"), s("too late")], &rotation_caller(f.root, f.banto)).await;
        assert!(!late.is_ok() && late.message().contains("archived"), "{}", late.message());
    }

    #[test]
    fn rotating_a_handoff_log_repoints_the_pointer() {
        crate::on_rc_thread(rotating_a_handoff_log_repoints_the_pointer_body);
    }

    #[test]
    fn rotate_without_a_successor_loadout_leaves_the_predecessor_live() {
        crate::on_rc_thread(rotate_without_a_successor_loadout_leaves_the_predecessor_live_body);
    }

    #[test]
    fn rotate_carries_the_seat_to_its_successor() {
        crate::on_rc_thread(rotate_carries_the_seat_to_its_successor_body);
    }

    #[test]
    fn rotate_authority_and_liveness() {
        crate::on_rc_thread(rotate_authority_and_liveness_body);
    }

    /// The successor keeps the predecessor's senpai. The kernel moves no
    /// kohai: they keep answering to the archived predecessor, whose
    /// character still reviews their asks, until the successor takes them
    /// with `kj senpai set`. An archived kohai is not listed.
    async fn rotate_keeps_the_senpai_and_moves_no_kohai_body() {
        let f = rotation_fixture().await;
        let lane = register_context(&f.d, Some("seat-lane"), Some(f.seat), f.amy);
        let old_lane = register_context(&f.d, Some("seat-old-lane"), Some(f.seat), f.amy);
        {
            let db = f.d.kernel_db().lock();
            db.update_played_by(lane, Some(f.coder)).unwrap();
            db.update_played_by(old_lane, Some(f.coder)).unwrap();
            db.archive_context(old_lane).unwrap();
        }
        let senpai = |ctx: ContextId| f.d.kernel_db().lock().senpai_of(ctx).unwrap();
        let seat_senpai = senpai(f.seat);
        assert_eq!(senpai(lane), Some(f.seat));

        let rotated = f.d.dispatch(&[s("context"), s("rotate")], &rotation_caller(f.seat, f.banto)).await;
        assert!(rotated.is_ok(), "{}", rotated.message());
        let KjResult::Ok { data: Some(data), .. } = &rotated else { panic!("rotate returns ids: {rotated:?}") };
        let successor = ContextId::parse(data["context_id"].as_str().unwrap()).unwrap();

        assert_eq!(senpai(successor), seat_senpai, "the successor answers to the predecessor's senpai");
        assert_eq!(senpai(lane), Some(f.seat), "the kernel moves no kohai");
        assert_eq!(senpai(old_lane), Some(f.seat));
        let db = f.d.kernel_db().lock();
        assert!(db.list_kohai(successor).unwrap().is_empty(), "the successor has no kohai until it takes them");
        assert_eq!(
            db.list_kohai(f.seat).unwrap().into_iter().map(|row| row.context_id).collect::<Vec<_>>(),
            vec![lane],
            "the predecessor's kohai are the live ones"
        );
        assert_eq!(
            db.route_review(crate::kernel_db::ReviewQuery::ask(lane, f.coder)).unwrap().reviewer(),
            f.banto,
            "and their asks reach the same character through the archived predecessor"
        );
        drop(db);
        f.d.kernel_db().lock().set_context_senpai(lane, Some(successor)).unwrap();
        assert_eq!(
            f.d.kernel_db().lock().list_kohai(successor).unwrap().into_iter().map(|row| row.context_id).collect::<Vec<_>>(),
            vec![lane],
            "the successor takes a kohai with `kj senpai set`"
        );
    }

    /// A rotated root console has no senpai and becomes the root character's
    /// console at the commit; the seats under the old console stay with it.
    async fn rotate_a_root_moves_its_console_and_no_kohai_body() {
        let f = rotation_fixture().await;
        let rotated = f.d.dispatch(&[s("context"), s("rotate"), s("amy")], &rotation_caller(f.root, f.amy)).await;
        assert!(rotated.is_ok(), "{}", rotated.message());
        let db = f.d.kernel_db().lock();
        let successor = db.get_character(f.amy).unwrap().unwrap().root_ctx.unwrap();
        assert_ne!(successor, f.root);
        assert_eq!(db.root_console(f.amy).unwrap(), Some(successor));
        assert_eq!(db.senpai_of(successor).unwrap(), None);
        assert!(db.list_kohai(successor).unwrap().is_empty(), "the kernel moves no kohai");
        assert_eq!(
            db.list_kohai(f.root).unwrap().into_iter().map(|row| row.context_id).collect::<Vec<_>>(),
            vec![f.seat],
            "the seat still answers to the archived console"
        );
        assert_eq!(db.lineage_root(f.seat).unwrap(), f.amy);
    }

    #[test]
    fn rotate_keeps_the_senpai_and_moves_no_kohai() {
        crate::on_rc_thread(rotate_keeps_the_senpai_and_moves_no_kohai_body);
    }

    #[test]
    fn rotate_a_root_moves_its_console_and_no_kohai() {
        crate::on_rc_thread(rotate_a_root_moves_its_console_and_no_kohai_body);
    }

    #[test]
    fn rotate_a_root_context() {
        crate::on_rc_thread(rotate_a_root_context_body);
    }

    #[test]
    fn rotate_runs_the_predecessor_s_archive_lifecycle() {
        crate::on_rc_thread(rotate_runs_the_predecessor_s_archive_lifecycle_body);
    }

    // Creating through a shell re-enters kaish for rc; use the production rc stack.
    #[test]
    fn context_create_as_records_performer_and_loads_director_handoff() {
        crate::spawn_kaish_thread("rc-test-thread", || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(context_create_as_director_body());
        })
        .unwrap()
        .join()
        .unwrap();
    }

    async fn context_create_as_director_body() {
        let d = std::sync::Arc::new(test_dispatcher_rc().await);
        d.set_self_arc();
        let mut caller = test_caller();
        caller.context_id = None;
        let actor = PrincipalId::new();
        d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
            principal_id: actor,
            name: s("banto"),
            created_at: 1,
            retired_at: None,
            handoff_ctx: None, root_ctx: None, root: false,
        }).unwrap();
        d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
            principal_id: caller.principal_id, name: s("amy"), created_at: 1,
            retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
        }).unwrap();
        let parent = register_context(&d, Some("parent-operator"), None, caller.principal_id);
        d.kernel_db().lock().update_played_by(parent, Some(caller.principal_id)).unwrap();
        caller.context_id = Some(parent);
        let note = d.dispatch(
            &[s("handoff"), s("note"), s("--for"), s("banto"), s("remember-the-operator-task")],
            &caller,
        ).await;
        assert!(note.is_ok(), "{}", note.message());
        let shell = crate::runtime::embedded_kaish::EmbeddedKaish::for_context(
            &d,
            "create-operator",
            crate::runtime::context_shell::ShellIdentity {
                requester: caller.principal_id, performer: caller.principal_id, reviewer: None,
                context: parent, session: caller.session_id,
            },
            crate::runtime::context_shell::ShellPolicy::Rc(crate::rc::RcAuthority::for_test()), crate::runtime::context_shell::ShellCwd::Context,
            None,
            std::sync::Arc::new(crate::runtime::synthesis::NoopBlockSource),
        ).await.unwrap();
        let result = shell.execute_with_options(
            "kj kohai create operator --type director --as banto --env 'KJ_CHARACTER=obsolete-name'",
            kaish_kernel::ExecuteOptions::default(),
        ).await.unwrap();
        assert!(result.ok(), "{result:?}");
        let context = {
            let db = d.kernel_db().lock();
            let id = db.resolve_context("operator").unwrap();
            let row = db.get_context(id).unwrap().unwrap();
            assert_eq!(row.created_by, caller.principal_id);
            assert_eq!(row.played_by, Some(actor));
            assert_eq!(row.context_type, "director");
            id
        };
        let info = d.dispatch(&[s("context"), s("info"), s("operator")], &caller).await;
        let KjResult::Ok { data: Some(data), .. } = info else { panic!("context info data: {info:?}") };
        assert_eq!(data["played_by_name"], "banto");
        let blocks = d.block_store().block_snapshots(context).unwrap();
        let instructions = crate::llm::extract_system_prompt_sections(&blocks).join("\n");
        assert!(instructions.contains("we are banto, the operator"), "{instructions}");
        assert!(!instructions.contains("obsolete-name"));
        assert!(!instructions.contains("remember-the-operator-task"));
        assert!(blocks.iter().any(|b| b.kind == kaijutsu_types::BlockKind::Notification
            && b.content.contains("remember-the-operator-task")), "handoff must be a notification");
        assert!(blocks.iter().filter(|b| b.role == kaijutsu_types::Role::System)
            .all(|b| b.id.principal_id == caller.principal_id), "rc authorship stays with requester");

        let mut confirmed = caller.clone();
        confirmed.confirmed = true;
        let retired = d.dispatch(&[s("character"), s("retire"), s("banto")], &confirmed).await;
        assert!(retired.is_ok(), "{}", retired.message());
        assert!(d.kernel_db().lock().get_context(context).unwrap().unwrap().archived_at.is_some());
    }

    // Creating `--as` a character loads director handoff advice through rc,
    // which re-enters kaish many levels deep (see `context_create_as_records_
    // performer_and_loads_director_handoff` above); run it on the production
    // rc stack rather than the default 2 MiB test-thread stack.
    #[test]
    fn context_create_as_preserves_character_names_in_rc_and_handoff_advice() {
        crate::spawn_kaish_thread("rc-test-thread", || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(context_create_as_preserves_character_names_in_rc_and_handoff_advice_body());
        })
        .unwrap()
        .join()
        .unwrap();
    }

    async fn context_create_as_preserves_character_names_in_rc_and_handoff_advice_body() {
        let d = std::sync::Arc::new(test_dispatcher_rc().await);
        d.set_self_arc();
        let mut caller = test_caller();
        d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
            principal_id: caller.principal_id, name: s("amy"), created_at: 1,
            retired_at: None, handoff_ctx: None, root_ctx: None, root: true,
        }).unwrap();
        let root = register_context(&d, Some("amy"), None, caller.principal_id);
        d.kernel_db().lock().assign_test_review(root, Some(caller.principal_id), None).unwrap();
        caller.context_id = Some(root);
        for (index, name) in ["two words", "O'Brien", "null", "$literal", "1.0"].into_iter().enumerate() {
            let actor = PrincipalId::new();
            d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
                principal_id: actor, name: s(name), created_at: 1,
                retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
            }).unwrap();
            let note = d.dispatch(&[s("handoff"), s("note"), s("--for"), s(name), s("name-specific-history")], &caller).await;
            assert!(note.is_ok(), "{}", note.message());
            let label = format!("named-{index}");
            let create = d.dispatch(&[s("kohai"), s("create"), s(&label), s("--type"), s("director"), s("--as"), s(name)], &caller).await;
            assert!(create.is_ok(), "{}", create.message());
            let id = d.kernel_db().lock().resolve_context(&label).unwrap();
            let blocks = d.block_store().block_snapshots(id).unwrap();
            let instructions = crate::llm::extract_system_prompt_sections(&blocks).join("\n");
            assert!(instructions.contains(&format!("we are {name},")), "name={name}: {instructions}");
            let handoff = blocks.iter().find(|b| b.kind == kaijutsu_types::BlockKind::Notification
                && b.content.contains("name-specific-history")).expect("performer's handoff");
            let advice = handoff.content.lines().find(|line| line.trim_start().starts_with("kj handoff note"))
                .expect("handoff command").trim();
            let shell = crate::runtime::embedded_kaish::EmbeddedKaish::for_context(
                &d,
                "handoff-advice",
                crate::runtime::context_shell::ShellIdentity {
                    requester: caller.principal_id, performer: caller.principal_id, reviewer: None,
                    context: id, session: caller.session_id,
                },
                crate::runtime::context_shell::ShellPolicy::Rc(crate::rc::RcAuthority::for_test()), crate::runtime::context_shell::ShellCwd::Context,
                None,
                std::sync::Arc::new(crate::runtime::synthesis::NoopBlockSource),
            ).await.unwrap();
            let result = shell.execute_with_options(advice, kaish_kernel::ExecuteOptions::default()).await.unwrap();
            assert!(result.ok(), "name={name}, advice={advice}: {result:?}");
            let tail = d.dispatch(&[s("handoff"), s("tail"), s(name)], &caller).await;
            assert!(tail.is_ok(), "{}", tail.message());
            assert!(tail.message().contains("what you did, what is next"), "name={name}, advice={advice}, result={result:?}, tail={}", tail.message());
        }
    }

    /// `create --as` needs the Operator capability, as other changes to
    /// an existing context do.
    #[tokio::test]
    async fn kohai_create_requires_operator_with_or_without_as() {
        let d = test_dispatcher().await;
        let amy = test_reviewer_principal();
        let coder = PrincipalId::new();
        d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
            principal_id: coder, name: s("coder"), created_at: 1, retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
        }).unwrap();
        crate::kj::test_helpers::register_rooted_context(&d, Some("cast-parent"), amy);
        // A caller context with no binding row holds no capability.
        let bare = ContextId::new();
        {
            let db = d.kernel_db().lock();
            let ws = db.get_or_create_default_workspace(amy).unwrap();
            db.insert_context_with_document(&crate::kernel_db::ContextRow {
                context_id: bare, label: Some(s("bare-caller")), system_prompt: None,
                context_state: ContextState::Live, context_type: s("default"), created_at: 1, created_by: amy,
                forked_from: None, fork_kind: None, archived_at: None, workspace_id: None, preset_id: None,
                concluded_at: None, last_activity_at: None, promoted_at: None, demoted_at: None, paused_at: None,
                origin_host: None, played_by: None,
            }, ws).unwrap();
        }
        let caller = caller_with_context(bare).with_actor(amy, None);
        let refused = d.dispatch(&[s("kohai"), s("create"), s("cast-lane"), s("--parent"), s("cast-parent"), s("--as"), s("coder")], &caller).await;
        assert!(!refused.is_ok(), "{}", refused.message());
        assert!(refused.message().contains("operator"), "{}", refused.message());
        assert!(d.kernel_db().lock().resolve_context("cast-lane").is_err(), "a refusal creates nothing");
        let plain = d.dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("plain-lane"), s("--parent"), s("cast-parent")], &caller).await;
        assert!(!plain.is_ok(), "creating needs operator with no performer too (`docs/senpai-kohai.md`)");
        assert!(plain.message().contains("operator"), "{}", plain.message());
        assert!(d.kernel_db().lock().resolve_context("plain-lane").is_err());
    }

    /// Casting makes the cast character accountable to the caller, who
    /// directs the new context, so only a live character may cast.
    #[tokio::test]
    async fn context_create_as_requires_a_live_character_caller() {
        let d = test_dispatcher().await;
        let amy = test_reviewer_principal();
        let coder = PrincipalId::new();
        let ghost = PrincipalId::new();
        d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
            principal_id: coder, name: s("coder"), created_at: 1, retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
        }).unwrap();
        let parent = crate::kj::test_helpers::register_rooted_context(&d, Some("cast-parent"), amy);
        // A senpai must be held by someone; amy performs the parent.
        d.kernel_db().lock().update_played_by(parent, Some(amy)).unwrap();
        let mut caller = test_caller();
        caller.context_id = Some(parent);
        let characterless = d.dispatch(&[s("kohai"), s("create"), s("ghost-lane"), s("--as"), s("coder")], &caller).await;
        assert!(!characterless.is_ok(), "{}", characterless.message());
        assert!(characterless.message().contains("character sheet"), "{}", characterless.message());

        d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
            principal_id: ghost, name: s("ghost"), created_at: 1, retired_at: Some(2), handoff_ctx: None, root_ctx: None, root: false,
        }).unwrap();
        let retired = d.dispatch(&[s("kohai"), s("create"), s("ghost-lane"), s("--as"), s("coder")], &caller.clone().with_actor(ghost, None)).await;
        assert!(!retired.is_ok() && retired.message().contains("retired"), "{}", retired.message());
        assert!(d.kernel_db().lock().resolve_context("ghost-lane").is_err(), "a refusal creates nothing");

        let cast = d.dispatch(&[s("kohai"), s("create"), s("amy-lane"), s("--as"), s("coder")], &caller.with_actor(amy, None)).await;
        assert!(cast.is_ok(), "a live character directs what it casts: {}", cast.message());
    }

    #[tokio::test]
    async fn context_create_as_refuses_unknown_or_retired_without_mutation() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("as-refuses-parent"), None, principal);
        let caller = caller_with_context(parent);
        crate::kj::test_helpers::give_character(&d, &caller);
        d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
            principal_id: PrincipalId::new(), name: s("retired"), created_at: 1,
            retired_at: Some(2), handoff_ctx: None, root_ctx: None, root: false,
        }).unwrap();
        let before = d.kernel_db().lock().list_documents().unwrap().len();
        for (name, expected) in [("missing", "no character named"), ("retired", "is retired")] {
            let result = d.dispatch(
                &[s("kohai"), s("create"), s("refused"), s("--as"), s(name)], &caller,
            ).await;
            assert!(!result.is_ok());
            assert!(result.message().contains(expected), "{}", result.message());
            assert_eq!(d.kernel_db().lock().list_documents().unwrap().len(), before);
            assert!(d.kernel_db().lock().resolve_context("refused").is_err());
        }
    }

    #[tokio::test]
    async fn final_context_insert_rejects_a_performer_retired_after_validation() {
        let d = test_dispatcher().await;
        let amy = test_reviewer_principal();
        let performer = PrincipalId::new();
        {
            let db = d.kernel_db().lock();
            db.insert_character(&crate::kernel_db::CharacterRow {
                principal_id: performer,
                name: s("retired-between-checks"),
                created_at: 1,
                retired_at: Some(2),
                handoff_ctx: None, root_ctx: None, root: false,
            }).unwrap();
        }
        let context_id = ContextId::new();
        let row = crate::kernel_db::ContextRow {
            context_id,
            label: Some(s("must-not-insert")),
            system_prompt: None,
            context_state: ContextState::Live,
            context_type: s("default"),
            created_at: 1,
            created_by: amy,
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
            played_by: None,
        };
        let error = insert_new_context_checked(&d.kernel_db().lock(), &row, CharacterPlacement::Start { performer, senpai: None }).unwrap_err();
        assert!(error.to_string().contains("performer must name a live character"), "{error}");
        let db = d.kernel_db().lock();
        assert!(db.get_context(context_id).unwrap().is_none());
        assert!(!db.list_documents().unwrap().iter().any(|doc| doc.document_id == context_id));
    }

    #[tokio::test]
    async fn final_context_insert_rejects_a_performer_nobody_can_review() {
        let d = test_dispatcher().await;
        let amy = test_reviewer_principal();
        let performer = PrincipalId::new();
        {
            let db = d.kernel_db().lock();
            db.insert_character(&crate::kernel_db::CharacterRow {
                principal_id: performer,
                name: s("reviewer-changed-after-validation"),
                created_at: 1,
                retired_at: None,
                handoff_ctx: None, root_ctx: None, root: false,
            }).unwrap();
        }
        let context_id = ContextId::new();
        let row = crate::kernel_db::ContextRow {
            context_id,
            label: Some(s("must-not-self-review")),
            system_prompt: None,
            context_state: ContextState::Live,
            context_type: s("default"),
            created_at: 1,
            created_by: amy,
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
            played_by: None,
        };
        let top = register_context(&d, Some("held-by-performer"), None, amy);
        d.kernel_db().lock().assign_stand_in_character(top, performer).unwrap();
        let error = insert_new_context_checked(&d.kernel_db().lock(), &row, CharacterPlacement::Start { performer, senpai: Some(top) }).unwrap_err();
        assert!(error.to_string().contains("nobody reviews") && error.to_string().contains("reviewer-changed-after-validation"), "{error}");
        let db = d.kernel_db().lock();
        assert!(db.get_context(context_id).unwrap().is_none());
        assert!(!db.list_documents().unwrap().iter().any(|doc| doc.document_id == context_id));
    }

    /// Archiving a root console would leave `root_ctx` on an archived
    /// context; rotation is the only way out of one.
    #[tokio::test]
    async fn a_root_console_is_not_archived() {
        let d = test_dispatcher().await;
        let root = register_root_context(&d);
        let amy = d.kernel_db().lock().context_character(root).unwrap().unwrap().principal_id;
        let caller = crate::kj::KjCaller { principal_id: amy, actor_id: amy, context_id: Some(root), confirmed: true, ..test_caller() };
        let refused = d.dispatch(&[s("context"), s("archive"), root.to_hex()], &caller).await;
        assert!(!refused.is_ok(), "a root console was archived");
        assert!(refused.message().contains("root console") && refused.message().contains("kj context rotate"), "{}", refused.message());
        assert!(!d.kernel_db().lock().get_context(root).unwrap().unwrap().is_archived());
    }

    /// A context with no senpai is its character's root console, which
    /// `kj character create` makes. A kohai start with no senpai refuses, and
    /// so does clearing the senpai of a context that is not a root console.
    #[tokio::test]
    async fn only_a_root_console_has_no_senpai() {
        let d = test_dispatcher().await;
        let performer = PrincipalId::new();
        d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
            principal_id: performer, name: s("topless"), created_at: 1, retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
        }).unwrap();
        let root = register_root_context(&d);
        let lane = register_context(&d, Some("topless-lane"), Some(root), test_reviewer_principal());
        d.kernel_db().lock().update_played_by(lane, Some(performer)).unwrap();
        d.kernel_db().lock().set_context_senpai(lane, Some(root)).unwrap();
        let error = d.kernel_db().lock().set_context_senpai(lane, None).unwrap_err();
        assert!(error.to_string().contains("root console"), "{error}");
        assert_eq!(d.kernel_db().lock().senpai_of(lane).unwrap(), Some(root), "nothing was written");

        let context_id = ContextId::new();
        let row = crate::kernel_db::ContextRow {
            context_id, label: Some(s("topless-start")), system_prompt: None, context_state: ContextState::Live,
            context_type: s("default"), created_at: 1, created_by: test_reviewer_principal(), forked_from: None,
            fork_kind: None, archived_at: None, workspace_id: None, preset_id: None, concluded_at: None,
            last_activity_at: None, promoted_at: None, demoted_at: None, paused_at: None, origin_host: None, played_by: None,
        };
        let error = insert_new_context_checked(&d.kernel_db().lock(), &row, CharacterPlacement::Start { performer, senpai: None }).unwrap_err();
        assert!(error.to_string().contains("root console"), "{error}");
        assert!(d.kernel_db().lock().get_context(context_id).unwrap().is_none(), "nothing was inserted");
    }

    /// The default senpai, the caller's own context, follows the placing
    /// rule as `--senpai` does: a character that neither plays that context,
    /// holds its senpai, nor is its lineage root cannot start a kohai under it.
    #[tokio::test]
    async fn kohai_create_under_a_context_the_caller_cannot_place_under_is_refused() {
        let d = test_dispatcher().await;
        let [owner, visitor, performer] = [PrincipalId::new(), PrincipalId::new(), PrincipalId::new()];
        for (principal_id, name) in [(owner, "owner-x"), (visitor, "visitor-x"), (performer, "performer-x")] {
            d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
                principal_id, name: name.into(), created_at: 1, retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
            }).unwrap();
        }
        d.kernel_db().lock().seed_test_cast_roles(performer, &[("default", "mock", "mock-model")]).unwrap();
        let root = register_root_context(&d);
        let seat = register_context(&d, Some("owner-seat"), Some(root), test_reviewer_principal());
        d.kernel_db().lock().update_played_by(seat, Some(owner)).unwrap();
        d.kernel_db().lock().set_context_senpai(seat, Some(root)).unwrap();
        let visiting = crate::kj::KjCaller { principal_id: visitor, actor_id: visitor, context_id: Some(seat), privileged: true, ..test_caller() };
        let refused = d.dispatch(&[s("kohai"), s("create"), s("visited"), s("--as"), s("performer-x")], &visiting).await;
        assert!(!refused.is_ok(), "a visitor placed a kohai under the owner's context");
        assert!(d.kernel_db().lock().find_context_by_label("visited").unwrap().is_none(), "nothing was written");
    }

    /// A context's character is the instance's character, so `--as` is
    /// required: clap refuses its absence before the kernel looks at anything.
    #[tokio::test]
    async fn kohai_create_without_as_is_refused_by_clap() {
        let d = test_dispatcher().await;
        let mut caller = test_caller();
        let parent = register_context(&d, Some("unset-performer-parent"), None, caller.principal_id);
        caller.context_id = Some(parent);
        d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
            principal_id: caller.principal_id, name: s("requester"), created_at: 1,
            retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
        }).unwrap();
        let result = d.dispatch(&[s("kohai"), s("create"), s("unassigned")], &caller).await;
        let KjResult::Err(message) = result else { panic!("expected a refusal, got {result:?}") };
        assert!(message.contains("--as") && message.contains("required"), "{message}");
        assert!(d.kernel_db().lock().find_context_by_label("unassigned").unwrap().is_none(), "nothing was created");
    }

    /// `--type` with no matching rc bucket is refused before any row lands
    /// — `test_dispatcher()`'s rc tree is the full embedded seed, so
    /// "codr" (a typo of "coder") has no bucket to run.
    #[tokio::test]
    async fn context_create_refuses_an_unknown_type() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);

        let c = caller_with_context(parent);

        crate::kj::test_helpers::give_character(&d, &c);
        let result = d
            .dispatch(
                &[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("--type"), s("codr"), s("x")],
                &c,
            )
            .await;
        assert!(!result.is_ok(), "expected error, got: {}", result.message());
        assert!(
            result.message().contains("codr"),
            "error should name the rejected type: {}",
            result.message()
        );

        // No orphan row left behind.
        let db = d.kernel_db().lock();
        let contexts = db.list_active_contexts().unwrap();
        assert!(
            !contexts.iter().any(|r| r.label.as_deref() == Some("x")),
            "a refused create must not leave a row"
        );
    }

    /// `gate` is a cast role, not a context type: creating or setting a
    /// context of that type is refused with a message that says so, and no
    /// row changes.
    ///
    /// Falsified by accepting `gate` as an ordinary type, or by refusing it
    /// only as an unknown rc bucket.
    #[tokio::test]
    async fn context_create_and_set_refuse_the_reserved_gate_role() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);
        let c = caller_with_context(parent);
        crate::kj::test_helpers::give_character(&d, &c);

        let created = d.dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("--type"), s("gate"), s("g")], &c).await;
        assert!(!created.is_ok(), "expected error, got: {}", created.message());
        assert!(created.message().contains("is a cast role, not a context type"), "{}", created.message());
        assert!(
            !d.kernel_db().lock().list_active_contexts().unwrap().iter().any(|r| r.label.as_deref() == Some("g")),
            "a refused create must not leave a row"
        );

        let set = d.dispatch(&[s("context"), s("set"), s("."), s("--type"), s("gate")], &c).await;
        assert!(!set.is_ok(), "expected error, got: {}", set.message());
        assert!(set.message().contains("is a cast role, not a context type"), "{}", set.message());
        let row = d.kernel_db().lock().get_context(parent).unwrap().unwrap();
        assert_ne!(row.context_type, "gate", "a refused set changes nothing");
    }

    #[tokio::test]
    async fn context_create_duplicate_label() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);

        let c = caller_with_context(parent);

        crate::kj::test_helpers::give_character(&d, &c);
        // First create succeeds
        let r1 = d.dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("dup")], &c).await;
        assert!(r1.is_ok());

        // Second create with same label should fail
        let r2 = d.dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("dup")], &c).await;
        assert!(!r2.is_ok(), "expected error, got: {}", r2.message());
    }

    #[tokio::test]
    async fn context_help() {
        let d = test_dispatcher().await;
        let c = test_caller();
        // `--help` routes through clap's DisplayHelp (the bare `help` word is no
        // longer special). Assert on the clap-rendered usage + a known verb.
        let result = d.dispatch(&[s("context"), s("--help")], &c).await;
        assert!(result.is_ok());
        assert!(
            result.message().contains("Usage")
                && result.message().contains("switch")
                && result.message().contains("promote")
                && result.message().contains("demote")
                && result.message().contains("pause")
                && result.message().contains("resume"),
            "clap help should list usage + verbs (incl. the ring-placement ones): {}",
            result.message()
        );
    }

    /// `--env` validates its key at write time, after `--system-prompt` has
    /// already been written. One failed field must leave the row as it was.
    ///
    /// Falsified by applying the fields outside one transaction.
    #[tokio::test]
    async fn context_set_partial_failure_commits_nothing() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("atomic"), None, principal);
        let before = d.kernel_db().lock().get_context(ctx).unwrap().unwrap();
        assert!(before.system_prompt.is_none(), "fixture starts without a system prompt");

        let c = caller_with_context(ctx);
        let result = d
            .dispatch(
                &[
                    s("context"),
                    s("set"),
                    s("."),
                    s("--system-prompt"),
                    s("changed"),
                    s("--env"),
                    s("1BAD=y"),
                ],
                &c,
            )
            .await;
        assert!(!result.is_ok(), "an invalid env key must fail the whole set");

        let after = d.kernel_db().lock().get_context(ctx).unwrap().unwrap();
        assert_eq!(
            after.system_prompt, before.system_prompt,
            "the system prompt must not be committed when a later field fails"
        );
        assert!(
            d.kernel_db().lock().get_context_env(ctx).unwrap().is_empty(),
            "no env row either"
        );
    }
    #[tokio::test]
    async fn context_hydrate_sets_marks_tail_and_clears() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("hydra"), None, principal);
        // Seed a block so the default prefix marker (current tail) resolves.
        d.block_store()
            .create_document(ctx, crate::DocumentKind::Conversation, None)
            .unwrap();
        let tail = d
            .block_store()
            .insert_block_as(
                ctx,
                None,
                None,
                kaijutsu_types::Role::User,
                kaijutsu_types::BlockKind::Text,
                "seed".to_string(),
                kaijutsu_types::Status::Done,
                kaijutsu_types::ContentType::Plain,
                Some(principal),
            )
            .unwrap();
        let c = caller_with_context(ctx);

        let r = d
            .dispatch(&[s("context"), s("hydrate"), s("--window"), s("5")], &c)
            .await;
        assert!(r.is_ok(), "hydrate failed: {}", r.message());
        assert_eq!(
            d.kernel_db().lock().get_hydration_policy(ctx).unwrap(),
            Some((tail, 5)),
            "marker defaults to the current tail, window 5"
        );

        let r2 = d.dispatch(&[s("context"), s("hydrate"), s("--clear")], &c).await;
        assert!(r2.is_ok(), "clear failed: {}", r2.message());
        assert!(
            d.kernel_db().lock().get_hydration_policy(ctx).unwrap().is_none(),
            "clear reverts to hydrate-everything"
        );
    }

    #[tokio::test]
    async fn context_hydrate_requires_window_or_clear() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("hydra2"), None, principal);
        let c = caller_with_context(ctx);
        let r = d.dispatch(&[s("context"), s("hydrate")], &c).await;
        assert!(!r.is_ok(), "bare hydrate must error (no --window, no --clear)");
        assert!(r.message().contains("--window"), "got: {}", r.message());
    }

    #[tokio::test]
    async fn context_hydrate_rejects_window_zero() {
        // window 0 → prefix-only → the just-inserted user prompt (in the tail)
        // never reaches the wire; the turn answers a prompt the model can't see
        // (or 400s on an assistant-final / empty messages). Reject at the verb.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("hydra-w0"), None, principal);
        d.block_store()
            .create_document(ctx, crate::DocumentKind::Conversation, None)
            .unwrap();
        d.block_store()
            .insert_block_as(
                ctx,
                None,
                None,
                kaijutsu_types::Role::User,
                kaijutsu_types::BlockKind::Text,
                "seed".to_string(),
                kaijutsu_types::Status::Done,
                kaijutsu_types::ContentType::Plain,
                Some(principal),
            )
            .unwrap();
        let c = caller_with_context(ctx);

        let r = d
            .dispatch(&[s("context"), s("hydrate"), s("--window"), s("0")], &c)
            .await;
        assert!(!r.is_ok(), "--window 0 must error");
        assert!(r.message().contains("window"), "got: {}", r.message());
        assert!(
            d.kernel_db().lock().get_hydration_policy(ctx).unwrap().is_none(),
            "a rejected --window 0 must not persist a policy"
        );
    }

    #[tokio::test]
    async fn context_hydrate_rejects_mark_not_in_context() {
        // A parseable but non-existent --mark would persist durably, then
        // fail-safe to the whole log every turn — the cost guard silently OFF
        // forever. Validate the block exists in the target context.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("hydra-mark"), None, principal);
        d.block_store()
            .create_document(ctx, crate::DocumentKind::Conversation, None)
            .unwrap();
        d.block_store()
            .insert_block_as(
                ctx,
                None,
                None,
                kaijutsu_types::Role::User,
                kaijutsu_types::BlockKind::Text,
                "seed".to_string(),
                kaijutsu_types::Status::Done,
                kaijutsu_types::ContentType::Plain,
                Some(principal),
            )
            .unwrap();
        let c = caller_with_context(ctx);

        // Parseable BlockId, but never inserted into this context.
        let phantom = kaijutsu_types::BlockId::new(ctx, PrincipalId::new(), 9999).to_key();
        let r = d
            .dispatch(
                &[s("context"), s("hydrate"), s("--window"), s("4"), s("--mark"), s(&phantom)],
                &c,
            )
            .await;
        assert!(!r.is_ok(), "a --mark not in the context must error");
        assert!(
            r.message().contains("not in") || r.message().contains("not found"),
            "got: {}",
            r.message()
        );
        assert!(
            d.kernel_db().lock().get_hydration_policy(ctx).unwrap().is_none(),
            "a rejected --mark must not persist a policy"
        );
    }

    #[tokio::test]
    async fn context_log_shows_lineage() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let root = register_context(&d, Some("root"), None, principal);
        let child = register_context(&d, Some("child"), Some(root), principal);

        let c = caller_with_context(child);
        let result = d.dispatch(&[s("context"), s("log")], &c).await;
        assert!(result.is_ok(), "log failed: {}", result.message());
        let msg = result.message();
        assert!(msg.contains("child"), "output: {msg}");
        assert!(msg.contains("root"), "output: {msg}");
    }

    #[tokio::test]
    async fn context_archive_requires_latch() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("doomed"), None, principal);

        let c = caller_with_context(ctx);
        let result = d
            .dispatch(&[s("context"), s("archive"), s("doomed")], &c)
            .await;
        assert!(result.is_latch(), "expected latch, got: {:?}", result);
    }

    #[tokio::test]
    async fn context_archive_confirmed() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);
        let target = register_context(&d, Some("target"), Some(parent), principal);

        let c = crate::kj::test_helpers::confirmed_holder_caller(&d, parent);
        let result = d
            .dispatch(&[s("context"), s("archive"), s("target")], &c)
            .await;
        assert!(result.is_ok(), "archive failed: {}", result.message());
        assert!(
            result.message().contains("archived"),
            "msg: {}",
            result.message()
        );

        // Verify archived
        let db = d.kernel_db().lock();
        let row = db.get_context(target).unwrap().unwrap();
        assert!(row.archived_at.is_some());
    }

    #[tokio::test]
    async fn context_rename_current_updates_db_and_drift_index() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("oldname"), None, principal);

        let c = caller_with_context(ctx);
        let result = d
            .dispatch(&[s("context"), s("rename"), s("newname")], &c)
            .await;
        assert!(result.is_ok(), "rename failed: {}", result.message());
        assert!(result.message().contains("oldname"), "{}", result.message());
        assert!(result.message().contains("newname"), "{}", result.message());

        // DB is the authority.
        {
            let db = d.kernel_db().lock();
            let row = db.get_context(ctx).unwrap().unwrap();
            assert_eq!(row.label.as_deref(), Some("newname"));
        }
        // The live label index followed (the mirror the verb must not skip).
        {
            let router = d.drift_router().read();
            let h = router.get(ctx).expect("still registered");
            assert_eq!(h.label.as_deref(), Some("newname"));
        }
    }

    #[tokio::test]
    async fn context_rename_by_ref_and_resolves_after() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let home = register_context(&d, Some("home"), None, principal);
        let _other = register_context(&d, Some("scratchpad"), None, principal);

        let c = caller_with_context(home);
        let result = d
            .dispatch(
                &[s("context"), s("rename"), s("workbench"), s("--context"), s("scratchpad")],
                &c,
            )
            .await;
        assert!(result.is_ok(), "rename failed: {}", result.message());

        // The new label resolves as a context ref end-to-end.
        let info = d
            .dispatch(&[s("context"), s("info"), s("workbench")], &c)
            .await;
        assert!(info.is_ok(), "info by new label failed: {}", info.message());
    }

    #[tokio::test]
    async fn context_rename_refuses_taken_label() {
        // Label-stealing is `retag`'s (latched) job — rename must refuse.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let a = register_context(&d, Some("alpha"), None, principal);
        let _b = register_context(&d, Some("beta"), None, principal);

        let c = caller_with_context(a);
        let result = d
            .dispatch(&[s("context"), s("rename"), s("beta")], &c)
            .await;
        assert!(!result.is_ok(), "rename onto a taken label must fail");
        assert!(
            result.message().contains("already in use"),
            "msg: {}",
            result.message()
        );

        // The original label survives the refused rename.
        let db = d.kernel_db().lock();
        let row = db.get_context(a).unwrap().unwrap();
        assert_eq!(row.label.as_deref(), Some("alpha"));
    }

    /// Archive is one row, never a subtree: a child keeps its state and its
    /// parent edge, so the lineage above it stays in the graph.
    #[tokio::test]
    async fn context_archive_leaves_children_live_with_their_parent_edge() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let root = register_context(&d, Some("root"), None, principal);
        let old = register_context(&d, Some("old-root"), Some(root), principal);
        let child = register_context(&d, Some("child"), Some(old), principal);

        let c = crate::kj::test_helpers::confirmed_holder_caller(&d, root);
        let result = d
            .dispatch(&[s("context"), s("archive"), s("old-root")], &c)
            .await;
        assert!(result.is_ok(), "archive failed: {}", result.message());
        assert_eq!(result.message(), "archived old-root");

        let db = d.kernel_db().lock();
        assert!(db.get_context(old).unwrap().unwrap().archived_at.is_some());
        let child_row = db.get_context(child).unwrap().unwrap();
        assert!(child_row.archived_at.is_none(), "the child was archived with its parent");
        assert_eq!(child_row.context_state, kaijutsu_types::ContextState::Live);
        assert_eq!(child_row.forked_from, Some(old), "the child lost its parent");
        let router = d.drift_router().read();
        assert_eq!(router.get(child).unwrap().state, kaijutsu_types::ContextState::Live);
    }

    /// The name of an archived context can be given again — the daily root
    /// rotation depends on it — and the archived row keeps its own copy.
    #[tokio::test]
    async fn context_create_may_reuse_an_archived_label() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);
        let old = register_context(&d, Some("ROOT-old"), Some(parent), principal);

        let c = crate::kj::test_helpers::confirmed_holder_caller(&d, parent);
        let result = d
            .dispatch(&[s("context"), s("archive"), s("ROOT-old")], &c)
            .await;
        assert!(result.is_ok(), "archive failed: {}", result.message());

        crate::kj::test_helpers::give_character(&d, &c);
        let result = d
            .dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("ROOT-old")], &c)
            .await;
        assert!(result.is_ok(), "create over an archived label failed: {}", result.message());

        let db = d.kernel_db().lock();
        let holder = db.find_context_by_label("ROOT-old").unwrap().expect("a live holder");
        assert_ne!(holder.context_id, old);
        assert_eq!(db.get_context(old).unwrap().unwrap().label.as_deref(), Some("ROOT-old"));
        let router = d.drift_router().read();
        assert_eq!(router.resolve_context("ROOT-old").unwrap(), holder.context_id);
    }

    #[tokio::test]
    async fn context_archive_flips_drift_router_state() {
        // M2-B3: archive must mark the in-memory drift router state as
        // Archived so an active session can't resurrect the context with
        // the next op (the constellation archive-while-joined bug).
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);
        let target = register_context(&d, Some("target"), Some(parent), principal);

        // Sanity: target is Live in drift router pre-archive.
        {
            let router = d.drift_router().read();
            let h = router.get(target).expect("target registered");
            assert_eq!(h.state, kaijutsu_types::ContextState::Live);
        }

        let c = crate::kj::test_helpers::confirmed_holder_caller(&d, parent);
        let result = d
            .dispatch(&[s("context"), s("archive"), s("target")], &c)
            .await;
        assert!(result.is_ok(), "archive failed: {}", result.message());

        // Drift router state must reflect archive.
        let router = d.drift_router().read();
        let h = router.get(target).expect("still registered");
        assert_eq!(
            h.state,
            kaijutsu_types::ContextState::Archived,
            "drift router state should be Archived post-archive"
        );
    }

    #[tokio::test]
    async fn context_promote_then_reprompt_is_no_op() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("risingstar"), None, principal);
        let steward = register_context(&d, Some("steward"), None, principal);

        let c = caller_with_context(steward);
        let result = d
            .dispatch(&[s("context"), s("promote"), s("risingstar")], &c)
            .await;
        assert!(result.is_ok(), "promote failed: {}", result.message());
        assert!(result.message().contains("promoted"), "msg: {}", result.message());

        let stamp = {
            let db = d.kernel_db().lock();
            db.get_context(ctx).unwrap().unwrap().promoted_at.unwrap()
        };

        // Re-promote is an honest no-op, not a restamp.
        let result = d
            .dispatch(&[s("context"), s("promote"), s("risingstar")], &c)
            .await;
        assert!(result.is_ok());
        assert!(
            result.message().contains("already promoted"),
            "msg: {}",
            result.message()
        );
        let db = d.kernel_db().lock();
        assert_eq!(db.get_context(ctx).unwrap().unwrap().promoted_at, Some(stamp));
    }

    #[tokio::test]
    async fn context_demote_ladder_via_dispatch() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("falling"), None, principal);
        let steward = register_context(&d, Some("steward"), None, principal);
        let mut c = caller_with_context(steward);

        d.dispatch(&[s("context"), s("promote"), s("falling")], &c)
            .await;

        // promoted → unpromoted
        let result = d
            .dispatch(&[s("context"), s("demote"), s("falling")], &c)
            .await;
        assert!(result.is_ok());
        assert!(result.message().contains("unpromoted"), "msg: {}", result.message());

        // neither → demoted
        let result = d
            .dispatch(&[s("context"), s("demote"), s("falling")], &c)
            .await;
        assert!(result.is_ok());
        assert!(result.message().contains("demoted"), "msg: {}", result.message());

        // already demoted → archived once confirmed, and the drift router
        // reflects it
        c.confirmed = true;
        let result = d
            .dispatch(&[s("context"), s("demote"), s("falling")], &c)
            .await;
        assert!(result.is_ok());
        assert!(result.message().contains("archived"), "msg: {}", result.message());

        let router = d.drift_router().read();
        let h = router.get(ctx).expect("still registered");
        assert_eq!(h.state, kaijutsu_types::ContextState::Archived);
    }

    #[tokio::test]
    async fn context_pause_and_resume_round_trip() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("napping"), None, principal);
        let c = caller_with_context(ctx);

        let result = d
            .dispatch(&[s("context"), s("pause"), s("napping")], &c)
            .await;
        assert!(result.is_ok(), "pause failed: {}", result.message());
        assert!(result.message().contains("paused"));
        {
            let db = d.kernel_db().lock();
            assert!(db.get_context(ctx).unwrap().unwrap().paused_at.is_some());
        }

        let result = d
            .dispatch(&[s("context"), s("resume"), s("napping")], &c)
            .await;
        assert!(result.is_ok(), "resume failed: {}", result.message());
        assert!(result.message().contains("resumed"));
        let db = d.kernel_db().lock();
        assert_eq!(db.get_context(ctx).unwrap().unwrap().paused_at, None);
    }

    /// The house as `kaijutsu-server bootstrap` builds it. amy (a root)
    /// plays her human root `amy` (type `root`); the `ROOT` console (type
    /// `console`) sits under it, played by the model character `rootbot`;
    /// banto plays the director seat `banto` under `amy`, and coder plays
    /// `lane` under banto's seat. bob (another root) plays `bob`, and
    /// `elsewhere` is a worker in bob's lineage, played by `other`. The
    /// root, console, and director seats hold Operator, as their rc grants
    /// it; the workers do not.
    struct PlacementFixture {
        d: std::sync::Arc<crate::kj::KjDispatcher>,
        amy: PrincipalId,
        rootbot: PrincipalId,
        banto: PrincipalId,
        coder: PrincipalId,
        root: ContextId,
        console: ContextId,
        seat: ContextId,
        lane: ContextId,
        bob_root: ContextId,
        elsewhere: ContextId,
    }

    async fn placement_fixture() -> PlacementFixture {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        d.kernel().broker().set_kj_dispatcher(&d).await;
        let [amy, rootbot, banto, coder, bob, other] = std::array::from_fn(|_| PrincipalId::new());
        let root = register_context(&d, Some("amy"), None, amy);
        let console = register_context(&d, Some("ROOT"), Some(root), amy);
        let seat = register_context(&d, Some("banto"), Some(root), amy);
        let lane = register_context(&d, Some("lane"), Some(seat), banto);
        let bob_root = register_context(&d, Some("bob"), None, bob);
        let elsewhere = register_context(&d, Some("elsewhere"), Some(bob_root), bob);
        let mut db = d.kernel_db().lock();
        for (principal_id, name, root) in [
            (amy, "placement-amy", true), (rootbot, "placement-rootbot", false), (banto, "placement-banto", false),
            (coder, "placement-coder", false), (bob, "placement-bob", true), (other, "placement-other", false),
        ] {
            db.insert_character(&crate::kernel_db::CharacterRow {
                principal_id, name: s(name), created_at: 1, retired_at: None, handoff_ctx: None, root_ctx: None, root,
            }).unwrap();
        }
        for (context, performer, context_type) in [
            (root, amy, crate::kj::character::ROOT_CONTEXT_TYPE), (console, rootbot, kaijutsu_types::CONSOLE_CONTEXT_TYPE),
            (seat, banto, "director"), (lane, coder, "coder"), (bob_root, bob, crate::kj::character::ROOT_CONTEXT_TYPE),
            (elsewhere, other, "coder"),
        ] {
            db.update_played_by(context, Some(performer)).unwrap();
            db.update_context_type(context, context_type).unwrap();
        }
        let mut operator = crate::mcp::ContextToolBinding::new();
        operator.grant(crate::mcp::Capability::Operator);
        let mut worker = crate::mcp::ContextToolBinding::new();
        worker.grant(crate::mcp::Capability::Fork);
        for (context, binding) in [
            (root, &operator), (console, &operator), (seat, &operator), (bob_root, &operator),
            (lane, &worker), (elsewhere, &worker),
        ] {
            db.upsert_context_binding(context, binding).unwrap();
        }
        drop(db);
        PlacementFixture { d, amy, rootbot, banto, coder, root, console, seat, lane, bob_root, elsewhere }
    }

    fn placement_caller(context: ContextId, actor: PrincipalId) -> crate::kj::KjCaller {
        crate::kj::KjCaller {
            principal_id: PrincipalId::new(), actor_id: actor, reviewer_id: None, context_id: Some(context),
            session_id: kaijutsu_types::SessionId::new(), confirmed: false, rc_depth: 0, privileged: false,
            cancel: tokio_util::sync::CancellationToken::new(),
        }
    }

    fn placement_row(f: &PlacementFixture, context: ContextId) -> crate::kernel_db::ContextRow {
        f.d.kernel_db().lock().get_context(context).unwrap().unwrap()
    }

    fn placement_unchanged(row: &crate::kernel_db::ContextRow) -> bool {
        row.promoted_at.is_none() && row.demoted_at.is_none() && row.concluded_at.is_none() && !row.is_archived()
    }

    /// The ROOT console moves a director's seat, and itself.
    #[tokio::test]
    async fn the_root_console_places_a_directors_seat_and_itself() {
        let f = placement_fixture().await;
        let console = placement_caller(f.console, f.rootbot);
        for verb in ["promote", "demote", "demote", "conclude"] {
            let result = f.d.dispatch(&[s("context"), s(verb), s("banto")], &console).await;
            assert!(result.is_ok(), "ROOT may {verb} the director's seat: {}", result.message());
        }
        let row = placement_row(&f, f.seat);
        assert!(row.demoted_at.is_some() && row.concluded_at.is_some(), "{row:?}");

        let result = f.d.dispatch(&[s("context"), s("promote"), s("ROOT")], &console).await;
        assert!(result.is_ok(), "ROOT may promote itself: {}", result.message());
        assert!(placement_row(&f, f.console).promoted_at.is_some());
    }

    /// A human root moves any context: a worker, a director's seat, the
    /// ROOT console, another human root, and its own root.
    #[tokio::test]
    async fn a_human_root_places_any_context() {
        let f = placement_fixture().await;
        let amy = placement_caller(f.root, f.amy);
        for target in ["lane", "elsewhere", "banto", "ROOT", "bob", "amy"] {
            for verb in ["promote", "demote", "demote"] {
                let result = f.d.dispatch(&[s("context"), s(verb), s(target)], &amy).await;
                assert!(result.is_ok(), "a human root may {verb} {target}: {}", result.message());
            }
        }
        for target in ["lane", "banto", "ROOT"] {
            let result = f.d.dispatch(&[s("context"), s("conclude"), s(target)], &amy).await;
            assert!(result.is_ok(), "a human root may conclude {target}: {}", result.message());
        }
        assert!(placement_row(&f, f.root).demoted_at.is_some());
        assert!(placement_row(&f, f.console).concluded_at.is_some());
    }

    /// Conclusion has no undo, and a concluded context refuses drifts, so
    /// nobody concludes a human root, not even the human root itself.
    #[tokio::test]
    async fn nobody_concludes_a_human_root() {
        let f = placement_fixture().await;
        for (caller, who) in [(placement_caller(f.root, f.amy), "amy"), (placement_caller(f.console, f.rootbot), "ROOT")] {
            let result = f.d.dispatch(&[s("context"), s("conclude"), s("amy")], &caller).await;
            assert!(!result.is_ok(), "{who} may not conclude a human root: {}", result.message());
            assert!(result.message().contains("rotate"), "the refusal names the alternative: {}", result.message());
        }
        assert!(placement_row(&f, f.root).concluded_at.is_none());
    }

    /// A director moves any context in the house that is not a root, the
    /// console, or its own: here a worker in another root's lineage.
    #[tokio::test]
    async fn a_director_places_a_worker_outside_its_lineage() {
        let f = placement_fixture().await;
        let banto = placement_caller(f.seat, f.banto);
        for verb in ["promote", "demote", "demote", "conclude"] {
            let result = f.d.dispatch(&[s("context"), s(verb), s("elsewhere")], &banto).await;
            assert!(result.is_ok(), "the director may {verb} elsewhere: {}", result.message());
        }
        let row = placement_row(&f, f.elsewhere);
        assert!(row.demoted_at.is_some() && row.concluded_at.is_some(), "{row:?}");
    }

    /// A director does not move its own seat, whether it names the context
    /// it runs in or another context its character plays. The refusal
    /// says to ask ROOT.
    #[tokio::test]
    async fn a_director_is_refused_on_its_own_seat() {
        let f = placement_fixture().await;
        let second = register_context(&f.d, Some("banto-2"), Some(f.root), f.amy);
        f.d.kernel_db().lock().update_played_by(second, Some(f.banto)).unwrap();
        let banto = placement_caller(f.seat, f.banto);
        for (target, id) in [("banto", f.seat), ("banto-2", second)] {
            for verb in ["promote", "demote", "conclude"] {
                let result = f.d.dispatch(&[s("context"), s(verb), s(target)], &banto).await;
                assert!(!result.is_ok(), "the director may not {verb} its own {target}: {}", result.message());
                assert!(result.message().contains("ask ROOT"), "the refusal says who to ask: {}", result.message());
                assert!(placement_unchanged(&placement_row(&f, id)), "a refusal changes nothing");
            }
        }
    }

    /// A worker without Operator moves nothing. Another context's refusal
    /// names the capability and says to go through its director or ROOT;
    /// its own context's refusal says to ask ROOT.
    #[tokio::test]
    async fn a_worker_without_operator_places_nothing() {
        let f = placement_fixture().await;
        let coder = placement_caller(f.lane, f.coder);
        for verb in ["promote", "demote", "conclude"] {
            let other = f.d.dispatch(&[s("context"), s(verb), s("elsewhere")], &coder).await;
            assert!(!other.is_ok(), "coder may not {verb} elsewhere: {}", other.message());
            assert!(other.message().contains("'operator'"), "the refusal names the capability: {}", other.message());
            assert!(other.message().contains("director") && other.message().contains("ROOT"), "{}", other.message());

            let own = f.d.dispatch(&[s("context"), s(verb), s("lane")], &coder).await;
            assert!(!own.is_ok(), "coder may not {verb} its own lane: {}", own.message());
            assert!(own.message().contains("ask ROOT"), "{}", own.message());
        }
        assert!(placement_unchanged(&placement_row(&f, f.elsewhere)));
        assert!(placement_unchanged(&placement_row(&f, f.lane)));
    }

    /// Only ROOT moves a human root or the console. A director with
    /// Operator is refused on both. The console itself may not demote or
    /// conclude a human root, or demote itself: only a human root does.
    #[tokio::test]
    async fn only_root_places_a_root_or_the_console() {
        let f = placement_fixture().await;
        let banto = placement_caller(f.seat, f.banto);
        for (target, id) in [("amy", f.root), ("ROOT", f.console), ("bob", f.bob_root)] {
            for verb in ["promote", "demote", "conclude"] {
                let result = f.d.dispatch(&[s("context"), s(verb), s(target)], &banto).await;
                assert!(!result.is_ok(), "the director may not {verb} {target}: {}", result.message());
                assert!(result.message().contains("ROOT"), "{}", result.message());
            }
            assert!(placement_unchanged(&placement_row(&f, id)), "a refusal changes nothing");
        }

        let console = placement_caller(f.console, f.rootbot);
        for (verb, target) in [("demote", "amy"), ("demote", "bob"), ("demote", "ROOT")] {
            let result = f.d.dispatch(&[s("context"), s(verb), s(target)], &console).await;
            assert!(!result.is_ok(), "the console may not {verb} {target}: {}", result.message());
            assert!(result.message().contains("human root"), "{}", result.message());
        }
        for id in [f.root, f.bob_root, f.console] {
            assert!(placement_unchanged(&placement_row(&f, id)), "a refusal changes nothing");
        }
    }

    /// ROOT privilege follows the human: a live root character acting from
    /// inside a worker that lacks Operator demotes and archives that worker,
    /// as a client's key on the worker's card does. The worker's model seat
    /// in the same context is refused both.
    #[tokio::test]
    async fn a_root_character_places_and_archives_from_inside_a_lane() {
        let f = placement_fixture().await;
        let coder = placement_caller(f.lane, f.coder);
        let demote = f.d.dispatch(&[s("context"), s("demote"), s("lane")], &coder).await;
        assert!(!demote.is_ok(), "the lane's model seat may not demote it: {}", demote.message());
        let mut coder_confirmed = coder.clone();
        coder_confirmed.confirmed = true;
        let archive = f.d.dispatch(&[s("context"), s("archive"), s("lane")], &coder_confirmed).await;
        assert!(!archive.is_ok(), "the lane's model seat may not archive it: {}", archive.message());
        assert!(archive.message().contains("'operator'"), "{}", archive.message());
        assert!(placement_unchanged(&placement_row(&f, f.lane)));

        let mut amy = placement_caller(f.lane, f.amy);
        for verb in ["promote", "demote", "demote"] {
            let result = f.d.dispatch(&[s("context"), s(verb), s("lane")], &amy).await;
            assert!(result.is_ok(), "amy may {verb} the lane from inside it: {}", result.message());
        }
        for (verb, target) in [("demote", "ROOT"), ("conclude", "ROOT"), ("demote", "banto")] {
            let result = f.d.dispatch(&[s("context"), s(verb), s(target)], &amy).await;
            assert!(result.is_ok(), "amy may {verb} {target} from the lane: {}", result.message());
        }
        let conclude = f.d.dispatch(&[s("context"), s("conclude"), s("amy")], &amy).await;
        assert!(!conclude.is_ok(), "nobody concludes a human root: {}", conclude.message());

        let latched = f.d.dispatch(&[s("context"), s("archive"), s("lane")], &amy).await;
        assert!(latched.is_latch(), "archive still needs --confirm: {latched:?}");
        amy.confirmed = true;
        let archived = f.d.dispatch(&[s("context"), s("archive"), s("lane")], &amy).await;
        assert!(archived.is_ok(), "amy may archive the lane from inside it: {}", archived.message());
        assert!(placement_row(&f, f.lane).is_archived());
    }

    /// A retired root character is no longer anyone's hands: it gets no
    /// ROOT privilege from a context that lacks Operator.
    #[tokio::test]
    async fn a_retired_root_character_has_no_root_privilege() {
        let f = placement_fixture().await;
        f.d.kernel_db().lock().retire_character(f.amy, 2).unwrap();
        let amy = placement_caller(f.lane, f.amy);
        let result = f.d.dispatch(&[s("context"), s("demote"), s("elsewhere")], &amy).await;
        assert!(!result.is_ok(), "a retired root has no privilege: {}", result.message());
        assert!(placement_unchanged(&placement_row(&f, f.elsewhere)));
    }

    /// The demote step that archives a context latches like
    /// `kj context archive`: without `--confirm` nothing changes and no
    /// archive lifecycle runs; with it the context is archived. The latch
    /// holds for a privileged caller too.
    #[tokio::test]
    async fn context_demote_latches_the_step_that_archives() {
        let f = placement_fixture().await;
        install_archive_script(&f.d, f.lane, "true").await;
        let mut banto = placement_caller(f.seat, f.banto);
        let demoted = f.d.dispatch(&[s("context"), s("demote"), s("lane")], &banto).await;
        assert!(demoted.is_ok() && demoted.message().contains("demoted"), "{}", demoted.message());

        let mut privileged = banto.clone();
        privileged.privileged = true;
        for caller in [&banto, &privileged] {
            let latched = f.d.dispatch(&[s("context"), s("demote"), s("lane")], caller).await;
            assert!(latched.is_latch(), "archiving needs --confirm: {latched:?}");
            assert!(latched.message().contains("--confirm") && latched.message().contains("archive"), "{}", latched.message());
            assert!(!placement_row(&f, f.lane).is_archived());
            assert!(!archive_lifecycle_ran(&f.d, f.lane), "a latched demote runs no archive lifecycle");
        }

        banto.confirmed = true;
        let archived = f.d.dispatch(&[s("context"), s("demote"), s("lane")], &banto).await;
        assert!(archived.is_ok() && archived.message().contains("archived"), "{}", archived.message());
        assert!(placement_row(&f, f.lane).is_archived());
    }

    #[tokio::test]
    async fn context_promote_surfaces_the_ring_full_error() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let c = caller_with_context(register_context(&d, Some("steward"), None, principal));

        for i in 0..10 {
            register_context(&d, Some(&format!("seat-{i}")), None, principal);
            let result = d
                .dispatch(&[s("context"), s("promote"), s(&format!("seat-{i}"))], &c)
                .await;
            assert!(result.is_ok(), "seat {i} promote failed: {}", result.message());
        }

        register_context(&d, Some("overflow"), None, principal);
        let result = d
            .dispatch(&[s("context"), s("promote"), s("overflow")], &c)
            .await;
        assert!(!result.is_ok(), "11th promote should fail");
        assert!(
            result.message().contains("active ring full"),
            "the Validation message should surface through the verb: {}",
            result.message()
        );
    }

    /// Demoting past the rim archives through the `archive` lifecycle, as
    /// `kj context archive` does. Falsified by archiving in the demote
    /// ladder alone: no archive run is recorded.
    #[tokio::test]
    async fn context_demote_past_the_rim_runs_the_archive_lifecycle() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        d.kernel().broker().set_kj_dispatcher(&d).await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);
        let target = register_context(&d, Some("outward"), Some(parent), principal);
        install_archive_script(&d, target, "true").await;
        let c = confirmed_caller(parent);
        let demoted = d.dispatch(&[s("context"), s("demote"), s("outward")], &c).await;
        assert!(demoted.is_ok(), "{}", demoted.message());
        assert!(!archive_lifecycle_ran(&d, target), "a demote that does not archive runs no archive lifecycle");
        let archived = d.dispatch(&[s("context"), s("demote"), s("outward")], &c).await;
        assert!(archived.is_ok() && archived.message().contains("archived"), "{}", archived.message());
        assert!(archive_lifecycle_ran(&d, target), "the archive lifecycle ran before demote archived it");
    }

    #[tokio::test]
    async fn context_demote_on_archived_errors_loudly() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);
        let target = register_context(&d, Some("gone"), Some(parent), principal);

        let c = crate::kj::test_helpers::confirmed_holder_caller(&d, parent);
        let result = d
            .dispatch(&[s("context"), s("archive"), s("gone")], &c)
            .await;
        assert!(result.is_ok(), "archive failed: {}", result.message());

        // Reachable only by full id (fuzzy resolution skips archived), and
        // the ladder has nothing further out to step to.
        let result = d
            .dispatch(&[s("context"), s("demote"), target.to_string()], &c)
            .await;
        assert!(!result.is_ok(), "demote of an archived context must error");
        assert!(
            result.message().contains("cannot demote an archived context"),
            "msg: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn context_promote_resurrects_an_archived_context_by_full_id() {
        // Promote is the resurrection door. Fuzzy resolution can't reach an
        // archived context (label lookups walk the active set), so the full
        // id is the handle — and the drift router must come back Live, or
        // sessions would still see the context as dead.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);
        let target = register_context(&d, Some("lazarus"), Some(parent), principal);

        let c = crate::kj::test_helpers::confirmed_holder_caller(&d, parent);
        let result = d
            .dispatch(&[s("context"), s("archive"), s("lazarus")], &c)
            .await;
        assert!(result.is_ok(), "archive failed: {}", result.message());

        // The label no longer resolves (archived is outside the fuzzy set)…
        let result = d
            .dispatch(&[s("context"), s("promote"), s("lazarus")], &c)
            .await;
        assert!(
            !result.is_ok(),
            "label of an archived context should not resolve: {}",
            result.message()
        );

        // …but the full id resurrects.
        let result = d
            .dispatch(&[s("context"), s("promote"), target.to_string()], &c)
            .await;
        assert!(result.is_ok(), "resurrect failed: {}", result.message());
        assert!(
            result.message().contains("resurrected"),
            "msg: {}",
            result.message()
        );

        {
            let db = d.kernel_db().lock();
            let row = db.get_context(target).unwrap().unwrap();
            assert_eq!(row.archived_at, None);
            assert!(row.promoted_at.is_some());
        }

        let router = d.drift_router().read();
        let h = router.get(target).expect("still registered");
        assert_eq!(
            h.state,
            kaijutsu_types::ContextState::Live,
            "resurrection must reverse archive's drift-router sync"
        );
    }

    #[tokio::test]
    async fn context_promote_resurrects_a_concluded_context_as_concluded() {
        // Conclusion is orthogonal to placement: a resurrected concluded
        // context comes back Concluded (seated for review), not Live.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);
        let target = register_context(&d, Some("reviewed"), Some(parent), principal);

        let c = crate::kj::test_helpers::confirmed_holder_caller(&d, parent);
        let result = d
            .dispatch(&[s("context"), s("conclude"), s("reviewed")], &c)
            .await;
        assert!(result.is_ok(), "conclude failed: {}", result.message());
        let result = d
            .dispatch(&[s("context"), s("archive"), s("reviewed")], &c)
            .await;
        assert!(result.is_ok(), "archive failed: {}", result.message());

        let result = d
            .dispatch(&[s("context"), s("promote"), target.to_string()], &c)
            .await;
        assert!(result.is_ok(), "resurrect failed: {}", result.message());

        {
            let db = d.kernel_db().lock();
            let row = db.get_context(target).unwrap().unwrap();
            assert_eq!(row.archived_at, None);
            assert!(row.concluded_at.is_some(), "resurrection must not un-conclude");
            assert!(row.promoted_at.is_some());
        }

        let router = d.drift_router().read();
        let h = router.get(target).expect("still registered");
        assert_eq!(h.state, kaijutsu_types::ContextState::Concluded);
    }

    #[tokio::test]
    async fn contexts_retain_history_instead_of_supporting_removal() {
        let d = test_dispatcher_persistent().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);
        let target = register_context(&d, Some("retained"), Some(parent), principal);
        d.block_store().create_document(target, kaijutsu_types::DocKind::Conversation, None).unwrap();
        let block = d.block_store().insert_block_as(target, None, None, kaijutsu_types::Role::User,
            kaijutsu_types::BlockKind::Text, "retained history", kaijutsu_types::Status::Done,
            kaijutsu_types::ContentType::Plain, Some(principal)).unwrap();
        let caller = crate::kj::test_helpers::confirmed_holder_caller(&d, parent);
        for verb in ["remove", "rm"] {
            let result = d.dispatch(&[s("context"), s(verb), target.to_hex()], &caller).await;
            assert!(!result.is_ok(), "{verb} must not delete contexts");
            assert!(result.message().contains("unrecognized subcommand"), "{}", result.message());
        }
        let archived = d.dispatch(&[s("context"), s("archive"), target.to_hex()], &caller).await;
        assert!(archived.is_ok(), "{}", archived.message());
        assert!(d.kernel_db().lock().get_context(target).unwrap().unwrap().is_archived());
        let deleted = d.dispatch(&[s("doc"), s("delete"), target.to_hex()], &caller).await;
        assert!(!deleted.is_ok(), "document deletion must not bypass context retention");
        assert!(deleted.message().contains("archive"), "{}", deleted.message());
        assert_eq!(d.block_store().get_block_snapshot(target, &block).unwrap().unwrap().content, "retained history");
        assert_eq!(d.kernel_db().lock().get_context(target).unwrap().unwrap().forked_from, Some(parent));
        let db = d.kernel_db().clone();
        let workspace = db.lock().get_or_create_default_workspace(principal).unwrap();
        let restored = crate::block_store::BlockStore::with_db(db, workspace, principal);
        restored.load_from_db().unwrap();
        assert_eq!(restored.get_block_snapshot(target, &block).unwrap().unwrap().content, "retained history");
    }

    #[tokio::test]
    async fn context_set_cwd() {
        let d = wired_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("target"), None, principal);
        mount_scratch_work(&d).await;

        let c = caller_with_context(ctx);
        let result = d
            .dispatch(
                &[s("context"), s("set"), s("."), s("--cwd"), s("/scratch/work")],
                &c,
            )
            .await;
        assert!(result.is_ok(), "set --cwd failed: {}", result.message());
        assert!(
            result.message().contains("cwd="),
            "msg: {}",
            result.message()
        );

        let db = d.kernel_db().lock();
        let shell = db.get_context_shell(ctx).unwrap().unwrap();
        assert_eq!(shell.cwd, Some("/scratch/work".into()));
    }

    /// A dispatcher registered as its own `Arc`, which a contextual shell
    /// needs: `--cwd` is checked through one, the way `cd` resolves it.
    async fn wired_dispatcher() -> std::sync::Arc<crate::kj::KjDispatcher> {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        d
    }

    /// `/scratch/work` is a directory and `/scratch/file` is a file, on the
    /// kernel mount table.
    async fn mount_scratch_work(d: &crate::kj::KjDispatcher) {
        use crate::vfs::VfsOps;
        d.kernel().mount("/scratch", crate::vfs::MemoryBackend::new()).await;
        d.kernel().vfs().mkdir(std::path::Path::new("/scratch/work"), 0o755).await.unwrap();
        d.kernel().vfs().write_all(std::path::Path::new("/scratch/file"), b"x").await.unwrap();
    }

    #[tokio::test]
    async fn context_set_cwd_refuses_a_path_that_is_not_a_directory() {
        let d = wired_dispatcher().await;
        let ctx = register_context(&d, Some("target"), None, PrincipalId::new());
        mount_scratch_work(&d).await;
        let c = caller_with_context(ctx);
        for path in ["/scratch/missing", "/scratch/file", "scratch/work"] {
            let result = d.dispatch(&[s("context"), s("set"), s("."), s("--cwd"), s(path)], &c).await;
            assert!(!result.is_ok(), "set --cwd {path} must refuse: {}", result.message());
            assert!(result.message().contains(path), "the refusal names {path}: {}", result.message());
        }
        assert!(d.kernel_db().lock().get_context_shell(ctx).unwrap().is_none(), "a refused cwd writes nothing");
    }

    #[tokio::test]
    async fn context_create_cwd_refuses_a_missing_directory_before_creating() {
        let d = wired_dispatcher().await;
        let parent = register_context(&d, Some("parent"), None, PrincipalId::new());
        mount_scratch_work(&d).await;
        let c = caller_with_context(parent);
        crate::kj::test_helpers::give_character(&d, &c);
        let before = d.kernel_db().lock().list_active_contexts().unwrap().len();
        let result = d.dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("lost"), s("--cwd"), s("/scratch/missing")], &c).await;
        assert!(!result.is_ok(), "create --cwd on a missing directory must refuse: {}", result.message());
        assert!(result.message().contains("kj kohai create: --cwd '/scratch/missing'"), "clients match this refusal: {}", result.message());
        assert_eq!(d.kernel_db().lock().list_active_contexts().unwrap().len(), before, "no context is created");

        let result = d.dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("found"), s("--cwd"), s("/scratch/work")], &c).await;
        assert!(result.is_ok(), "create --cwd on a directory succeeds: {}", result.message());
    }

    /// `--cwd-optional` decides when the create runs: a directory the kernel
    /// cannot use is dropped, the context takes the caller's cwd, and the
    /// result says why in its message and its `cwd_refused` data.
    #[tokio::test]
    async fn context_create_cwd_optional_drops_a_directory_the_kernel_cannot_use() {
        let d = wired_dispatcher().await;
        let parent = register_context(&d, Some("parent"), None, PrincipalId::new());
        mount_scratch_work(&d).await;
        let c = caller_with_context(parent);
        crate::kj::test_helpers::seed_test_performer(&d.kernel_db().lock());
        crate::kj::test_helpers::give_character(&d, &c);
        let cwd_of = |id: ContextId| d.kernel_db().lock().get_context_shell(id).unwrap().and_then(|shell| shell.cwd);
        let create = |label: &'static str, cwd: &'static str| {
            let d = d.clone();
            let c = c.clone();
            async move {
                d.dispatch(&[s("kohai"), s("create"), s(label), s("--as"), s(crate::kj::test_helpers::TEST_PERFORMER), s("--cwd"), s(cwd), s("--cwd-optional")], &c).await
            }
        };

        let result = create("lost", "/scratch/missing").await;
        let KjResult::Ok { message, data: Some(data), .. } = &result else {
            panic!("--cwd-optional creates without a missing directory: {}", result.message());
        };
        let refused = data["cwd_refused"].as_str().expect("the result carries the refusal");
        assert!(refused.contains("'/scratch/missing'"), "the refusal names the directory: {refused}");
        assert!(message.contains(refused), "the message says why too: {message}");
        let lost = ContextId::parse(data["context_id"].as_str().unwrap()).unwrap();
        assert_eq!(cwd_of(lost), None, "the parent has no cwd to inherit");

        let result = create("found", "/scratch/work").await;
        let KjResult::Ok { data: Some(data), .. } = &result else { panic!("{}", result.message()) };
        assert!(data["cwd_refused"].is_null(), "a usable directory is not refused: {data}");
        let found = ContextId::parse(data["context_id"].as_str().unwrap()).unwrap();
        assert_eq!(cwd_of(found).as_deref(), Some("/scratch/work"));

        let result = d.dispatch(&[s("kohai"), s("create"), s("bare"), s("--as"), s(crate::kj::test_helpers::TEST_PERFORMER), s("--cwd-optional")], &c).await;
        assert!(!result.is_ok(), "--cwd-optional needs --cwd: {}", result.message());
    }

    /// `/` is accepted as a cwd, and every command that leaves a context
    /// there says so in its output. Another directory draws no warning.
    #[tokio::test]
    async fn a_root_cwd_is_accepted_with_a_warning() {
        let d = wired_dispatcher().await;
        let parent = register_context(&d, Some("parent"), None, PrincipalId::new());
        mount_scratch_work(&d).await;
        let c = caller_with_context(parent);
        crate::kj::test_helpers::give_character(&d, &c);
        let warns = |message: &str, ctx: ContextId| message.contains(&format!(
            "\nwarning: context {short} has cwd /, which is almost always a mistake; \
             set its work tree with `kj context set {short} --cwd DIR`", short = ctx.short()));

        let result = d.dispatch(&[s("context"), s("set"), s("."), s("--cwd"), s("/")], &c).await;
        assert!(result.is_ok() && warns(result.message(), parent), "set --cwd /: {}", result.message());
        let result = d.dispatch(&[s("context"), s("set"), s("."), s("--cwd"), s("/scratch/work")], &c).await;
        assert!(result.is_ok() && !result.message().contains("warning"), "set --cwd DIR: {}", result.message());

        for (label, argv) in [
            ("explicit", vec![s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("explicit"), s("--cwd"), s("/")]),
            ("inherited", vec![s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("inherited")]),
        ] {
            if label == "inherited" {
                d.dispatch(&[s("context"), s("set"), s("."), s("--cwd"), s("/")], &c).await;
            }
            let result = d.dispatch(&argv, &c).await;
            assert!(result.is_ok(), "{label}: {}", result.message());
            let child = d.kernel_db().lock().find_context_by_label(label).unwrap().unwrap().context_id;
            assert!(warns(result.message(), child), "{label}: {}", result.message());
            assert_eq!(d.kernel_db().lock().get_context_shell(child).unwrap().unwrap().cwd.as_deref(), Some("/"));
        }
        let result = d.dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("worked"), s("--cwd"), s("/scratch/work")], &c).await;
        assert!(result.is_ok() && !result.message().contains("warning"), "create --cwd DIR: {}", result.message());
    }

    #[tokio::test]
    async fn context_set_env() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("target"), None, principal);

        let c = caller_with_context(ctx);
        let result = d
            .dispatch(
                &[
                    s("context"),
                    s("set"),
                    s("."),
                    s("--env"),
                    s("RUST_LOG=debug"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "set --env failed: {}", result.message());
        assert!(
            result.message().contains("env RUST_LOG=debug"),
            "msg: {}",
            result.message()
        );

        let db = d.kernel_db().lock();
        let env = db.get_context_env(ctx).unwrap();
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].key, "RUST_LOG");
        assert_eq!(env[0].value, "debug");
    }

    /// `--env` repeats: every KEY=VALUE given lands as a row. A single
    /// `Option<String>` field kept only the last one and dropped the rest
    /// without a word, which is how a seat created with
    /// `--env KJ_CHARACTER=banto --env ROTATED_FROM=ROOT` came up nameless.
    #[tokio::test]
    async fn context_set_env_repeated_keeps_every_pair() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("target"), None, principal);
        let c = caller_with_context(ctx);
        let result = d
            .dispatch(
                &[s("context"), s("set"), s("."), s("--env"), s("FIRST=1"), s("--env"), s("SECOND=2")],
                &c,
            )
            .await;
        assert!(result.is_ok(), "set --env twice failed: {}", result.message());
        let db = d.kernel_db().lock();
        let mut env: Vec<(String, String)> = db
            .get_context_env(ctx)
            .unwrap()
            .into_iter()
            .map(|v| (v.key, v.value))
            .collect();
        env.sort();
        assert_eq!(
            env,
            vec![("FIRST".to_string(), "1".to_string()), ("SECOND".to_string(), "2".to_string())],
            "both --env pairs must be stored"
        );
    }

    #[tokio::test]
    async fn context_set_env_bad_format() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("target"), None, principal);

        let c = caller_with_context(ctx);
        let result = d
            .dispatch(
                &[s("context"), s("set"), s("."), s("--env"), s("NOEQUALS")],
                &c,
            )
            .await;
        assert!(
            !result.is_ok(),
            "should fail without =: {}",
            result.message()
        );
        assert!(
            result.message().contains("KEY=VALUE"),
            "msg: {}",
            result.message()
        );
    }

    /// `KEY=VALUE` shape passes the upfront check, but a key that can never
    /// become a shell identifier must still be refused — at the durable
    /// write (`kernel_db.rs::validate_env_key`), not merely at the next
    /// shell materialization. Falsifies the bug this closes: before that
    /// write-time check existed, this same command stored the row and only
    /// failed later, inside `apply_context_config`'s `export`.
    #[tokio::test]
    async fn context_set_env_rejects_key_that_is_not_a_shell_identifier() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("target"), None, principal);

        let c = caller_with_context(ctx);
        let result = d
            .dispatch(
                &[s("context"), s("set"), s("."), s("--env"), s("1BAD=x")],
                &c,
            )
            .await;
        assert!(
            matches!(result, KjResult::Err(_)),
            "a key starting with a digit must be refused, got {result:?}"
        );
        assert!(
            result.message().contains("1BAD"),
            "error must name the offending key: {}",
            result.message()
        );

        let db = d.kernel_db().lock();
        assert!(
            db.get_context_env(ctx).unwrap().is_empty(),
            "a rejected key must never reach the durable table"
        );
    }

    #[tokio::test]
    async fn context_unset_env() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("target"), None, principal);

        // Set env var first
        {
            let db = d.kernel_db().lock();
            db.set_context_env(ctx, "FOO", "bar").unwrap();
        }

        let c = caller_with_context(ctx);
        let result = d
            .dispatch(
                &[s("context"), s("unset"), s("."), s("--env"), s("FOO")],
                &c,
            )
            .await;
        assert!(result.is_ok(), "unset failed: {}", result.message());
        assert!(
            result.message().contains("unset env FOO"),
            "msg: {}",
            result.message()
        );

        // Verify it's gone
        let db = d.kernel_db().lock();
        let env = db.get_context_env(ctx).unwrap();
        assert!(env.is_empty());
    }

    #[tokio::test]
    async fn context_unset_env_missing() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("target"), None, principal);

        let c = caller_with_context(ctx);
        let result = d
            .dispatch(
                &[s("context"), s("unset"), s("."), s("--env"), s("NOPE")],
                &c,
            )
            .await;
        assert!(
            !result.is_ok(),
            "should error for missing var: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn context_info_shows_shell_config() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("enriched"), None, principal);

        // Set shell config and env
        {
            let db = d.kernel_db().lock();
            db.upsert_context_shell(&crate::kernel_db::ContextShellRow {
                context_id: ctx,
                cwd: Some("/home/user/project".into()),
                updated_at: kaijutsu_types::now_millis() as i64,
            })
            .unwrap();
            db.set_context_env(ctx, "RUST_LOG", "debug").unwrap();
        }

        let c = caller_with_context(ctx);
        let result = d.dispatch(&[s("context"), s("info")], &c).await;
        assert!(result.is_ok(), "info failed: {}", result.message());
        let msg = result.message();
        assert!(msg.contains("Cwd:"), "should show cwd: {msg}");
        assert!(
            msg.contains("/home/user/project"),
            "should show cwd path: {msg}"
        );
        assert!(msg.contains("Env:"), "should show env: {msg}");
        assert!(msg.contains("RUST_LOG=debug"), "should show env var: {msg}");
    }

    /// Regression test for `kj context info`: the human-text and `--json`
    /// renders must agree about cwd. Guards against the pre-kaish-0.13
    /// `--json` envelope, which built a separate JSON shape
    /// from the human-text render and could show `shell: null` for a
    /// context whose human text showed a real `Cwd:` line. kaish 0.13
    /// retired that separate envelope (`render_json_envelope` is gone —
    /// `--json` now emits `data` verbatim, per `kj_builtin.rs`), and
    /// `context_info` itself has always fed both renders from the SAME
    /// `shell` binding (one `db.get_context_shell` read, destructured once,
    /// used by both the `Cwd:` text line and the `data.cwd` field) — so a
    /// disagreement is no longer structurally possible. This test pins
    /// that: both renders must agree whether cwd is set, and its value,
    /// covering the case that reproduced the original report (cwd IS set)
    /// as well as the unset case.
    #[tokio::test]
    async fn context_info_json_cwd_matches_human_render() {
        use crate::kj::KjResult;
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();

        // Case 1: cwd set — the exact shape of the original bug report.
        let with_cwd = register_context(&d, Some("cwd-set"), None, principal);
        {
            let db = d.kernel_db().lock();
            db.upsert_context_shell(&crate::kernel_db::ContextShellRow {
                context_id: with_cwd,
                cwd: Some("/home/atobey/kaijutsu".into()),
                updated_at: kaijutsu_types::now_millis() as i64,
            })
            .unwrap();
        }
        let c = caller_with_context(with_cwd);
        let result = d.dispatch(&[s("context"), s("info")], &c).await;
        assert!(result.is_ok(), "info failed: {}", result.message());
        let msg = result.message().to_string();
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                assert_eq!(
                    v["cwd"].as_str(),
                    Some("/home/atobey/kaijutsu"),
                    "data.cwd must carry the configured path, not null: {v:?}"
                );
                assert!(
                    msg.contains("Cwd:     /home/atobey/kaijutsu"),
                    "human text must show the same cwd data.cwd reports: {msg}"
                );
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }

        // Case 2: no shell row at all — both renders must agree it's absent
        // (omitted from text, null in JSON), not one showing stale data.
        let no_cwd = register_context(&d, Some("cwd-unset"), None, principal);
        let c2 = caller_with_context(no_cwd);
        let result2 = d.dispatch(&[s("context"), s("info")], &c2).await;
        assert!(result2.is_ok(), "info failed: {}", result2.message());
        let msg2 = result2.message().to_string();
        match result2 {
            KjResult::Ok { data: Some(v), .. } => {
                assert!(
                    v["cwd"].is_null(),
                    "data.cwd must be null when no shell row exists: {v:?}"
                );
                assert!(
                    !msg2.contains("Cwd:"),
                    "human text must omit Cwd: when data.cwd is null: {msg2}"
                );
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    fn record_turn(d: &crate::kj::KjDispatcher, ctx: kaijutsu_types::ContextId, input: u64, at: i64) -> kaijutsu_types::TurnId {
        let db = d.kernel_db().lock();
        let turn_id = kaijutsu_types::TurnId::new();
        db.begin_turn(&crate::TurnStart {
            turn_id, context_id: ctx, performer: PrincipalId::new(),
            role: "coder".into(), provider: "deepseek".into(), model: "deepseek-flash".into(),
            started_at: at,
        }).unwrap();
        db.add_turn_usage(turn_id, crate::RawUsage { input, output: 3, cache_read: 7, cache_write: 0, reasoning: 1 }).unwrap();
        db.finish_turn(turn_id, at + 5, "end_turn").unwrap();
        turn_id
    }

    /// `kj context info` totals the context's turns, and `kj context turns`
    /// lists them newest first with the model and role that answered.
    #[tokio::test]
    async fn context_info_totals_turns_and_turns_lists_them() {
        use crate::kj::KjResult;
        let d = test_dispatcher().await;
        let ctx = register_context(&d, Some("spender"), None, PrincipalId::new());
        let c = caller_with_context(ctx);
        let none = d.dispatch(&[s("context"), s("info")], &c).await;
        assert!(none.message().contains("\nTurns:   0"), "{}", none.message());

        let first = record_turn(&d, ctx, 100, 1_000);
        let second = record_turn(&d, ctx, 250, 2_000);
        let info = d.dispatch(&[s("context"), s("info")], &c).await;
        assert!(info.message().contains("\nTurns:   2 (2 calls): 350 in, 6 out, cache read 14, write 0, reasoning 2"),
            "{}", info.message());
        let KjResult::Ok { data: Some(data), .. } = &info else { panic!("info carries a record") };
        assert_eq!(data["turns"]["turns"], 2);
        assert_eq!(data["turns"]["input_tokens"], 350);

        let listed = d.dispatch(&[s("context"), s("turns")], &c).await;
        assert!(listed.is_ok(), "{}", listed.message());
        let msg = listed.message();
        assert!(msg.find(&second.short()).unwrap() < msg.find(&first.short()).unwrap(), "newest first: {msg}");
        assert!(msg.contains("deepseek/deepseek-flash") && msg.contains("(role coder)")
            && msg.contains("end_turn"), "{msg}");
        let KjResult::Ok { data: Some(data), .. } = &listed else { panic!("turns carries a record") };
        assert_eq!(data.as_array().unwrap().len(), 2);
        assert_eq!(data[0]["turn_id"], second.to_hex());

        let one = d.dispatch(&[s("context"), s("turns"), s("--limit"), s("1")], &c).await;
        let KjResult::Ok { data: Some(data), .. } = &one else { panic!("turns carries a record") };
        assert_eq!(data.as_array().unwrap().len(), 1);
    }

    /// `kj context info --json`'s `usage` field is the token-usage gauge's
    /// public surface — an app-side gauge is built against this shape, so it
    /// needs to expose exactly the persisted `ContextUsageRow`, both in the
    /// human text and the structured `data` record.
    #[tokio::test]
    async fn context_info_shows_usage() {
        use crate::kj::KjResult;
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("usage-ctx"), None, principal);

        {
            let db = d.kernel_db().lock();
            db.set_context_usage(&crate::kernel_db::ContextUsageRow {
                context_id: ctx,
                provider: "anthropic".into(),
                model: "claude-sonnet-4-6".into(),
                input_tokens: 1234,
                output_tokens: 567,
                cache_read_tokens: 800,
                cache_write_tokens: 100,
                reasoning_tokens: 0,
                cache_ttl_secs: 0,
                updated_at: kaijutsu_types::now_millis() as i64,
            })
            .unwrap();
        }

        let c = caller_with_context(ctx);
        let result = d.dispatch(&[s("context"), s("info")], &c).await;
        assert!(result.is_ok(), "info failed: {}", result.message());
        let msg = result.message();
        assert!(msg.contains("Tokens:"), "should show usage line: {msg}");
        assert!(msg.contains("1234"), "should show input tokens: {msg}");
        assert!(msg.contains("567"), "should show output tokens: {msg}");

        match result {
            KjResult::Ok { data: Some(v), .. } => {
                let usage = &v["usage"];
                assert_eq!(usage["provider"], "anthropic");
                assert_eq!(usage["model"], "claude-sonnet-4-6");
                assert_eq!(usage["input_tokens"], 1234);
                assert_eq!(usage["output_tokens"], 567);
                assert_eq!(usage["total_tokens"], 1234 + 567);
                assert_eq!(usage["cache_read_tokens"], 800);
                assert_eq!(usage["cache_write_tokens"], 100);
            }
            other => panic!("expected Ok with usage data, got {other:?}"),
        }
    }

    /// A context that never completed an LLM call must show `usage: null` —
    /// honest absence, never a fabricated zero (which would be
    /// indistinguishable from "measured and genuinely empty").
    #[tokio::test]
    async fn context_info_usage_null_when_absent() {
        use crate::kj::KjResult;
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("no-usage-ctx"), None, principal);

        let c = caller_with_context(ctx);
        let result = d.dispatch(&[s("context"), s("info")], &c).await;
        assert!(result.is_ok(), "info failed: {}", result.message());
        assert!(
            !result.message().contains("Tokens:"),
            "no usage line without a recorded call: {}",
            result.message()
        );

        match result {
            KjResult::Ok { data: Some(v), .. } => {
                assert!(v["usage"].is_null(), "usage must be null, got {v}");
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    /// The single most important correctness property of the token-usage
    /// gauge: a model with no configured context window (no `provider_configs`
    /// registered at all, exactly like a fresh kernel, or a model deliberately
    /// left unset such as `claude-sonnet-4-20250514`) must NEVER get a
    /// fabricated denominator. `context_window` and `context_used_pct` must
    /// both come back `null` — never a guessed window standing in, and never
    /// a percentage computed against one.
    #[tokio::test]
    async fn context_info_usage_pct_null_when_window_unconfigured() {
        use crate::kj::KjResult;
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("usage-no-window"), None, principal);

        {
            let db = d.kernel_db().lock();
            db.set_context_usage(&crate::kernel_db::ContextUsageRow {
                context_id: ctx,
                provider: "anthropic".into(),
                model: "claude-sonnet-4-20250514".into(),
                input_tokens: 1000,
                output_tokens: 200,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
                cache_ttl_secs: 0,
                updated_at: kaijutsu_types::now_millis() as i64,
            })
            .unwrap();
        }
        // No provider_configs registered at all — `LlmRegistry::context_window_for`
        // must resolve `None`, never a guessed default.

        let c = caller_with_context(ctx);
        let result = d.dispatch(&[s("context"), s("info")], &c).await;
        assert!(result.is_ok(), "info failed: {}", result.message());
        assert!(
            result.message().contains("window unknown"),
            "human text must say the window is unknown, not print a guessed pct: {}",
            result.message()
        );

        match result {
            KjResult::Ok { data: Some(v), .. } => {
                let usage = &v["usage"];
                assert!(
                    usage["context_window"].is_null(),
                    "context_window must be null, got {usage}"
                );
                assert!(
                    usage["context_used_pct"].is_null(),
                    "context_used_pct must be null when the window is unknown — a non-null \
                     percentage here would mean someone divided by a fabricated denominator, \
                     got {usage}"
                );
            }
            other => panic!("expected Ok with usage data, got {other:?}"),
        }
    }

    /// `kj context info` reports the model the context's slot gives its
    /// type, in the text and in the structured record.
    ///
    /// Falsified by reading a model from anywhere but the context's slots.
    #[tokio::test]
    async fn context_info_data_carries_the_slots_model() {
        use crate::kj::KjResult;
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_model(&d, Some("resolved-ctx"), None, principal, &["default"]);

        let c = caller_with_context(ctx);
        let result = d.dispatch(&[s("context"), s("info")], &c).await;
        assert!(result.is_ok(), "info failed: {}", result.message());
        assert!(
            result.message().contains("Model:    mock/mock-model"),
            "human text must show the bound model: {}",
            result.message()
        );
        assert!(!result.message().contains("Cast:"), "no cast label is stored: {}", result.message());

        match result {
            KjResult::Ok { data: Some(v), .. } => {
                assert_eq!(v["resolved_backend"], "mock");
                assert_eq!(v["resolved_model"], "mock-model");
                assert!(v.get("cast_label").is_none(), "no cast label is stored: {v}");
                assert!(v["model_absent"].is_null(), "{v}");
                assert!(v.get("resolved_source").is_none(), "no resolver tier remains: {v}");
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    /// A context with no slot for its type reports no model, and says why:
    /// the same line `kj kohai create` and `kj context rotate` print. A
    /// context with no context character says so.
    ///
    /// Falsified by an info that names any other model.
    #[tokio::test]
    async fn context_info_says_why_the_context_has_no_model() {
        use crate::kj::KjResult;
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let bare = register_context(&d, Some("unbound-ctx"), None, principal);
        let c = caller_with_context(bare);
        let result = d.dispatch(&[s("context"), s("info")], &c).await;
        assert!(result.is_ok(), "info failed: {}", result.message());
        match result {
            KjResult::Ok { data: Some(v), message, .. } => {
                assert!(v["resolved_backend"].is_null() && v["resolved_model"].is_null(), "got {v}");
                assert!(v["model_absent"].as_str().unwrap().contains("no context character"), "got {v}");
                assert!(message.contains("runs no model"), "{message}");
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }

        let slotless = register_context_with_model(&d, Some("slotless-ctx"), None, PrincipalId::new(), &["musician"]);
        let result = d.dispatch(&[s("context"), s("info")], &caller_with_context(slotless)).await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                let line = v["model_absent"].as_str().expect("a reason");
                assert!(line.contains("has no model for default: ") && line.contains(" has no default slot"), "{line}");
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }
    /// The flip side: once `LlmRegistry::context_window_for` resolves a real
    /// window (mirroring how it's populated from the backend model metadata in
    /// production), `kj context info --json` must compute the percentage
    /// kernel-side from the LAST call's fill — not fabricate it, not leave it
    /// null, and not clamp a result over 100% (this project's standing
    /// preference against unnecessary clamping — a context genuinely over its
    /// window is a fact worth surfacing as-is).
    #[tokio::test]
    async fn context_info_usage_pct_computed_when_window_known() {
        use crate::kj::KjResult;
        let d = test_dispatcher().await;
        let _ = PrincipalId::new();

        // A 100_000-token window on a "mock/big-model" pair — chosen so the
        // fill fractions below (25%, 200%) are exactly representable in
        // binary floating point, ruling out an epsilon-fudged assertion. The
        // window reaches a context through its slot, copied from the
        // model's row when the binding is written.
        let with_window = |label: &str| {
            let character = PrincipalId::new();
            {
                let db = d.kernel_db().lock();
                db.insert_character(&crate::kernel_db::CharacterRow {
                    principal_id: character, name: format!("windowed-{label}"), created_at: 0,
                    retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
                }).unwrap();
                db.seed_test_cast(character, "mock", "big-model", &["default"]).unwrap();
                let backend = db.get_backend_by_name("mock").unwrap().unwrap();
                db.set_backend_model(&crate::kernel_db::BackendModelRow {
                    backend_id: backend.backend_id,
                    model_id: "big-model".into(),
                    context_window: Some(100_000),
                    extra: None,
                    tunables: Default::default(),
                }).unwrap();
            }
            let context = register_context(&d, Some(label), None, character);
            d.kernel_db().lock().assign_stand_in_character(context, character).unwrap();
            context
        };

        // Comfortably under the window: 20_000 + 5_000 = 25_000 / 100_000 = 25%.
        let ctx_under = with_window("usage-window-under");
        {
            let db = d.kernel_db().lock();
            db.set_context_usage(&crate::kernel_db::ContextUsageRow {
                context_id: ctx_under,
                provider: "mock".into(),
                model: "big-model".into(),
                input_tokens: 20_000,
                output_tokens: 5_000,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
                cache_ttl_secs: 0,
                updated_at: kaijutsu_types::now_millis() as i64,
            })
            .unwrap();
        }
        let c_under = caller_with_context(ctx_under);
        let result_under = d.dispatch(&[s("context"), s("info")], &c_under).await;
        assert!(result_under.is_ok(), "info failed: {}", result_under.message());
        assert!(
            result_under.message().contains("100000 window"),
            "human text should show the resolved window: {}",
            result_under.message()
        );
        match result_under {
            KjResult::Ok { data: Some(v), .. } => {
                let usage = &v["usage"];
                assert_eq!(usage["context_window"], 100_000);
                assert_eq!(usage["context_used_pct"], 25.0);
            }
            other => panic!("expected Ok with usage data, got {other:?}"),
        }

        // Past the window: 150_000 + 50_000 = 200_000 / 100_000 = 200% — must
        // be reported as 200, not clamped to 100.
        let ctx_over = with_window("usage-window-over");
        {
            let db = d.kernel_db().lock();
            db.set_context_usage(&crate::kernel_db::ContextUsageRow {
                context_id: ctx_over,
                provider: "mock".into(),
                model: "big-model".into(),
                input_tokens: 150_000,
                output_tokens: 50_000,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
                cache_ttl_secs: 0,
                updated_at: kaijutsu_types::now_millis() as i64,
            })
            .unwrap();
        }
        let c_over = caller_with_context(ctx_over);
        let result_over = d.dispatch(&[s("context"), s("info")], &c_over).await;
        assert!(result_over.is_ok(), "info failed: {}", result_over.message());
        match result_over {
            KjResult::Ok { data: Some(v), .. } => {
                let usage = &v["usage"];
                assert_eq!(usage["context_window"], 100_000);
                assert_eq!(
                    usage["context_used_pct"], 200.0,
                    "over-100% usage must be reported as-is, never clamped"
                );
            }
            other => panic!("expected Ok with usage data, got {other:?}"),
        }
    }

    // ── Track D: cast-on-context ─────────────────────────────────────────

    #[tokio::test]
    async fn context_info_and_list_surface_the_model() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let target = register_context_with_model(&d, Some("staffed"), None, principal, &["default"]);
        let c = caller_with_context(target);

        let info = d.dispatch(&[s("context"), s("info")], &c).await;
        assert!(info.is_ok());
        assert!(info.message().contains("Model:    mock/mock-model"), "info: {}", info.message());

        let list = d.dispatch(&[s("context"), s("list")], &c).await;
        assert!(list.is_ok());
        assert!(list.message().contains("mock/mock-model"), "list: {}", list.message());
        assert!(!list.message().contains("[cast:"), "no cast label is stored: {}", list.message());
    }
    /// `kj kohai create --as <character>` copies the character's cast slots:
    /// the new context's info reports the model, and a character whose cast
    /// has no slot for the type is told so at create.
    ///
    /// Falsified by a create that prints nothing when the context has no slot.
    #[tokio::test]
    async fn kohai_create_copies_the_cast_and_reports_a_missing_slot() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("staffing-parent"), None, principal);
        let c = caller_with_context(parent);
        crate::kj::test_helpers::give_character(&d, &c);
        let performer = d.kernel_db().lock().get_character_by_name("kohai-tester").unwrap().unwrap().principal_id;
        d.kernel_db().lock().seed_test_cast(performer, "mock", "mock-model", &["default"]).unwrap();

        let create = d
            .dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("staffed-child")], &c)
            .await;
        assert!(create.is_ok(), "create failed: {}", create.message());
        assert!(!create.message().contains("has no model"), "a slot exists, so nothing is reported: {}", create.message());

        let child = d.kernel_db().lock().find_context_by_label("staffed-child").unwrap().unwrap().context_id;
        let info = d.dispatch(&[s("context"), s("info")], &caller_with_context(child)).await;
        assert!(info.message().contains("Model:    mock/mock-model"), "info: {}", info.message());
        let KjResult::Ok { data: Some(data), .. } = &info else { panic!("info returns structured data: {info:?}") };
        assert_eq!(data["resolved_model"], "mock-model", "info json: {data}");

        // Another type has no slot in that cast.
        let slotless = d
            .dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("slotless-child"), s("--type"), s("coder")], &c)
            .await;
        assert!(slotless.is_ok(), "create failed: {}", slotless.message());
        assert!(
            slotless.message().contains("has no model for coder: kohai-tester has no coder slot"),
            "{}", slotless.message()
        );
    }

    /// A character with no cast says so at create.
    #[tokio::test]
    async fn kohai_create_says_when_the_character_has_no_cast() {
        let d = test_dispatcher().await;
        let parent = register_context(&d, Some("castless-parent"), None, PrincipalId::new());
        let c = caller_with_context(parent);
        crate::kj::test_helpers::give_character(&d, &c);
        let create = d
            .dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("castless-child")], &c)
            .await;
        assert!(create.is_ok(), "create failed: {}", create.message());
        assert!(
            create.message().contains("has no model for default: kohai-tester has no default slot"),
            "{}", create.message()
        );
    }

    /// `--model`, `-m` and `--cast` are gone from `kj kohai create`, and
    /// `--model`, `--cast` from `kj context set` and `unset`: a model comes
    /// from the character's cast, copied when its context character is written.
    #[tokio::test]
    async fn the_model_and_cast_flags_are_gone_from_create_set_and_unset() {
        let d = test_dispatcher().await;
        let parent = register_context(&d, Some("flag-parent"), None, PrincipalId::new());
        let c = caller_with_context(parent);
        crate::kj::test_helpers::give_character(&d, &c);
        for argv in [
            vec![s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("a"), s("--model"), s("x")],
            vec![s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("a"), s("-m"), s("x")],
            vec![s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("a"), s("--cast"), s("x")],
            vec![s("context"), s("set"), s("--model"), s("x")],
            vec![s("context"), s("set"), s("--cast"), s("x")],
            vec![s("context"), s("unset"), s("--model")],
            vec![s("context"), s("unset"), s("--cast")],
        ] {
            let result = d.dispatch(&argv, &c).await;
            assert!(!result.is_ok(), "{argv:?} must be refused");
            assert!(result.message().contains("unexpected argument"), "{argv:?}: {}", result.message());
        }
        assert!(d.kernel_db().lock().find_context_by_label("a").unwrap().is_none());
    }
    #[tokio::test]
    async fn context_create_name_alias() {
        // `--name` / `-n` is accepted as an alias for the positional label,
        // matching `kj fork`.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);

        let c = caller_with_context(parent);

        crate::kj::test_helpers::give_character(&d, &c);
        let result = d
            .dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("--name"), s("aliased")], &c)
            .await;
        assert!(result.is_ok(), "create --name failed: {}", result.message());

        let db = d.kernel_db().lock();
        assert!(
            db.resolve_context("aliased").is_ok(),
            "context created via --name should resolve by label"
        );
    }

    #[tokio::test]
    async fn context_create_with_cwd_and_env() {
        let d = wired_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);
        mount_scratch_work(&d).await;

        let c = caller_with_context(parent);

        crate::kj::test_helpers::give_character(&d, &c);
        let result = d
            .dispatch(
                &[
                    s("kohai"), s("create"), s("--as"), s("kohai-tester"),
                    s("kid"),
                    s("--cwd"),
                    s("/scratch/work"),
                    s("--env"),
                    s("RUST_LOG=debug"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        let id = {
            let db = d.kernel_db().lock();
            db.resolve_context("kid").expect("kid should exist")
        };
        let db = d.kernel_db().lock();
        let shell = db.get_context_shell(id).unwrap().unwrap();
        assert_eq!(shell.cwd, Some("/scratch/work".into()));
        let env = db.get_context_env(id).unwrap();
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].key, "RUST_LOG");
        assert_eq!(env[0].value, "debug");
    }

    fn set_cwd(d: &crate::kj::KjDispatcher, context: ContextId, cwd: &str) {
        d.kernel_db().lock().upsert_context_shell(&crate::kernel_db::ContextShellRow {
            context_id: context, cwd: Some(cwd.into()), updated_at: 0,
        }).unwrap();
    }

    fn cwd_of(d: &crate::kj::KjDispatcher, label: &str) -> Option<String> {
        let db = d.kernel_db().lock();
        let id = db.resolve_context(label).unwrap_or_else(|e| panic!("{label} should exist: {e}"));
        db.get_context_shell(id).unwrap().and_then(|shell| shell.cwd)
    }

    /// A create without `--cwd` starts where the caller stands, as `kj fork`
    /// does; `--cwd` names another directory. The caller's context is the
    /// source even when `--parent` places the new context elsewhere.
    #[tokio::test]
    async fn context_create_inherits_the_callers_cwd() {
        let d = wired_dispatcher().await;
        let principal = PrincipalId::new();
        let caller = register_context(&d, Some("caller"), None, principal);
        let elsewhere = register_context(&d, Some("elsewhere"), None, principal);
        mount_scratch_work(&d).await;
        set_cwd(&d, caller, "/scratch/work");
        set_cwd(&d, elsewhere, "/scratch");
        let c = caller_with_context(caller);
        crate::kj::test_helpers::give_character(&d, &c);

        let result = d.dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("kid")], &c).await;
        assert!(result.is_ok(), "create failed: {}", result.message());
        assert_eq!(cwd_of(&d, "kid").as_deref(), Some("/scratch/work"));
        assert!(result.message().contains("cwd=/scratch/work"), "the result names the cwd: {}", result.message());

        let result = d.dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("placed"), s("--parent"), s("elsewhere")], &c).await;
        assert!(result.is_ok(), "create failed: {}", result.message());
        assert_eq!(cwd_of(&d, "placed").as_deref(), Some("/scratch/work"), "the caller's cwd, not the parent's");

        let result = d.dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("named"), s("--cwd"), s("/scratch")], &c).await;
        assert!(result.is_ok(), "create failed: {}", result.message());
        assert_eq!(cwd_of(&d, "named").as_deref(), Some("/scratch"));
    }

    /// `--cwd` accepts what `cd` enters, including the shell's own
    /// `/v/docs`, which is not on the kernel mount table, and a create from
    /// such a context inherits it.
    #[tokio::test]
    async fn context_cwd_accepts_what_cd_enters() {
        let d = wired_dispatcher().await;
        let caller = register_context(&d, Some("caller"), None, PrincipalId::new());
        let c = caller_with_context(caller);
        crate::kj::test_helpers::give_character(&d, &c);
        let result = d.dispatch(&[s("context"), s("set"), s("."), s("--cwd"), s("/v/docs")], &c).await;
        assert!(result.is_ok(), "set --cwd /v/docs: {}", result.message());
        assert_eq!(cwd_of(&d, "caller").as_deref(), Some("/v/docs"));
        let result = d.dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("kid")], &c).await;
        assert!(result.is_ok(), "create from a /v/docs cwd: {}", result.message());
        assert_eq!(cwd_of(&d, "kid").as_deref(), Some("/v/docs"));
    }

    #[tokio::test]
    async fn context_create_from_a_context_without_a_cwd_sets_none() {
        let d = test_dispatcher().await;
        let caller = register_context(&d, Some("caller"), None, PrincipalId::new());
        let creator = caller_with_context(caller);
        crate::kj::test_helpers::give_character(&d, &creator);
        let result = d.dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("kid")], &creator).await;
        assert!(result.is_ok(), "create failed: {}", result.message());
        assert_eq!(cwd_of(&d, "kid"), None);
    }

    /// An inherited cwd that no longer resolves refuses the create and
    /// names `--cwd`, rather than creating a context in a missing directory.
    #[tokio::test]
    async fn context_create_refuses_an_inherited_cwd_that_is_gone() {
        let d = wired_dispatcher().await;
        let caller = register_context(&d, Some("caller"), None, PrincipalId::new());
        mount_scratch_work(&d).await;
        set_cwd(&d, caller, "/scratch/gone");
        let before = d.kernel_db().lock().list_active_contexts().unwrap().len();
        let creator = caller_with_context(caller);
        crate::kj::test_helpers::give_character(&d, &creator);
        let result = d.dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("kid")], &creator).await;
        assert!(!result.is_ok(), "an unavailable inherited cwd must refuse: {}", result.message());
        assert!(result.message().contains("current context's cwd '/scratch/gone'") && result.message().contains("--cwd DIR"), "msg: {}", result.message());
        assert_eq!(d.kernel_db().lock().list_active_contexts().unwrap().len(), before, "no context is created");
    }

    /// `kj kohai create` has no `--egress-allow`/`--egress-deny` of its
    /// own — a freshly created context's egress list starts empty
    /// (`docs/egress.md`, "Who changes the list": "`kj kohai create`
    /// starts with an empty list").
    #[tokio::test]
    async fn context_create_starts_with_an_empty_egress_list() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("egress-create-parent"), None, principal);

        let c = caller_with_context(parent);

        crate::kj::test_helpers::give_character(&d, &c);
        let result = d
            .dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("egress-create-kid")], &c)
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        let id = {
            let db = d.kernel_db().lock();
            db.resolve_context("egress-create-kid").expect("kid should exist")
        };
        assert!(d.kernel_db().lock().list_context_egress(id).unwrap().is_empty());
    }

    /// A coder context's rc create lifecycle may widen the egress list of the
    /// context it is creating, when the creator holds the authority
    /// `docs/egress.md` names. The root character `solo` plays the root
    /// context and creates a `coder` context under it, performed by
    /// `coder`. The rc shell's `kj` acts as the creator, which is the new
    /// context's lineage root; a change to who rc acts as fails here.
    /// Afterwards the context's own shell builds `curl` with that list, so
    /// a request to a non-loopback name gets past the egress check and
    /// fails at name resolution instead (`.invalid` never resolves).
    #[test]
    fn a_create_script_may_open_egress_on_the_context_it_creates() {
        crate::on_rc_thread(a_create_script_may_open_egress_on_the_context_it_creates_body);
    }

    async fn a_create_script_may_open_egress_on_the_context_it_creates_body() {
        let d = std::sync::Arc::new(test_dispatcher_rc().await);
        d.set_self_arc();
        let solo = PrincipalId::new();
        let coder = PrincipalId::new();
        for (principal_id, name, root) in [(solo, "solo", true), (coder, "coder", false)] {
            d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
                principal_id, name: name.into(), created_at: 0, retired_at: None, handoff_ctx: None, root_ctx: None, root,
            }).unwrap();
        }
        let root = register_context(&d, Some("solo"), None, solo);
        d.kernel_db().lock().assign_test_review(root, Some(solo), None).unwrap();
        install_rc_script_file(
            &d,
            "/config/rc/coder/create/S50-egress.kai",
            "set -e\nkj context set . --egress-allow '*'\n",
        )
        .await;

        let solo_call = crate::kj::KjCaller {
            principal_id: solo, actor_id: solo, reviewer_id: None, context_id: Some(root),
            session_id: kaijutsu_types::SessionId::new(), confirmed: false, rc_depth: 0, privileged: false,
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        let created = d
            .dispatch(&[s("kohai"), s("create"), s("bench"), s("--type"), s("coder"), s("--as"), s("coder")], &solo_call)
            .await;
        assert!(created.is_ok(), "{}", created.message());
        let kid = d.kernel_db().lock().resolve_context("bench").expect("the coder context exists");
        assert_eq!(d.kernel_db().lock().lineage_root(kid).unwrap(), solo, "solo is the new context's lineage root");

        let errors: Vec<String> = d
            .block_store()
            .block_snapshots(kid)
            .unwrap()
            .into_iter()
            .filter(|b| b.kind == kaijutsu_types::BlockKind::Error)
            .map(|b| b.content)
            .collect();
        assert!(
            !errors.iter().any(|e| e.contains("S50-egress")),
            "the egress script must run without an error block: {errors:#?}"
        );
        assert_eq!(d.kernel_db().lock().list_context_egress(kid).unwrap(), vec!["*".to_string()]);

        let info = d.dispatch(&[s("context"), s("info"), s("bench")], &solo_call).await;
        assert!(info.is_ok(), "{}", info.message());
        assert!(info.message().contains("\nEgress:\n  *"), "kj context info shows the egress list:\n{}", info.message());

        let shell = crate::runtime::embedded_kaish::EmbeddedKaish::for_context(
            &d,
            "bench-egress",
            crate::runtime::context_shell::ShellIdentity {
                requester: solo, performer: coder, reviewer: Some(solo), context: kid,
                session: kaijutsu_types::SessionId::new(),
            },
            crate::runtime::context_shell::ShellPolicy::Agent,
            crate::runtime::context_shell::ShellCwd::Context,
            None,
            std::sync::Arc::new(crate::runtime::synthesis::NoopBlockSource),
        )
        .await
        .expect("build the coder context's shell");
        let r = shell
            .execute_with_options("curl --max-time 5 https://kaijutsu-egress-probe.invalid/", kaish_kernel::ExecuteOptions::default())
            .await
            .unwrap();
        assert!(!r.ok(), "an .invalid name never resolves: {}", r.err);
        assert!(
            !r.err.contains("egress allowlist"),
            "curl must get past the egress check and fail later: {}",
            r.err
        );
    }

    #[tokio::test]
    async fn context_create_musician_arms_the_beat() {
        // A musician created via `kj` MUST arm the beat scheduler. The OODA Act
        // (ABC→cell→MIDI) never fires for an un-armed context and
        // `kj transport play` is silently ignored ("play on un-armed context").
        // The arm is now driven by the musician's `create/` rc
        // (`S20-arm.kai` → `kj transport arm`), NOT a Rust `== "musician"` branch
        // — so this exercises the rc end-to-end: `set_self_arc` is required for
        // `kj`-inside-rc to resolve (else the rc kaish falls back to bare kaish
        // and the arm command never fires). A non-musician runs no arm rc.
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);

        // Own the only beat ingress so we can observe what create sends. Dispatch
        // now AWAITs the scheduler's ack, so stub a replier that acks Ok and
        // forwards each command for inspection — else create's arm rc would hang.
        let (tx, mut ingress) =
            tokio::sync::mpsc::unbounded_channel::<crate::hyoushigi::BeatRequest>();
        assert!(d.kernel().set_beat_ingress(tx), "test owns the ingress");
        let (cmd_tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(req) = ingress.recv().await {
                match req {
                    crate::hyoushigi::BeatRequest::Command { command, reply } => {
                        let _ = cmd_tx.send(command);
                        if let Some(reply) = reply {
                            let _ = reply.send(Ok(None));
                        }
                    }
                    crate::hyoushigi::BeatRequest::Snapshot { reply } => {
                        let _ = reply.send(Vec::new());
                    }
                    // The stub holds no attachments; a capture commit refuses.
                    crate::hyoushigi::BeatRequest::CommitCapture { reply, .. } => {
                        let _ = reply.send(Err("beat stub: no tracks".into()));
                    }
                    // Fire-and-forget; the stub has no clocks to slave.
                    crate::hyoushigi::BeatRequest::ClockEstimate { .. } => {}
                }
            }
        });

        let c = caller_with_context(parent);

        crate::kj::test_helpers::give_character(&d, &c);

        // Default type: no beat command.
        let r = d
            .dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("plain")], &c)
            .await;
        assert!(r.is_ok(), "default create failed: {}", r.message());
        assert!(
            rx.try_recv().is_err(),
            "a non-musician context must not arm the beat"
        );

        // Musician type: arms, with a track derived from the label.
        let r = d
            .dispatch(
                &[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("bassline"), s("--type"), s("musician")],
                &c,
            )
            .await;
        assert!(r.is_ok(), "musician create failed: {}", r.message());

        let id = {
            let db = d.kernel_db().lock();
            db.resolve_context("bassline").expect("bassline exists")
        };
        let expected_track = kaijutsu_types::TrackId::new("bassline")
            .ok()
            .or_else(|| kaijutsu_types::TrackId::slugify("bassline"))
            .expect("bassline yields a valid track");
        match rx.recv().await {
            Some(crate::hyoushigi::BeatCommand::Attach {
                context_id, track, ..
            }) => {
                assert_eq!(context_id, id, "attaches the musician we just created");
                assert_eq!(track, expected_track, "track derives from the label");
            }
            other => panic!("musician create must send BeatCommand::Attach, got {other:?}"),
        }
    }

    /// `kj context list` must emit a JSON array of resolver-friendly handles
    /// (labels preferred, short-id fallback) so kaish for-loops iterate them.
    #[tokio::test]
    async fn context_list_emits_handle_array() {
        use crate::kj::KjResult;
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let _alpha = register_context(&d, Some("alpha"), None, principal);
        let unlabeled = register_context(&d, None, None, principal);
        let caller = caller_with_context(unlabeled);

        let result = d.dispatch(&[s("context"), s("list")], &caller).await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                let arr = v.as_array().expect("data must be a JSON array");
                let handles: Vec<&str> = arr.iter().filter_map(|x| x.as_str()).collect();
                assert!(
                    handles.contains(&"alpha"),
                    "labeled context should appear by label: {handles:?}"
                );
                let full_hex = unlabeled.to_hex();
                assert!(
                    handles.iter().any(|h| *h == full_hex),
                    "unlabeled context should fall back to full hex ({full_hex}): {handles:?}",
                );
                assert!(
                    !handles.iter().any(|h| *h == unlabeled.short()),
                    "must NOT use short prefix when full hex is required: {handles:?}",
                );
            }
            other => panic!("expected Ok with array data, got {other:?}"),
        }
    }

    // ── kj context prompt ─────────────────────────────────────────────

    /// The director stance names its senpai from the kernel fact (`kj senpai
    /// show`), says "our senpai", and points reports at `kj senpai report`
    /// (`docs/senpai-kohai.md`, "What a context sees").
    #[test]
    fn director_stance_names_its_senpai_from_the_kernel_fact() {
        crate::on_rc_thread(director_stance_names_its_senpai_from_the_kernel_fact_body);
    }

    async fn director_stance_names_its_senpai_from_the_kernel_fact_body() {
        let d = std::sync::Arc::new(test_dispatcher_rc().await);
        d.set_self_arc();
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("senpai-seat"), None, principal);
        let create_caller = caller_with_context(parent);
        let banto = PrincipalId::new();
        for (principal_id, name) in [(create_caller.actor_id, "sensei"), (banto, "banto")] {
            d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
                principal_id, name: s(name), created_at: 0,
                retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
            }).unwrap();
        }
        d.kernel_db().lock().update_played_by(parent, Some(create_caller.actor_id)).unwrap();
        let create = d
            .dispatch(&[s("kohai"), s("create"), s("banto-seat"), s("--type"), s("director"), s("--as"), s("banto")], &create_caller)
            .await;
        assert!(create.is_ok(), "create failed: {}", create.message());
        let ctx = d.kernel_db().lock().resolve_context("banto-seat").expect("banto-seat exists");
        let result = d.dispatch(&[s("context"), s("prompt"), ctx.to_hex()], &caller_with_context(ctx)).await;
        assert!(result.is_ok(), "prompt failed: {}", result.message());
        let flat = result.message().split_whitespace().collect::<Vec<_>>().join(" ");
        for said in [
            "In this seat we are banto, the operator of this Kaijutsu kernel. Our senpai is sensei, in senpai-seat.",
            "Our own asks go to our senpai",
            "kj senpai report",
        ] {
            assert!(flat.contains(said), "director stance is missing {said:?}: {flat}");
        }
        assert!(!flat.contains("We are the reviewer for the contexts we create"), "{flat}");
    }

    #[tokio::test]
    async fn system_prompt_read_failure_reaches_preview() {
        let d = test_dispatcher().await;
        let context_id = register_context(&d, Some("missing-document"), None, PrincipalId::new());
        let result = d.dispatch(&[s("context"), s("prompt")], &caller_with_context(context_id)).await;
        assert!(!result.is_ok(), "{}", result.message());
        assert!(result.message().contains("instruction blocks"), "{}", result.message());
    }

    /// Context types choose their prompt sections through rc. The opted-in
    /// coder gets the shared base before its role section; musician, which has
    /// no link, gets none. A legacy kernel config file must affect neither.
    // Creating a coder runs its create rc; use the production rc stack.
    #[test]
    fn context_prompt_uses_rc_owned_sections() {
        crate::on_rc_thread(context_prompt_uses_rc_owned_sections_body);
    }

    async fn context_prompt_uses_rc_owned_sections_body() {
        use crate::vfs::VfsOps;

        let d = std::sync::Arc::new(test_dispatcher_rc().await);
        d.set_self_arc();
        d.kernel()
            .vfs()
            .write_all(
                std::path::Path::new("/config/kernel/system.md"),
                b"LEGACY-KERNEL-PROMPT-MUST-NOT-APPEAR",
            )
            .await
            .expect("write legacy kernel prompt sentinel");
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);
        let create_caller = caller_with_context(parent);

        d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
            principal_id: create_caller.actor_id, name: s("amy"), created_at: 0,
            retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
        }).unwrap();
        d.kernel_db().lock().assign_stand_in_character(parent, create_caller.actor_id).unwrap();

        let create = d
            .dispatch(
                &[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("c1"), s("--type"), s("coder")],
                &create_caller,
            )
            .await;
        assert!(create.is_ok(), "create failed: {}", create.message());
        let ctx = {
            let db = d.kernel_db().lock();
            db.resolve_context("c1").expect("c1 should exist")
        };

        let c = caller_with_context(ctx);
        let result = d.dispatch(&[s("context"), s("prompt")], &c).await;
        assert!(result.is_ok(), "prompt failed: {}", result.message());
        let msg = result.message();

        let opening = "We are a cybernetic system focused on software engineering.";
        assert_eq!(
            msg.match_indices(opening).count(),
            1,
            "the coder receives its stance exactly once: {msg}"
        );
        assert!(
            !msg.contains("LEGACY-KERNEL-PROMPT-MUST-NOT-APPEAR"),
            "the old /config/kernel/system.md input must be ignored: {msg}"
        );
        // The coder stance carries accountability, the define-done and
        // orient rhythm, honest guessing, the working rules, and done.
        for rule in ["The human in our system is accountable for our work",
            "how we will know when it is complete", "We begin each task by getting oriented",
            "The most important thing to do when guessing is say so",
            "Search file contents with grep", "least privilege",
            "kj wait --operation", "Use the `done` tool", "fair & square", "頑張って"] {
            // Prose wraps freely; compare with line breaks folded to spaces.
            let flat = msg.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(flat.contains(rule), "coder stance is missing {rule:?}: {msg}");
        }

        // Order is the rc sort order: coder S00-stance, then the situation.
        let rc_pos = msg.find(opening).expect("rc present");
        let situation_pos = msg.find("<situation>").expect("situation present");
        assert!(
            rc_pos < situation_pos,
            "expected role -> situation order; got rc={rc_pos}, situation={situation_pos}\nfull:\n{msg}"
        );

        let create = d
            .dispatch(
                &[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("m1"), s("--type"), s("musician")],
                &create_caller,
            )
            .await;
        assert!(create.is_ok(), "musician create failed: {}", create.message());
        let musician = {
            let db = d.kernel_db().lock();
            db.resolve_context("m1").expect("m1 should exist")
        };
        let result = d
            .dispatch(&[s("context"), s("prompt")], &caller_with_context(musician))
            .await;
        assert!(result.is_ok(), "musician prompt failed: {}", result.message());
        let musician_prompt = result.message();
        assert!(
            !musician_prompt.contains(opening),
            "the musician receives no coder stance: {musician_prompt}"
        );
        assert!(
            musician_prompt.contains("You're a musician here"),
            "musician's own role section is still present: {musician_prompt}"
        );
    }

    /// Rc source files are read when a lifecycle runs. Editing an rc source body
    /// changes a later create, never the already-authored instruction blocks
    /// in an existing context.
    // Creating a coder runs its create rc; use the production rc stack.
    #[test]
    fn rc_source_edit_applies_only_to_later_lifecycle_runs() {
        crate::on_rc_thread(rc_source_edit_applies_only_to_later_lifecycle_runs_body);
    }

    async fn rc_source_edit_applies_only_to_later_lifecycle_runs_body() {
        use crate::vfs::VfsOps;

        let d = std::sync::Arc::new(test_dispatcher_rc().await);
        d.set_self_arc();
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);
        let caller = caller_with_context(parent);
        crate::kj::test_helpers::give_character(&d, &caller);

        let create = d
            .dispatch(
                &[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("before"), s("--type"), s("coder")],
                &caller,
            )
            .await;
        assert!(create.is_ok(), "first create failed: {}", create.message());
        let before = {
            let db = d.kernel_db().lock();
            db.resolve_context("before").expect("before context")
        };
        let before_sections = d
            .block_store()
            .block_snapshots(before)
            .map(|blocks| crate::llm::extract_system_prompt_sections(&blocks))
            .expect("before snapshots");
        assert!(
            before_sections
                .iter()
                .any(|section| section.contains("We are a cybernetic system focused on software engineering.")),
            "first context must receive the seeded coder stance: {before_sections:?}"
        );

        d.kernel()
            .vfs()
            .write_all(
                std::path::Path::new("/config/rc/coder/create/S00-stance.md"),
                b"CHANGED-STANCE-FOR-LATER-CREATE",
            )
            .await
            .expect("edit coder rc source");

        let unchanged_sections = d
            .block_store()
            .block_snapshots(before)
            .map(|blocks| crate::llm::extract_system_prompt_sections(&blocks))
            .expect("before snapshots after source edit");
        assert_eq!(
            unchanged_sections, before_sections,
            "editing an rc source must not mutate an existing context's stored sections"
        );

        let create = d
            .dispatch(
                &[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("after"), s("--type"), s("coder")],
                &caller,
            )
            .await;
        assert!(create.is_ok(), "second create failed: {}", create.message());
        let after = {
            let db = d.kernel_db().lock();
            db.resolve_context("after").expect("after context")
        };
        let after_sections = d
            .block_store()
            .block_snapshots(after)
            .map(|blocks| crate::llm::extract_system_prompt_sections(&blocks))
            .expect("after snapshots");
        assert!(
            after_sections
                .iter()
                .any(|section| section.contains("CHANGED-STANCE-FOR-LATER-CREATE")),
            "a later lifecycle run must read the edited source: {after_sections:?}"
        );
    }

    /// The `<situation>` names the context's instance and the instance's senpai
    /// beside its performer, by conversation label and character
    /// (`docs/senpai-kohai.md`, "What a context sees"). It replaces
    /// `<reviewer>`. A root console has no senpai, so no element.
    #[tokio::test]
    async fn context_prompt_situation_names_the_senpai() {
        let d = test_dispatcher().await;
        let amy = test_reviewer_principal();
        let root = register_root_context(&d);
        let banto = PrincipalId::new();
        let coder = PrincipalId::new();
        for (principal_id, name) in [(banto, "banto"), (coder, "coder")] {
            d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
                principal_id, name: name.into(), created_at: 0, retired_at: None,
                handoff_ctx: None, root_ctx: None, root: false,
            }).unwrap();
        }
        let seat = register_context(&d, Some("banto-seat"), Some(root), amy);
        let lane = register_context(&d, Some("lane"), Some(seat), amy);
        for (ctx, who) in [(seat, banto), (lane, coder)] {
            d.kernel_db().lock().update_played_by(ctx, Some(who)).unwrap();
        }
        for ctx in [root, seat, lane] {
            let _ = d.block_store().create_document(ctx, crate::DocumentKind::Conversation, None);
        }
        let situation = |ctx: ContextId| {
            let d = &d;
            async move {
                let result = d.dispatch(&[s("context"), s("prompt"), ctx.to_hex()], &caller_with_context(ctx)).await;
                assert!(result.is_ok(), "prompt failed: {}", result.message());
                match result {
                    KjResult::Ok { data: Some(v), .. } => v["situation"].as_str().unwrap().to_string(),
                    other => panic!("expected data: {other:?}"),
                }
            }
        };
        let lane_situation = situation(lane).await;
        assert!(lane_situation.contains("<character name=\"coder\"/>"), "{lane_situation}");
        assert!(
            lane_situation.contains(&format!("<senpai id=\"{}\" context=\"banto-seat\" character=\"banto\"/>", seat.short())),
            "{lane_situation}"
        );
        assert!(lane_situation.contains("name=\"coder\""), "the performer stays beside it: {lane_situation}");
        assert!(!lane_situation.contains("<reviewer"), "{lane_situation}");
        let seat_situation = situation(seat).await;
        assert!(
            seat_situation.contains(&format!("<senpai id=\"{}\" context=\"amy\" character=\"amy\"/>", root.short())),
            "{seat_situation}"
        );
        let root_situation = situation(root).await;
        assert!(!root_situation.contains("<senpai"), "a root console has no senpai: {root_situation}");
        assert!(root_situation.contains("<character name=\"amy\"/>"), "{root_situation}");

        // The edge is the context character's: moving it changes the fact.
        d.kernel_db().lock().set_context_senpai(lane, Some(root)).unwrap();
        let moved = situation(lane).await;
        assert!(
            moved.contains(&format!("<senpai id=\"{}\" context=\"amy\" character=\"amy\"/>", root.short())),
            "{moved}"
        );
    }

    /// `kj context info` names the context's senpai and its character; a
    /// context with no context character shows neither.
    #[tokio::test]
    async fn context_info_shows_the_senpai_of_a_context_character() {
        let d = test_dispatcher().await;
        let amy = test_reviewer_principal();
        let root = register_root_context(&d);
        let coder = PrincipalId::new();
        d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
            principal_id: coder, name: "coder".into(), created_at: 0, retired_at: None,
            handoff_ctx: None, root_ctx: None, root: false,
        }).unwrap();
        let lane = register_context(&d, Some("lane"), Some(root), amy);
        let fork = register_context(&d, Some("lane-fork"), Some(lane), amy);
        register_context(&d, Some("bare"), None, amy);
        {
            let db = d.kernel_db().lock();
            db.update_played_by(lane, Some(coder)).unwrap();
            db.copy_context_character(lane, fork).unwrap();
        }
        let info = d.dispatch(&[s("context"), s("info"), s("lane-fork")], &caller_with_context(lane)).await;
        assert!(info.is_ok(), "{}", info.message());
        assert!(info.message().contains(&format!("Senpai: lane ({}), coder", lane.short())), "{}", info.message());
        let KjResult::Ok { data: Some(data), .. } = &info else { panic!("expected data") };
        assert_eq!(data["played_by_name"], "coder");
        assert_eq!(data["senpai"]["character"], "coder");
        assert_eq!(data["senpai"]["id"], lane.to_hex());
        let lane_info = d.dispatch(&[s("context"), s("info"), s("lane")], &caller_with_context(lane)).await;
        let KjResult::Ok { data: Some(lane_data), .. } = &lane_info else { panic!("expected data") };
        assert_eq!(lane_data["senpai"]["character"], "amy");

        let bare_info = d.dispatch(&[s("context"), s("info"), s("bare")], &caller_with_context(lane)).await;
        assert!(!bare_info.message().contains("Senpai:"), "{}", bare_info.message());
        let KjResult::Ok { data: Some(bare_data), .. } = &bare_info else { panic!("expected data") };
        assert!(bare_data["senpai"].is_null());
    }

    /// `--json`'s `data.prompt` must be exactly the string rendered in the
    /// human message (the header line precedes it) — no second assembly
    /// path that could quietly drift from what the human sees. Also checks
    /// the selected `rc_sections` piece the caller can inspect without
    /// re-running the assembly.
    #[tokio::test]
    async fn context_prompt_json_matches_human_render() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("promptctx"), None, principal);
        d.block_store()
            .create_document(ctx, crate::DocumentKind::Conversation, None)
            .unwrap();
        d.block_store()
            .insert_block_as(
                ctx,
                None,
                None,
                kaijutsu_types::Role::System,
                kaijutsu_types::BlockKind::Text,
                "You are a terse test stance.".to_string(),
                kaijutsu_types::Status::Done,
                kaijutsu_types::ContentType::Markdown,
                Some(principal),
            )
            .unwrap();

        let c = caller_with_context(ctx);
        let result = d.dispatch(&[s("context"), s("prompt")], &c).await;
        assert!(result.is_ok(), "prompt failed: {}", result.message());
        let msg = result.message().to_string();
        assert!(
            msg.contains("promptctx"),
            "header should show the context label: {msg}"
        );

        match result {
            KjResult::Ok { data: Some(v), .. } => {
                let prompt = v["prompt"].as_str().expect("data.prompt is a string");
                assert!(
                    msg.contains(prompt),
                    "human render must contain the exact data.prompt string;\nmsg={msg}\nprompt={prompt}"
                );
                assert_eq!(
                    v["char_count"].as_u64(),
                    Some(prompt.chars().count() as u64),
                    "char_count must match the assembled prompt, not the wrapped message"
                );

                assert!(
                    v.get("base").is_none(),
                    "the preview must not imply a kernel-wide base: {v}"
                );
                assert!(
                    prompt.starts_with("You are a terse test stance."),
                    "prompt must start with the context's selected section: {prompt}"
                );

                let rc_sections = v["rc_sections"].as_array().expect("data.rc_sections is an array");
                assert_eq!(
                    rc_sections.as_slice(),
                    &[serde_json::json!("You are a terse test stance.")],
                    "rc_sections must carry the seeded rc block content verbatim"
                );
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    /// An explicit context ref must render THAT context's prompt, not the
    /// caller's currently-joined one — the same default-to-current pattern
    /// every other `context <verb> [<ctx>]` follows (see `context_info`).
    #[tokio::test]
    async fn context_prompt_resolves_explicit_ref_not_current() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let current = register_context(&d, Some("current-ctx"), None, principal);
        let other = register_context(&d, Some("other-ctx"), None, principal);

        for (ctx, marker) in [(current, "current marker"), (other, "other marker")] {
            d.block_store()
                .create_document(ctx, crate::DocumentKind::Conversation, None)
                .unwrap();
            d.block_store()
                .insert_block_as(
                    ctx,
                    None,
                    None,
                    kaijutsu_types::Role::System,
                    kaijutsu_types::BlockKind::Text,
                    marker.to_string(),
                    kaijutsu_types::Status::Done,
                    kaijutsu_types::ContentType::Markdown,
                    Some(principal),
                )
                .unwrap();
        }

        let c = caller_with_context(current);
        let result = d
            .dispatch(&[s("context"), s("prompt"), s("other-ctx")], &c)
            .await;
        assert!(result.is_ok(), "prompt failed: {}", result.message());
        let msg = result.message();
        assert!(
            msg.contains("other marker"),
            "explicit ref must render the target context's rc section: {msg}"
        );
        assert!(
            !msg.contains("current marker"),
            "explicit ref must NOT leak the caller's current context: {msg}"
        );
    }

    /// Regression: staging half of `kj context prompt` matching the turn
    /// path. The turn path (`spawn_llm_for_prompt`) refuses a `Staging`
    /// context outright; the preview must refuse the same way instead of
    /// rendering a prompt that could never actually run.
    #[tokio::test]
    async fn context_prompt_refuses_staging_context_like_turn_path() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("stagingctx"), None, principal);
        d.block_store()
            .create_document(ctx, crate::DocumentKind::Conversation, None)
            .unwrap();
        {
            let mut drift = d.drift_router().write();
            drift.set_state(ctx, ContextState::Staging).unwrap();
        }

        let c = caller_with_context(ctx);
        let result = d.dispatch(&[s("context"), s("prompt")], &c).await;
        assert!(
            !result.is_ok(),
            "a staging context's prompt must be refused, matching the turn path: {}",
            result.message()
        );
        assert!(
            result.message().contains("staging"),
            "refusal should name staging as the reason: {}",
            result.message()
        );
    }
}

#[cfg(test)]
mod rebind_tests {
    //! `kj context rebind` — the repair half of the create-time rc failure.
    //! Creation deliberately still succeeds when its rc lifecycle fails
    //! (Amy, 2026-08-12: a fresh context holds nothing worth
    //! saving, and aborting would destroy the Error blocks that explain the
    //! failure), so this verb has to be able to fix the result afterwards.

    use crate::kj::test_helpers::*;
    use kaijutsu_types::PrincipalId;

    fn s(v: &str) -> String {
        v.to_string()
    }

    /// **The decision this test exists to protect.** `rebind` must not be
    /// Operator-gated: that would put the repair behind a capability the broken
    /// context cannot hold, which is precisely the lockout it undoes. Here the
    /// caller *is* the unbound context, and the refusal it gets must be about
    /// the outcome, never about authorization.
    #[tokio::test]
    async fn rebind_is_ungated_so_an_unbound_context_can_invoke_it() {
        let d = test_dispatcher().await;
        let ctx = register_context(&d, Some("inert"), None, PrincipalId::new());
        d.kernel_db()
            .lock()
            .delete_context_binding(ctx)
            .expect("delete the binding row");

        let result = d
            .dispatch(&[s("context"), s("rebind")], &caller_with_context(ctx))
            .await;

        // This dispatcher has no rc scripts, so the repair cannot actually
        // succeed — the point is *which* failure comes back.
        let msg = result.message();
        assert!(
            !msg.contains("denied"),
            "rebind must never be capability-gated: {msg}"
        );
        assert!(
            msg.contains("still has no loadout"),
            "the refusal should report the outcome: {msg}"
        );
    }

    /// The happy path: an unbound context re-runs its `create` lifecycle and
    /// comes back bound. The rc script is what grants — the caller never picks.
    #[tokio::test]
    async fn rebind_repairs_an_unbound_context_by_rerunning_create() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc(); // .kai scripts reach the `kj` builtin through this
        // Production wiring: the broker persists `set_binding` through to
        // KernelDb, which is the authoritative store `has_usable_loadout` and
        // `require_cap` read. `test_dispatcher` leaves that handle unset, so
        // without this the rc script's `kj binding allow` would touch only the
        // in-memory cache and the repair could never be observed.
        d.kernel().broker().set_db(d.kernel_db().clone()).await;
        install_rc_script_file(
            &d,
            "/config/rc/test/create/S10-binding.kai",
            "kj binding allow operator",
        )
        .await;

        let ctx = register_context(&d, Some("broken"), None, PrincipalId::new());
        d.kernel_db()
            .lock()
            .update_context_type(ctx, "test")
            .expect("update_context_type");
        d.kernel_db()
            .lock()
            .delete_context_binding(ctx)
            .expect("delete the binding row");
        assert_eq!(
            d.has_usable_loadout(ctx),
            Ok(false),
            "precondition: the context starts inert"
        );

        let result = d
            .dispatch(&[s("context"), s("rebind")], &caller_with_context(ctx))
            .await;
        let blocks: Vec<String> = d
            .block_store()
            .block_snapshots(ctx)
            .unwrap_or_default()
            .into_iter()
            .map(|b| b.content)
            .collect();
        assert!(
            result.is_ok(),
            "rebind should repair: {} (blocks: {blocks:?})",
            result.message()
        );
        assert_eq!(
            d.has_usable_loadout(ctx),
            Ok(true),
            "the rc create lifecycle should have bound it"
        );
    }

    /// Guarding on the outcome is what keeps this a repair instead of a general
    /// "re-run rc" button: `create` scripts append stances and arm beats, and
    /// are not idempotent.
    #[tokio::test]
    async fn rebind_refuses_a_context_that_already_has_a_loadout() {
        let d = test_dispatcher().await;
        let ctx = register_context(&d, Some("warm"), None, PrincipalId::new());

        let result = d
            .dispatch(&[s("context"), s("rebind")], &caller_with_context(ctx))
            .await;
        assert!(!result.is_ok(), "a bound context has nothing to repair");
        assert!(
            result.message().contains("already has a loadout"),
            "refusal should say why: {}",
            result.message()
        );
    }

    /// Archived contexts are retained work, inert by design — not a repair
    /// target. (`archived_at` is authoritative for archived-ness.)
    #[tokio::test]
    async fn rebind_refuses_an_archived_context() {
        let d = test_dispatcher().await;
        let ctx = register_context(&d, Some("archived"), None, PrincipalId::new());
        d.kernel_db()
            .lock()
            .delete_context_binding(ctx)
            .expect("delete the binding row");
        d.kernel_db()
            .lock()
            .archive_context(ctx)
            .expect("archive the context");

        let result = d
            .dispatch(&[s("context"), s("rebind")], &caller_with_context(ctx))
            .await;
        assert!(!result.is_ok(), "an archived context must not be rebound");
        assert!(
            result.message().contains("archived"),
            "refusal should name archival as the reason: {}",
            result.message()
        );
    }

    /// Creation stays truthful: when the rc lifecycle leaves the new context
    /// with no loadout, the success line says so instead of reporting a clean
    /// birth the operator only discovers is broken at their first real verb.
    #[tokio::test]
    async fn create_reports_a_context_its_rc_left_unbound() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("unbound-parent"), None, principal);
        let caller = caller_with_context(parent);
        crate::kj::test_helpers::give_character(&d, &caller);
        // No rc scripts are installed here, so nothing binds the new context.
        let result = d
            .dispatch(&[s("kohai"), s("create"), s("--as"), s("kohai-tester"), s("fresh")], &caller)
            .await;
        assert!(result.is_ok(), "create still succeeds: {}", result.message());
        let msg = result.message();
        assert!(
            msg.contains("created context 'fresh'"),
            "the creation itself is still reported: {msg}"
        );
        assert!(
            msg.contains("no loadout") && msg.contains("kj context rebind"),
            "an unbound outcome must be reported with its repair: {msg}"
        );
    }
}
