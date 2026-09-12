//! Context subcommands: list, info, switch, create, set, log, move, archive, remove, retag.

use clap::{Args, Parser, Subcommand};
use kaijutsu_types::{BlockId, ConsentMode, ContentType, ContextId, ContextState, EdgeKind, PrincipalId};

use crate::kernel_db::{ContextEdgeRow, ContextRow, ContextShellRow, DemoteOutcome, PromoteOutcome, KernelDb, KernelDbError, KernelDbResult};

use super::format::{
    format_context_info, format_context_table, format_context_tree, format_fork_lineage,
};
use super::parse::resolve_model_choice;
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
    /// Model spec `provider/model` or a bare model name (resolved to default provider)
    #[arg(long, short = 'm')]
    model: Option<String>,
    /// Stored legacy prompt value; not included in model instructions. Use rc instruction blocks
    #[arg(long = "system-prompt")]
    system_prompt: Option<String>,
    /// Consent mode: collaborative|autonomous
    #[arg(long)]
    consent: Option<String>,
    /// Working directory for the context's shell
    #[arg(long)]
    cwd: Option<String>,
    /// Set an env var as KEY=VALUE. Repeat the flag for more than one
    #[arg(long)]
    env: Vec<String>,
    /// Context type selecting the /config/rc lifecycle bundle
    #[arg(long = "type")]
    type_: Option<String>,
    /// Cast label — the named model ensemble this context plays under
    /// (`kj cast show <label>`). Must already exist; to clear an assigned
    /// cast use `kj context unset --cast`.
    #[arg(long)]
    cast: Option<String>,
}

impl From<ContextConfigArgs> for ContextConfig {
    fn from(a: ContextConfigArgs) -> Self {
        ContextConfig {
            model_spec: a.model,
            system_prompt: a.system_prompt,
            consent_spec: a.consent,
            cwd_spec: a.cwd,
            env_spec: a.env,
            type_spec: a.type_,
            cast_spec: a.cast,
            review: None,
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
    /// Show a context's metadata (default: current).
    Info {
        /// Context to show. Defaults to the current context. Ids and
        /// labels come from `kj context list`.
        context: Option<String>,
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
    /// Create a new context. Label is positional or `--name`.
    #[command(alias = "new")]
    Create {
        /// Label (positional form; or use --name)
        label: Option<String>,
        /// Label (flag form; takes precedence over the positional label)
        #[arg(long, short = 'n')]
        name: Option<String>,
        /// Parent context to fork the structural edge from
        #[arg(long, short = 'p')]
        parent: Option<String>,
        /// Existing live character performing model work. Amy reviews unless explicitly delegated
        #[arg(long = "as", value_name = "CHARACTER")]
        character: Option<String>,
        #[command(flatten)]
        config: ContextConfigArgs,
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
        /// Existing live character performing model work
        #[arg(long = "as", value_name = "CHARACTER")]
        character: Option<String>,
        /// Existing live character who explicitly reviews this context's work
        #[arg(long, value_name = "CHARACTER")]
        reviewer: Option<String>,
        /// Remove this context's explicit reviewer override
        #[arg(long)]
        clear_reviewer: bool,
        /// Existing live character directing this work
        #[arg(long, value_name = "CHARACTER")]
        director: Option<String>,
        #[command(flatten)]
        config: ContextConfigArgs,
    },
    /// Remove an env var from a context, or clear its assigned cast.
    Unset {
        /// Context to update. Defaults to the current context. Ids and
        /// labels come from `kj context list`.
        context: Option<String>,
        /// Env var key to remove
        #[arg(long)]
        env: Option<String>,
        /// Clear the context's assigned cast (falls back to the registry
        /// default at resolution time)
        #[arg(long)]
        cast: bool,
    },
    /// Show fork lineage from a context up to root (default: current).
    Log {
        /// Context to show lineage for. Defaults to the current context.
        /// Ids and labels come from `kj context list`.
        context: Option<String>,
    },
    /// Reparent a context under a new parent.
    #[command(alias = "mv")]
    Move {
        /// Context to reparent. Ids and labels come from `kj context list`.
        context: String,
        /// New parent to fork the structural edge from. Ids and labels
        /// come from `kj context list`.
        new_parent: String,
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
    /// Soft-delete one context (latched). Its children are untouched and
    /// keep their parent edge: an archived context stays in the graph as
    /// lineage.
    Archive {
        /// Context to archive. Ids and labels come from `kj context list`.
        context: String,
    },
    /// Conclude a context — mark this work "done" (the time-well's hot→recent
    /// transition). Reversible via fork; not latched, not destructive.
    #[command(alias = "done")]
    Conclude {
        /// Context to conclude. Does not recurse into children. Ids and
        /// labels come from `kj context list`.
        context: String,
    },
    /// Promote a context into ring 0 ("active"). Not latched, not
    /// capability-gated (same as conclude). First promote wins the
    /// timestamp — re-promoting an already-promoted context is a no-op.
    Promote {
        /// Context to promote. An archived context (reachable by full id)
        /// is resurrected rather than refused. Ids and labels come from
        /// `kj context list`.
        context: String,
    },
    /// Push a context outward one step on the demote ladder: promoted →
    /// unpromoted, unpromoted/undemoted → demoted, already demoted →
    /// archived. Not latched (each step is reversible except the last).
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
    /// Permanently delete a context (latched).
    #[command(alias = "rm")]
    Remove {
        /// Context to delete. Cannot be the current context. Ids and
        /// labels come from `kj context list`.
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
/// model, system prompt, consent mode, working directory, an env var, and
/// the rc-dispatch `context_type`. `create` reuses the same surface so a
/// context can be born fully configured (fork-parity) instead of needing a
/// follow-up `kj context set`.
#[derive(Default)]
struct ContextConfig {
    model_spec: Option<String>,
    system_prompt: Option<String>,
    consent_spec: Option<String>,
    cwd_spec: Option<String>,
    env_spec: Vec<String>,
    type_spec: Option<String>,
    /// `--cast <label>` — resolved + validated against `list_casts` in
    /// [`KjDispatcher::resolve_context_config`] before any mutation.
    cast_spec: Option<String>,
    review: Option<ReviewAssignment>,
}

struct ReviewAssignment {
    actor: Option<kaijutsu_types::PrincipalId>,
    reviewer: Option<kaijutsu_types::PrincipalId>,
    director: Option<kaijutsu_types::PrincipalId>,
    caller_actor: kaijutsu_types::PrincipalId,
    routing_changed: bool,
    expected_reviewer: kaijutsu_types::PrincipalId,
}

/// A `--model` spec resolved against the LLM registry: a bare model name has
/// had its provider filled in from the registry default (matching `kj fork`).
struct ResolvedModel {
    provider: Option<String>,
    model: Option<String>,
}

/// Insert a new context only while its model approval assignment is valid.
/// The check follows insertion within one database transaction so retirement or
/// delegation changes cannot leave a visible self-reviewed model context.
fn insert_new_context_checked(
    db: &KernelDb,
    row: &ContextRow,
    parent_id: Option<ContextId>,
) -> KernelDbResult<()> {
    db.in_transaction(|db| {
        if let Some(performer) = row.played_by {
            if !db.get_character(performer)?.is_some_and(|sheet| sheet.retired_at.is_none()) {
                return Err(KernelDbError::Validation("performer must name a live character".into()));
            }
            let default_ws = db.get_or_create_default_workspace(row.created_by)?;
            db.insert_context_with_document(row, default_ws)?;
            let reviewer = db.effective_approval_reviewer(row.context_id)?
                .ok_or_else(|| KernelDbError::Validation("no configured approval reviewer".into()))?;
            if performer == reviewer {
                return Err(KernelDbError::Validation("the performer cannot review its own work".into()));
            }
        } else {
            let default_ws = db.get_or_create_default_workspace(row.created_by)?;
            db.insert_context_with_document(row, default_ws)?;
        }
        if let Some(source_id) = parent_id {
            db.insert_edge(&ContextEdgeRow {
                edge_id: uuid::Uuid::now_v7(),
                source_id,
                target_id: row.context_id,
                kind: EdgeKind::Structural,
                metadata: None,
                created_at: kaijutsu_types::now_millis() as i64,
            })?;
        }
        Ok(())
    })
}

impl KjDispatcher {
    /// Validate user-supplied config and resolve `--model`/`--cast` BEFORE
    /// any mutation.
    ///
    /// Checks provider existence, consent-mode spelling, and `--env KEY=VALUE`
    /// shape, and resolves a bare model name (no `provider/` prefix) to the
    /// registry's default provider — erroring if none is configured, exactly
    /// like `kj fork`. A `--cast <label>` is resolved against `list_casts`
    /// the same way: an unknown label fails loud, listing the known casts,
    /// rather than silently leaving the context uncast. Pure checks plus a
    /// registry read and a `kernel_db` read, no writes, so callers can bail
    /// out cleanly without leaving a half-configured (or orphan) context
    /// behind. Returns the resolved model / cast when the corresponding flag
    /// was given, else `None`.
    async fn resolve_context_config(
        &self,
        cfg: &ContextConfig,
    ) -> Result<(Option<ResolvedModel>, Option<kaijutsu_types::CastId>), String> {
        let resolved_model = match cfg.model_spec {
            Some(ref spec) if !spec.is_empty() => {
                let registry = self.kernel().llm().read().await;
                let (provider, model) = resolve_model_choice(&registry, spec)?;
                Some(ResolvedModel { provider, model })
            }
            _ => None,
        };

        if let Some(ref spec) = cfg.consent_spec
            && spec.parse::<ConsentMode>().is_err()
        {
            return Err(format!(
                "invalid consent mode '{spec}' — use 'collaborative' or 'autonomous'"
            ));
        }
        if let Some(env) = cfg.env_spec.iter().find(|e| !e.contains('=')) {
            return Err(format!("--env requires KEY=VALUE format, got '{env}'"));
        }

        let resolved_cast = match cfg.cast_spec {
            Some(ref label) if !label.is_empty() => {
                let db = self.kernel_db().lock();
                match db.get_cast_by_label(label) {
                    Ok(Some(cast)) => Some(cast.cast_id),
                    Ok(None) => {
                        let known: Vec<String> = db
                            .list_casts()
                            .map(|casts| casts.iter().map(|c| c.label.clone()).collect())
                            .unwrap_or_default();
                        let known = if known.is_empty() {
                            "(no casts configured — `kj cast create <label>` starts one)"
                                .to_string()
                        } else {
                            known.join(", ")
                        };
                        return Err(format!("unknown cast '{label}' — known casts: {known}"));
                    }
                    Err(e) => return Err(e.to_string()),
                }
            }
            _ => None,
        };

        Ok((resolved_model, resolved_cast))
    }

    /// Apply already-validated config to an existing context row and return
    /// the human-readable change list. Assumes [`Self::resolve_context_config`]
    /// has already run, so the model is pre-resolved and not re-checked here —
    /// only DB I/O errors surface. The model column is updated in the DB; the
    /// DriftRouter is reconfigured whenever both provider and model are present
    /// (which, post-resolution, is every non-degenerate `--model` spec).
    async fn apply_context_config(
        &self,
        target_id: ContextId,
        cfg: &ContextConfig,
        resolved_model: Option<&ResolvedModel>,
        resolved_cast: Option<kaijutsu_types::CastId>,
    ) -> Result<Vec<String>, String> {
        // One transaction: a failure on a later field (an `--env` key is
        // validated at write time) must not leave an earlier one committed.
        let (changes, model_for_drift) = {
            let guard = self.kernel_db().lock();
            let applied = guard.in_transaction(|db| {
                let mut changes = Vec::new();
                let mut model_for_drift: Option<(String, String)> = None;

                if let Some(review) = &cfg.review {
                    let current = db.get_context(target_id)?
                        .ok_or_else(|| crate::kernel_db::KernelDbError::Validation("context disappeared before assignment".into()))?;
                    let is_default = db.cached_default_approval_reviewer()? == Some(review.caller_actor);
                    if review.routing_changed && !is_default {
                        return Err(crate::kernel_db::KernelDbError::Validation("approval assignment authority changed before commit".into()));
                    }
                    if !review.routing_changed && !is_default {
                        let current_reviewer = db.effective_approval_reviewer(target_id)?
                            .ok_or_else(|| crate::kernel_db::KernelDbError::Validation("approval assignment authority changed before commit".into()))?;
                        if review.caller_actor != review.expected_reviewer || current_reviewer != review.expected_reviewer {
                            return Err(crate::kernel_db::KernelDbError::Validation("approval assignment authority changed before commit".into()));
                        }
                    }
                    if db.list_pending_asks()?.iter().any(|ask| ask.context_id.as_slice() == target_id.as_bytes()) {
                        return Err(crate::kernel_db::KernelDbError::Validation("settle or cancel pending asks before changing this context's approval assignment".into()));
                    }
                    for (principal, must_be_live, field) in [
                        (review.actor, review.actor.is_some(), "performer"),
                        (review.reviewer, review.reviewer != current.reviewer_id, "reviewer"),
                        (review.director, review.director != current.director_id, "director"),
                    ] {
                        if must_be_live {
                            if let Some(principal) = principal {
                                if !db.get_character(principal)?.is_some_and(|sheet| sheet.retired_at.is_none()) {
                                    return Err(crate::kernel_db::KernelDbError::Validation(format!("{field} must name a live character")));
                                }
                            }
                        }
                    }
                    db.update_context_review_assignment(target_id, review.actor, review.reviewer, review.director)?;
                    let candidate_reviewer = db.effective_approval_reviewer(target_id)?
                        .ok_or_else(|| crate::kernel_db::KernelDbError::Validation("no configured approval reviewer".into()))?;
                    if review.actor == Some(candidate_reviewer) {
                        return Err(crate::kernel_db::KernelDbError::Validation("the performer cannot review its own work".into()));
                    }
                    changes.push("performer/reviewer".into());
                }

                if let Some(rm) = resolved_model {
                    db.update_model(target_id, rm.provider.as_deref(), rm.model.as_deref())?;
                    // `model_spec` is the original argv string — guaranteed present
                    // here since `resolved_model` is Some only when it was given.
                    let spec = cfg.model_spec.as_deref().unwrap_or("?");
                    changes.push(format!("model={spec}"));
                    if let (Some(p), Some(m)) = (&rm.provider, &rm.model) {
                        model_for_drift = Some((p.clone(), m.clone()));
                    }
                }

                if let Some(cast_id) = resolved_cast {
                    db.update_cast(target_id, Some(cast_id))?;
                    // `cast_spec` is the original argv label — guaranteed present
                    // here since `resolved_cast` is Some only when it was given.
                    let label = cfg.cast_spec.as_deref().unwrap_or("?");
                    changes.push(format!("cast={label}"));
                }

                // consent_spec is validated upstream; treat a parse miss as absent.
                let consent_mode = cfg
                    .consent_spec
                    .as_ref()
                    .and_then(|s| s.parse::<ConsentMode>().ok());

                if cfg.system_prompt.is_some() || consent_mode.is_some() {
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
                    let new_consent = consent_mode.unwrap_or(current.consent_mode);
                    db.update_settings(target_id, new_prompt, new_consent)?;
                    if cfg.system_prompt.is_some() {
                        changes.push("system-prompt".to_string());
                    }
                    if let Some(ref spec) = cfg.consent_spec
                        && consent_mode.is_some()
                    {
                        changes.push(format!("consent={spec}"));
                    }
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

                Ok((changes, model_for_drift))
            });
            applied.map_err(|e| e.to_string())?
        };
        // db lock released here

        if let Some((p, m)) = model_for_drift {
            let mut drift = self.drift_router().write();
            let _ = drift.configure_llm(target_id, &p, &m);
        }

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
        // `create`/`scratch` are deliberately ungated: minting a context is the
        // bootstrap entry point (a fresh, unjoined session must be able to make
        // its first context), and the new context's loadout is assigned by its
        // rc `create` lifecycle, not the caller's. Read/navigation verbs
        // (list/info/current/switch/log) stay ungated too.
        //
        // `rebind` is ungated on that same argument, and must stay that way: it
        // re-runs the rc lifecycle that assigns loadouts, so it grants exactly
        // what birth would have and nothing the caller chose. Gating it on
        // `Operator` would put the repair for a no-loadout context behind a
        // capability that exact context cannot hold — the lockout it exists to
        // undo. Its own guards (no usable loadout, not archived) are in
        // `context_rebind`.
        if matches!(
            parsed.command,
            ContextCommand::Set { .. }
                | ContextCommand::Unset { .. }
                | ContextCommand::Move { .. }
                | ContextCommand::Rename { .. }
                | ContextCommand::Archive { .. }
                | ContextCommand::Remove { .. }
                | ContextCommand::Retag { .. }
                | ContextCommand::Hydrate { .. }
        ) && let Err(denied) =
            self.require_cap(caller, crate::mcp::Capability::Operator, "context")
        {
            return denied;
        }

        match parsed.command {
            ContextCommand::List { tree } => self.context_list(tree, caller).await,
            ContextCommand::Info { context } => {
                self.context_info(context.as_deref(), caller).await
            }
            ContextCommand::Prompt { context } => {
                self.context_prompt(context.as_deref(), caller).await
            }
            ContextCommand::Current => self.context_current(caller).await,
            ContextCommand::Switch { context } => self.context_switch(&context, caller).await,
            ContextCommand::Create {
                label,
                name,
                parent,
                character,
                config,
            } => {
                self.context_create(
                    name.or(label).as_deref(),
                    parent.as_deref(),
                    character.as_deref(),
                    config.into(),
                    caller,
                )
                .await
            }
            ContextCommand::Scratch => self.context_scratch(caller).await,
            ContextCommand::Rebind { context } => {
                self.context_rebind(context.as_deref(), caller).await
            }
            ContextCommand::Set { context, character, reviewer, clear_reviewer, director, config } => {
                self.context_set(context.as_deref(), character.as_deref(), reviewer.as_deref(), clear_reviewer, director.as_deref(), config.into(), caller).await
            }
            ContextCommand::Unset { context, env, cast } => {
                self.context_unset(context.as_deref(), env.as_deref(), cast, caller)
            }
            ContextCommand::Log { context } => self.context_log(context.as_deref(), caller),
            ContextCommand::Move {
                context,
                new_parent,
            } => self.context_move(&context, &new_parent, caller).await,
            ContextCommand::Archive { context } => self.context_archive(&context, caller).await,
            ContextCommand::Conclude { context } => self.context_conclude(&context, caller).await,
            ContextCommand::Promote { context } => self.context_promote(&context, caller).await,
            ContextCommand::Demote { context } => self.context_demote(&context, caller).await,
            ContextCommand::Pause { context } => self.context_pause(&context, caller, true).await,
            ContextCommand::Resume { context } => self.context_pause(&context, caller, false).await,
            ContextCommand::Remove { context } => self.context_remove(&context, caller).await,
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
        // Resolved once per listing, never per-row — the operator-scale
        // `casts` table is small and this keeps the list a single DB round
        // trip beyond the context query itself.
        let casts: std::collections::HashMap<kaijutsu_types::CastId, String> = db
            .list_casts()
            .map(|cs| cs.into_iter().map(|c| (c.cast_id, c.label)).collect())
            .unwrap_or_default();
        if tree {
            match db.context_dag() {
                Ok(dag) => {
                    let text = format_context_tree(&dag, caller.context_id, &casts);
                    let ids = context_handles(dag.iter().map(|(row, _)| row));
                    KjResult::ok_with_data(text, ids)
                }
                Err(e) => KjResult::Err(format!("kj context list: {e}")),
            }
        } else {
            match db.list_active_contexts() {
                Ok(contexts) => {
                    let text = format_context_table(&contexts, caller.context_id, &casts);
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
            usage,
            workspace_paths,
            workspace_label,
            cast_label,
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

            // Count structural children
            let children_count = db
                .edges_from(target_id, Some(EdgeKind::Structural))
                .map(|edges| edges.len())
                .unwrap_or(0);

            // Count drift edges (both directions)
            let drift_from = db
                .edges_from(target_id, Some(EdgeKind::Drift))
                .map(|edges| edges.len())
                .unwrap_or(0);
            let drift_to = db
                .edges_to(target_id, Some(EdgeKind::Drift))
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

            // Token usage — a SNAPSHOT of the last completed LLM call ("how
            // full is this context right now"), not a running total; see
            // `ContextUsageRow`. `None` for a context that has never
            // completed a call — shown as absence, never a fabricated zero.
            let usage = db.get_context_usage(target_id).ok().flatten();

            // Workspace paths
            let workspace_paths = db.context_workspace_paths(target_id).ok().flatten();
            let workspace_label = row
                .workspace_id
                .and_then(|wsid| db.get_workspace(wsid).ok().flatten())
                .map(|ws| ws.label);

            // Cast label — the row only carries the id; resolve it here for
            // display the same way `workspace_label` above resolves a
            // `workspace_id`. A dangling id (cast removed between reads)
            // shows as absent, not an error.
            let cast_label = row
                .cast_id
                .and_then(|cid| db.get_cast(cid).ok().flatten())
                .map(|c| c.label);

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
                usage,
                workspace_paths,
                workspace_label,
                cast_label,
                played_by_name,
            )
        };

        // Context-window denominator: resolved kernel-side (never shipped to
        // the client as raw inputs to derive) via the same
        // `LlmRegistry::context_window_for_live(provider, model)` `kj model`
        // already uses — the configured backend model metadata wins as an override; absent
        // that, a live Anthropic `GET /v1/models/{id}` lookup (cached) fills
        // the honest gaps config never had (e.g. `claude-sonnet-4-20250514`,
        // deliberately unset in the backend model metadata). `None` when neither source
        // knows the window — the two fields below MUST both be `null` in
        // that case, never a guessed denominator standing in for one. The
        // percentage itself comes from
        // `context_used_pct` — the ONE percentage-math site, shared with the
        // wire's `contextUsedPct` (`ContextHandleInfo` /
        // `kaijutsu-server/src/rpc.rs::list_contexts`) so the two surfaces
        // can never disagree.
        // Resolved effective model: the SAME ladder the turn path walks
        // (explicit context override → cast slot on context_type → registry
        // default) via `resolve_context_model` — so a consumer of this JSON
        // never has to re-derive the resolution ladder from the raw
        // `provider`/`model` columns above (deepseek review P4). `kj model`
        // already resolves the identical way; kept in lockstep so the two
        // surfaces can never disagree. The registry read lock is acquired
        // unconditionally — the context-window lookup below needs it too.
        let registry = self.kernel().llm().read().await;
        let resolved = crate::model_resolution::resolve_context_model(
            &row.context_type,
            row.provider.as_deref(),
            row.model.as_deref(),
            cast_label.as_deref(),
            &registry,
        );
        let (resolved_backend, resolved_model, resolved_source) = match &resolved {
            Some(r) => {
                let source = match &r.source {
                    crate::model_resolution::ModelSource::ContextOverride => {
                        "context".to_string()
                    }
                    crate::model_resolution::ModelSource::CastSlot { cast } => {
                        format!("cast {cast}")
                    }
                    crate::model_resolution::ModelSource::RegistryDefault => {
                        "default".to_string()
                    }
                };
                (Some(r.backend.clone()), Some(r.model.clone()), source)
            }
            None => (None, None, "default".to_string()),
        };

        let (context_window, context_used_pct) = match &usage {
            Some(u) => {
                let window = registry.context_window_for_live(&u.provider, &u.model).await;
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

        {
            let display = match (&resolved_backend, &resolved_model) {
                (Some(b), Some(m)) => format!("{b}/{m}"),
                _ => "(none configured)".to_string(),
            };
            info.push_str(&format!("\nResolved: {display} ({resolved_source})"));
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

        if let Some(paths) = workspace_paths.as_ref().filter(|p| !p.is_empty()) {
            let ws_label = workspace_label.clone().unwrap_or_else(|| "?".into());
            info.push_str(&format!("\nWorkspace: {ws_label}"));
            for p in paths {
                let ro = if p.read_only { " (ro)" } else { "" };
                info.push_str(&format!("\n  {}{ro}", p.path));
            }
        }

        if let Some(ref label) = cast_label {
            info.push_str(&format!("\nCast:    {label}"));
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
        if let Some(override_id) = row.reviewer_id {
            info.push_str(&format!("\nReviewer override: {override_id}"));
        }
        if let Some(director) = row.director_id {
            let director_name = match self.kernel_db().lock().get_character(director) {
                Ok(character) => character.map(|character| character.name).unwrap_or_else(|| director.to_string()),
                Err(error) => return KjResult::Err(format!("kj context info: director lookup: {error}")),
            };
            info.push_str(&format!("\nDirector: {director_name}"));
        }

        // Structured record: full ids and the same fields the text view
        // surfaces, so `kaish-last` round-trips and per-field jq queries work.
        let mut record = serde_json::json!({
            "context_id": row.context_id.to_hex(),
            "label": row.label,
            "provider": row.provider,
            "model": row.model,
            "consent_mode": format!("{:?}", row.consent_mode),
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
            "cast_id": row.cast_id.map(|id| id.to_hex()),
            "cast_label": cast_label,
            // The character whose performance this context is, and its
            // kernel-owned name — `null` together when nobody plays it (a
            // score context, a file document, a pre-character row). See
            // `ContextRow::played_by`'s doc comment.
            "played_by": row.played_by.map(|id| id.to_hex()),
            "played_by_name": played_by_name,
            // Resolved effective model — same ladder as `kj model` and the
            // turn path (`resolve_context_model`): explicit context
            // override → cast slot on context_type → registry default.
            // `resolved_source` is one of "context" | "cast <label>" |
            // "default". Both `resolved_backend`/`resolved_model` are
            // `null` together only when nothing is configured anywhere
            // (no registry default either) — an honest absence, never a
            // fabricated fallback.
            "resolved_backend": resolved_backend,
            "resolved_model": resolved_model,
            "resolved_source": resolved_source,
            "cwd": shell.as_ref().and_then(|s| s.cwd.clone()),
            "env": env_vars.iter()
                .map(|v| (v.key.clone(), serde_json::Value::String(v.value.clone())))
                .collect::<serde_json::Map<_, _>>(),
            // `null` when this context has never completed an LLM call —
            // honest absence, not a fabricated zero. `context_window` /
            // `context_used_pct` are resolved kernel-side (never derived
            // client-side, per the thin-client convention) via
            // `LlmRegistry::context_window_for(provider, model)`; both are
            // `null` together whenever the window isn't configured for this
            // model (e.g. `claude-sonnet-4-20250514` deliberately has none) —
            // never a fabricated denominator standing in for an unknown one.
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
        record["reviewer_id"] = serde_json::json!(effective_review.as_ref().map(|review| review.reviewer.principal_id.to_hex()));
        record["reviewer_name"] = serde_json::json!(reviewer_name);
        record["review_source"] = serde_json::json!(effective_review.as_ref().map(|review| review.source.as_str()));
        record["reviewer_error"] = serde_json::json!(reviewer_error);
        record["reviewer_override_id"] = serde_json::json!(row.reviewer_id.map(|id| id.to_hex()));
        record["director_id"] = serde_json::json!(row.director_id.map(|id| id.to_hex()));

        KjResult::ok_with_data(info, record)
    }

    /// Render the system prompt this context actually gets: the exact
    /// `rc sections → <situation>` assembly `spawn_llm_for_prompt`
    /// builds for a real turn (`crates/kaijutsu-server/src/llm_stream.rs`),
    /// reusing the same pure pieces (`build_system_prompt`,
    /// `extract_system_prompt_sections`, `resolve_context_model`) so this
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
    /// Model source and the staging guard both now mirror the turn path
    /// exactly:
    ///
    /// - **Model source.** Both sides call the same `resolve_context_model`,
    ///   fed the live DriftRouter handle's `provider`/`model` — never the
    ///   KernelDb row — so a preview can't show a `<model>` the turn would
    ///   not pick. (The RPC `--model` override remains a legitimate
    ///   divergence: a preview has no future turn argument to reflect.)
    /// - **Staging.** A `Staging` context is refused here exactly as
    ///   `spawn_llm_for_prompt` refuses it, instead of rendering a prompt
    ///   that could not actually run.
    async fn context_prompt(&self, context: Option<&str>, caller: &KjCaller) -> KjResult {
        // Same !Send guard-scoping constraint as context_info: KernelDb's
        // parking_lot::MutexGuard must not cross an `.await` below.
        let (target_id, row, cast_label, performer, _) = {
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

            let cast_label = row
                .cast_id
                .and_then(|cid| db.get_cast(cid).ok().flatten())
                .map(|c| c.label);

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
            (target_id, row, cast_label, performer, ())
        };
        let reviewer = match self.kernel().resolve_context_review(target_id).await {
            Ok(review) => Some(review.reviewer),
            Err(error) => return KjResult::Err(format!("kj context prompt: {error}")),
        };

        // Situational label/state/provider/model come from the live
        // DriftRouter handle, NOT the KernelDb row — mirroring exactly what
        // `spawn_llm_for_prompt` reads for a real turn (llm_stream.rs). A
        // resolved target_id with no drift handle renders all four absent,
        // same as the turn path would — an honest gap, not a row fallback
        // that would make this view lie about what the turn path sees.
        let (ctx_label, ctx_state, ctx_provider_name, ctx_model) = {
            let drift = self.drift_router().read();
            match drift.get(target_id) {
                Some(h) => (h.label.clone(), Some(h.state), h.provider.clone(), h.model.clone()),
                None => (None, None, None, None),
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

        // Resolved effective model — same ladder `context_info` and the
        // turn path use (explicit override → cast slot → registry
        // default), fed from the same source the turn path reads
        // (DriftRouter handle, not the KernelDb row — see fn doc comment).
        // Unlike the turn path, an unresolved model does not error here.
        let resolved = {
            let registry = self.kernel().llm().read().await;
            crate::model_resolution::resolve_context_model(
                &row.context_type,
                ctx_provider_name.as_deref(),
                ctx_model.as_deref(),
                cast_label.as_deref(),
                &registry,
            )
        };
        let (resolved_backend, resolved_model) = match &resolved {
            Some(r) => (Some(r.backend.clone()), Some(r.model.clone())),
            None => (None, None),
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

        // tool_names: the current tool inventory via the same broker call
        // the turn path uses, gated on the CALLING principal's own
        // visibility — a preview should show what this caller's own turn
        // would actually see, not an omniscient view.
        let tool_names: Vec<String> = match self
            .kernel()
            .list_tool_defs_via_broker(target_id, caller.principal_id)
            .await
        {
            Ok(defs) => defs.into_iter().map(|(name, _, _)| name).collect(),
            Err(e) => return KjResult::Err(format!("kj context prompt: tool broker: {e}")),
        };

        let situational = crate::llm::SituationalContext {
            context_id: Some(target_id),
            context_label: ctx_label.clone(),
            context_state: ctx_state,
            provider: ctx_provider_name,
            model: resolved_model.clone(),
            performer,
            reviewer,
            tool_names,
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

    async fn context_create(
        &self,
        label: Option<&str>,
        parent: Option<&str>,
        character: Option<&str>,
        mut cfg: ContextConfig,
        caller: &KjCaller,
    ) -> KjResult {
        // Label comes from --name/-n (fork-parity) or the first positional
        // argument (resolved in the dispatcher as `name.or(label)`).
        let label = match label {
            Some(l) => l,
            None => {
                return KjResult::Err(
                    "kj context create: requires a label (positional or --name)".to_string(),
                );
            }
        };

        // Resolve --parent (default to root when absent / unresolvable-as-none).
        let parent_id = {
            let db = self.kernel_db().lock();
            match super::refs::resolve_context_arg(parent, caller, &db) {
                Ok(id) => Some(id),
                Err(_) if parent.is_none() => None, // Default to root if no current context
                Err(e) => return KjResult::Err(format!("kj context create: {e}")),
            }
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
            return KjResult::Err(format!("kj context create: {e}"));
        }

        // The beat is no longer a Rust special-case here: arming a musician is an
        // rc step (`musician/create/S20-arm.kai` runs `kj transport arm`), so a
        // context_type is a beat participant exactly when its `create/` rc arms it
        // — no `context_type == "musician"` branch, and new beat-bearing roles
        // (funkMusician, …) need no kernel edit. `docs/chameleon.md`,
        // "context_type is an rc bundle of features".

        // Validate + resolve the rest before any mutation so a typo'd
        // --model/--cast/--consent/--env can't leave an orphan context behind.
        let (resolved_model, resolved_cast) = match self.resolve_context_config(&cfg).await {
            Ok(r) => r,
            Err(e) => return KjResult::Err(format!("kj context create: {e}")),
        };
        let new_id = ContextId::new();

        // Resolve the performer before validating its model assignment. The
        // final insertion repeats the liveness and reviewer checks under its
        // transaction because either can change while that async validation runs.
        // The requester remains the author of creation and rc output.
        {
            let played_by = {
                let db = self.kernel_db().lock();
                match character {
                Some(name) => match db.get_character_by_name(name) {
                    Ok(Some(row)) if row.retired_at.is_none() => Some(row.principal_id),
                    Ok(Some(_)) => return KjResult::Err(format!(
                        "kj context create: character '{name}' is retired; choose a live character with `kj character list`"
                    )),
                    Ok(None) => return KjResult::Err(format!(
                        "kj context create: no character named '{name}'; use `kj character list` or `kj character create <name>`"
                    )),
                    Err(e) => return KjResult::Err(format!("kj context create: {e}")),
                },
                    None => None,
                }
            };
            if let Some(performer) = played_by {
                if let Err(error) = self.kernel().validate_model_review_assignment(performer, Some(caller.actor_id), None).await {
                    return KjResult::Err(format!("kj context create: {error}"));
                }
            }
            let row = ContextRow {
                context_id: new_id,
                label: Some(label.to_string()),
                provider: None,
                model: None,
                system_prompt: None,
                consent_mode: ConsentMode::Collaborative,
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
                cast_id: None,
                origin_host: None,
                played_by,
                reviewer_id: None,
                director_id: Some(caller.actor_id),
            };
            let db = self.kernel_db().lock();
            if let Err(e) = insert_new_context_checked(&db, &row, parent_id) {
                return KjResult::Err(format!("kj context create: {e}"));
            }
        }

        // Register in DriftRouter
        {
            let mut drift = self.drift_router().write();
            if let Err(e) = drift.register(new_id, Some(label), parent_id, caller.principal_id) {
                return KjResult::Err(format!("kj context create: {e}"));
            }
        }

        // Apply settable config (model, cast, system-prompt, consent, cwd,
        // env) now that the row and drift handle exist. Validated above;
        // only DB I/O errors surface here.
        let config_changes = match self
            .apply_context_config(new_id, &cfg, resolved_model.as_ref(), resolved_cast)
            .await
        {
            Ok(changes) => changes,
            Err(e) => return KjResult::Err(format!("kj context create: {e}")),
        };

        // Run rc create-lifecycle scripts. Failures surface as Error
        // blocks in the new context — they don't abort context creation
        // (Amy, 2026-08-12: a fresh context holds nothing worth saving, so
        // aborting buys little and destroys the Error blocks that explain the
        // failure; `kj context rebind` is the repair).
        if let Err(e) = self
            .run_rc_lifecycle("create", new_id, parent_id, None, None, caller)
            .await
        {
            tracing::warn!("rc create lifecycle: {e}");
        }

        // The beat arm now lives in the musician's `create/` rc (run above), not
        // here — see the note at the top of this fn and `docs/chameleon.md`.

        let mut msg = format!("created context '{}' ({})", label, new_id.short());
        if !config_changes.is_empty() {
            msg.push_str(&format!(" [{}]", config_changes.join(", ")));
        }
        // Report the *outcome*, not just the log line. A failed lifecycle used
        // to be a `tracing::warn!` under a plain success message — the operator
        // was told creation worked and only found out when the new context
        // refused its first real verb.
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
        }))
    }

    /// `kj context rebind [<ctx>]` — re-run the `create` rc lifecycle against a
    /// context that has no usable loadout. The repair half of the create-time
    /// rc failure: creation succeeds even when its rc lifecycle fails, and
    /// this verb fixes the result afterward.
    ///
    /// Amy ruled (2026-08-12) against aborting creation when the lifecycle
    /// fails: a fresh context holds nothing worth saving, and aborting would
    /// destroy the Error blocks that explain *why* it failed — the one part of
    /// diagnosis that works today, since reading stays ungated. So creation
    /// still succeeds, loudly, and this verb repairs the result.
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
        let row = {
            let db = self.kernel_db().lock();
            let target_id = match super::refs::resolve_context_arg(target_arg, caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj context rebind: {e}")),
            };
            match db.get_context(target_id) {
                Ok(Some(row)) => row,
                Ok(None) => {
                    return KjResult::Err(format!(
                        "kj context rebind: context {} not found",
                        target_id.short()
                    ));
                }
                Err(e) => return KjResult::Err(format!("kj context rebind: {e}")),
            }
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

        if let Err(e) = self
            .run_rc_lifecycle("create", target_id, row.forked_from, None, None, caller)
            .await
        {
            return KjResult::Err(format!("kj context rebind: rc create lifecycle: {e}"));
        }

        // Individual script failures land as Error blocks rather than an Err
        // from the lifecycle, so the read-back is what actually says whether the
        // repair took.
        match self.has_usable_loadout(target_id) {
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
            Err(e) => KjResult::Err(format!(
                "kj context rebind: lifecycle ran, but reading context {}'s loadout back \
                 failed, so the repair is unconfirmed: {e}",
                target_id.short()
            )),
        }
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
                provider: None,
                model: None,
                system_prompt: None,
                consent_mode: ConsentMode::Collaborative,
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
                cast_id: None,
                origin_host: None,
                played_by: None,
                reviewer_id: None,
                director_id: Some(caller.actor_id),
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

    /// `kj context set <ctx> [--model p/m] [--cast label] [--system-prompt text] [--consent mode] [--cwd path] [--env KEY=VALUE] [--type t]`
    async fn context_set(
        &self,
        target_arg: Option<&str>,
        character: Option<&str>,
        reviewer: Option<&str>,
        clear_reviewer: bool,
        director: Option<&str>,
        mut cfg: ContextConfig,
        caller: &KjCaller,
    ) -> KjResult {
        // Validate + resolve the model/cast before touching the DB.
        let (resolved_model, resolved_cast) = match self.resolve_context_config(&cfg).await {
            Ok(r) => r,
            Err(e) => return KjResult::Err(format!("kj context set: {e}")),
        };

        // Resolve target (brief lock; resolver borrows the db).
        let target_id = {
            let db = self.kernel_db().lock();
            match super::refs::resolve_context_arg(target_arg, caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj context set: {e}")),
            }
        };

        if character.is_some() || reviewer.is_some() || clear_reviewer || director.is_some() {
            if reviewer.is_some() && clear_reviewer {
                return KjResult::Err("kj context set: --reviewer and --clear-reviewer cannot be used together".into());
            }
            let changes_routing = reviewer.is_some() || clear_reviewer || director.is_some();
            let authority = match self.kernel().default_approval_reviewer().await {
                Ok(authority) => Some(authority),
                Err(error) if changes_routing => return KjResult::Err(format!("kj context set: {error}")),
                Err(error) => {
                    tracing::warn!("could not load default approval reviewer while assigning performer: {error}");
                    None
                }
            };
            if changes_routing && authority != Some(caller.actor_id) {
                return KjResult::Err("kj context set: only the default reviewer may change approval routing".into());
            }
            let expected_reviewer = if authority != Some(caller.actor_id) {
                let effective = match self.kernel().resolve_context_review(target_id).await {
                    Ok(review) => review,
                    Err(error) => return KjResult::Err(format!("kj context set: {error}")),
                };
                if caller.actor_id != effective.reviewer.principal_id {
                    return KjResult::Err("kj context set: only the default or effective reviewer may assign a performer".into());
                }
                effective.reviewer.principal_id
            } else {
                caller.actor_id
            };
            let assignment = {
                let db = self.kernel_db().lock();
                (|| -> Result<ReviewAssignment, String> {
                    let row = db.get_context(target_id).map_err(|e| e.to_string())?
                        .ok_or_else(|| "context not found".to_string())?;
                    if db.list_pending_asks().map_err(|e| e.to_string())?.iter().any(|ask| ask.context_id.as_slice() == target_id.as_bytes()) {
                        return Err("settle or cancel pending asks before changing this context's approval assignment".into());
                    }
                    let resolve = |name: &str| -> Result<kaijutsu_types::PrincipalId, String> {
                        let sheet = db.get_character_by_name(name).map_err(|e| e.to_string())?
                            .ok_or_else(|| format!("no character named '{name}'"))?;
                        if sheet.retired_at.is_some() { return Err(format!("character '{name}' is retired")); }
                        Ok(sheet.principal_id)
                    };
                    let actor = character.map(resolve).transpose()?.or(row.played_by);
                    let reviewer = if clear_reviewer { None } else { reviewer.map(resolve).transpose()?.or(row.reviewer_id) };
                    let director = director.map(resolve).transpose()?.or(row.director_id);
                    Ok(ReviewAssignment { actor, reviewer, director, caller_actor: caller.actor_id, routing_changed: changes_routing, expected_reviewer })
                })()
            };
            let assignment = match assignment { Ok(value) => value, Err(error) => return KjResult::Err(format!("kj context set: {error}")) };
            let review = if let Some(performer) = assignment.actor {
                self.kernel().validate_model_review_assignment(performer, assignment.director, assignment.reviewer).await
            } else {
                self.kernel().resolve_review_assignment(assignment.actor, assignment.director, assignment.reviewer).await
            };
            if let Err(error) = review {
                return KjResult::Err(format!("kj context set: {error}"));
            }
            cfg.review = Some(assignment);
        }

        match self
            .apply_context_config(target_id, &cfg, resolved_model.as_ref(), resolved_cast)
            .await
        {
            Ok(changes) if changes.is_empty() => {
                KjResult::ok("no changes specified".to_string())
            }
            Ok(changes) => KjResult::ok(format!("updated: {}", changes.join(", "))),
            Err(e) => KjResult::Err(format!("kj context set: {e}")),
        }
    }

    /// `kj context unset [<ctx>] --env KEY` — remove an env var from a context.
    fn context_unset(
        &self,
        target_arg: Option<&str>,
        env_key: Option<&str>,
        clear_cast: bool,
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
        } else if clear_cast {
            match db.update_cast(target_id, None) {
                Ok(()) => KjResult::ok("cleared cast".to_string()),
                Err(e) => KjResult::Err(format!("kj context unset: {e}")),
            }
        } else {
            KjResult::Err("kj context unset: requires --env KEY or --cast".to_string())
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
                let text = format_fork_lineage(&lineage, caller.context_id);
                let handles = context_handles(lineage.iter().map(|(row, _)| row));
                KjResult::ok_with_data(text, handles)
            }
            Err(e) => KjResult::Err(format!("kj context log: {e}")),
        }
    }

    /// `kj context move <ctx> <new-parent>` — reparent a context.
    async fn context_move(
        &self,
        ctx_ref: &str,
        new_parent_ref: &str,
        _caller: &KjCaller,
    ) -> KjResult {
        // All DB work in a single lock scope, no await
        let db = self.kernel_db().lock();

        let ctx_id = match db.resolve_context(ctx_ref) {
            Ok(id) => id,
            Err(e) => return KjResult::Err(format!("kj context move: {e}")),
        };
        let new_parent_id = match db.resolve_context(new_parent_ref) {
            Ok(id) => id,
            Err(e) => return KjResult::Err(format!("kj context move: {e}")),
        };

        // Delete old structural edges pointing to ctx_id
        let old_parents = match db.structural_parents(ctx_id) {
            Ok(p) => p,
            Err(e) => return KjResult::Err(format!("kj context move: {e}")),
        };
        for parent in &old_parents {
            let _ = db.delete_structural_edge(parent.context_id, ctx_id);
        }

        // Insert new structural edge (with cycle detection)
        let edge = ContextEdgeRow {
            edge_id: uuid::Uuid::now_v7(),
            source_id: new_parent_id,
            target_id: ctx_id,
            kind: EdgeKind::Structural,
            metadata: None,
            created_at: kaijutsu_types::now_millis() as i64,
        };
        if let Err(e) = db.insert_edge(&edge) {
            return KjResult::Err(format!("kj context move: {e}"));
        }

        let ctx_label = db
            .get_context(ctx_id)
            .ok()
            .flatten()
            .and_then(|r| r.label)
            .unwrap_or_else(|| ctx_id.short());
        let parent_label = db
            .get_context(new_parent_id)
            .ok()
            .flatten()
            .and_then(|r| r.label)
            .unwrap_or_else(|| new_parent_id.short());

        KjResult::ok(format!("moved '{}' under '{}'", ctx_label, parent_label))
    }

    /// `kj context archive <ctx>` — soft-delete one context. `Destroy`-classed
    /// (`kj/effect.rs`); the dispatcher latches an unconfirmed call
    /// before this handler runs.
    ///
    /// Never recurses. Children keep their structural edge and their own
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
            let label = db
                .get_context(target_id)
                .ok()
                .flatten()
                .and_then(|r| r.label)
                .unwrap_or_else(|| target_id.short());
            (target_id, label)
        };

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

    /// `kj context conclude <ctx>` — the explicit "this work is done" act.
    ///
    /// Sets `concluded_at` + the `Concluded` lifecycle state (KernelDb first as
    /// the authoritative stamp, then the in-memory drift router so `listContexts`
    /// reflects it immediately). Single context — unlike archive it does NOT
    /// recurse into children. Reversible by forking; idempotent (re-concluding a
    /// concluded context is a no-op success).
    async fn context_conclude(&self, ctx_ref: &str, caller: &KjCaller) -> KjResult {
        let target_id = {
            let db = self.kernel_db().lock();
            match super::refs::resolve_context_arg(Some(ctx_ref), caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj context conclude: {e}")),
            }
        };

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
    /// Not latched, not capability-gated (same as conclude). First-write-wins
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
    /// Not latched. Only the ladder's terminal step (archive) touches the
    /// drift router.
    async fn context_demote(&self, ctx_ref: &str, caller: &KjCaller) -> KjResult {
        let target_id = {
            let db = self.kernel_db().lock();
            match super::refs::resolve_context_arg(Some(ctx_ref), caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj context demote: {e}")),
            }
        };

        let outcome = {
            let db = self.kernel_db().lock();
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

    /// `kj context remove <ctx>` — permanently delete a context.
    /// `Destroy`-classed (`kj/effect.rs`); the dispatcher latches
    /// an unconfirmed call before this handler runs.
    async fn context_remove(&self, ctx_ref: &str, caller: &KjCaller) -> KjResult {
        let (target_id, target_label) = {
            let db = self.kernel_db().lock();
            let target_id =
                match super::refs::resolve_context_arg(Some(ctx_ref), caller, &db) {
                    Ok(id) => id,
                    Err(e) => return KjResult::Err(format!("kj context remove: {e}")),
                };
            let label = db
                .get_context(target_id)
                .ok()
                .flatten()
                .and_then(|r| r.label)
                .unwrap_or_else(|| target_id.short());
            (target_id, label)
        };

        if Some(target_id) == caller.context_id {
            return KjResult::Err(
                "kj context remove: cannot remove the current context".to_string(),
            );
        }

        // MCP subscription cleanup removed alongside the legacy MCP pool
        // in Phase 1 M5.

        // Kill any background host processes this context still owns
        // (`background_exec.rs`) before the document they stream into is
        // deleted below — an orphaned `Running` entry pointing at a gone
        // block would otherwise keep the OS process alive with nowhere for
        // its output to land. Fire-and-forget: the supervising tasks tear
        // the processes down independently, `context_remove` doesn't block
        // on their exit.
        self.kernel().background_processes().kill_all_for_context(target_id);

        // Delete from DB (CASCADE deletes edges)
        {
            let db = self.kernel_db().lock();
            if let Err(e) = db.delete_context(target_id) {
                return KjResult::Err(format!("kj context remove: {e}"));
            }
        }

        // Remove document from BlockStore
        let _ = self.block_store().delete_document(target_id);

        // Unregister from DriftRouter (no db lock held)
        let mut drift = self.drift_router().write();
        drift.unregister(target_id);

        KjResult::ok(format!("removed context '{}'", target_label))
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
            | Self::Prompt { .. }
            | Self::Current
            | Self::Log { .. } => Effect::Read,
            Self::Switch { .. }
            | Self::Create { .. }
            | Self::Scratch
            | Self::Rebind { .. }
            | Self::Set { .. }
            | Self::Unset { .. }
            | Self::Move { .. }
            | Self::Rename { .. }
            | Self::Conclude { .. }
            | Self::Promote { .. }
            | Self::Demote { .. }
            | Self::Pause { .. }
            | Self::Resume { .. }
            | Self::Hydrate { .. } => Effect::Write,
            Self::Archive { .. } | Self::Remove { .. } | Self::Retag { .. } => Effect::Destroy,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{insert_new_context_checked, ContextConfig, ReviewAssignment};
    use crate::kernel_db::ContextEdgeRow;
    #[allow(unused_imports)]
    use crate::kj::KjResult;
    use crate::kj::test_helpers::*;
    use kaijutsu_types::{ConsentMode, ContextId, ContextState, EdgeKind, PrincipalId};

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

        // Add structural edge for child
        let child = register_context(&d, Some("child"), Some(root), principal);
        {
            let db = d.kernel_db().lock();
            db.insert_edge(&ContextEdgeRow {
                edge_id: uuid::Uuid::now_v7(),
                source_id: root,
                target_id: child,
                kind: EdgeKind::Structural,
                metadata: None,
                created_at: kaijutsu_types::now_millis() as i64,
            })
            .unwrap();
        }

        let c = caller_with_context(root);
        let result = d
            .dispatch(&[s("context"), s("list"), s("--tree")], &c)
            .await;
        assert!(result.is_ok());
        let msg = result.message();
        assert!(msg.contains("root"), "output: {msg}");
        assert!(msg.contains("child"), "output: {msg}");
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
    async fn default_reviewer_repairs_a_retired_explicit_reviewer() {
        let d = test_dispatcher().await;
        let amy = test_reviewer_principal();
        let retired = PrincipalId::new();
        let owner = PrincipalId::new();
        let context = register_context(&d, Some("repair-reviewer"), None, owner);
        {
            let db = d.kernel_db().lock();
            db.insert_character(&crate::kernel_db::CharacterRow { principal_id: retired, name: "retired-reviewer".into(), created_at: 0, retired_at: Some(1), handoff_ctx: None }).unwrap();
            db.set_default_approval_reviewer(amy).unwrap();
            db.update_context_review_assignment(context, None, Some(retired), None).unwrap();
        }
        let caller = crate::kj::KjCaller { principal_id: amy, actor_id: amy, reviewer_id: None, context_id: Some(context), session_id: kaijutsu_types::SessionId::new(), confirmed: false, rc_depth: 0, privileged: false };
        let result = d.dispatch(&[s("context"), s("set"), s("."), s("--reviewer"), s("amy")], &caller).await;
        assert!(result.is_ok(), "{}", result.message());
        let row = d.kernel_db().lock().get_context(context).unwrap().unwrap();
        assert_eq!(row.reviewer_id, Some(amy));
        assert_eq!(d.kernel().resolve_context_review(context).await.unwrap().reviewer.principal_id, amy);
    }

    #[tokio::test]
    async fn explicit_reviewer_can_assign_a_performer_when_default_is_retired() {
        let d = test_dispatcher().await;
        let amy = test_reviewer_principal();
        let lead = PrincipalId::new();
        let coder = PrincipalId::new();
        let context = register_context(&d, Some("explicit-reviewer"), None, amy);
        {
            let db = d.kernel_db().lock();
            for (principal_id, name) in [(lead, "lead"), (coder, "coder")] {
                db.insert_character(&crate::kernel_db::CharacterRow {
                    principal_id,
                    name: name.into(),
                    created_at: 0,
                    retired_at: None,
                    handoff_ctx: None,
                }).unwrap();
            }
            db.update_context_review_assignment(context, None, Some(lead), Some(amy)).unwrap();
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
        };
        let assign = d.dispatch(&[s("context"), s("set"), s("."), s("--as"), s("coder")], &lead_caller).await;
        assert!(assign.is_ok(), "{}", assign.message());
        let row = d.kernel_db().lock().get_context(context).unwrap().unwrap();
        assert_eq!(row.played_by, Some(coder));
        assert_eq!(row.reviewer_id, Some(lead));
        assert_eq!(row.director_id, Some(amy), "retaining a retired director does not block performer repair");

        let routing = d.dispatch(&[s("context"), s("set"), s("."), s("--reviewer"), s("coder")], &lead_caller).await;
        assert!(!routing.is_ok());
        assert!(routing.message().contains("retired") || routing.message().contains("default reviewer"), "{}", routing.message());
        let row = d.kernel_db().lock().get_context(context).unwrap().unwrap();
        assert_eq!(row.reviewer_id, Some(lead));
    }

    #[tokio::test]
    async fn pending_ask_blocks_default_reviewer_routing_changes() {
        let d = test_dispatcher().await;
        let amy = test_reviewer_principal();
        let coder = PrincipalId::new();
        let director = PrincipalId::new();
        let context = register_context(&d, Some("pending-routing"), None, amy);
        {
            let db = d.kernel_db().lock();
            for (principal_id, name) in [(coder, "coder"), (director, "lead")] {
                db.insert_character(&crate::kernel_db::CharacterRow { principal_id, name: name.into(), created_at: 0, retired_at: None, handoff_ctx: None }).unwrap();
            }
            db.set_default_approval_reviewer(amy).unwrap();
            db.update_context_review_assignment(context, Some(coder), Some(amy), None).unwrap();
        }
        let gate_caller = crate::kj::KjCaller { principal_id: amy, actor_id: coder, reviewer_id: Some(amy), context_id: Some(context), session_id: kaijutsu_types::SessionId::new(), confirmed: false, rc_depth: 0, privileged: false };
        let spec = crate::kj::gate::GateSpec { origin: approval_ledger::types::Origin::Hook, instance: "test".into(), tool: "test".into(), hook_id: None, description: "pending".into(), authorized_label: "pending".into(), statements: vec![crate::kj::gate::GatedStatement { rendered: "pending".into(), statement_kind: "test".into(), vars: vec![], source_index: None }], exec_source: None, planned: vec![] };
        let outcome = crate::kj::gate::run_gate(d.kernel_db(), &gate_caller, spec, d.kernel().ledger_flows(), &crate::kj::gate_policy::no_config()).await;
        assert!(outcome.ask.is_some());
        let amy_caller = crate::kj::KjCaller { principal_id: amy, actor_id: amy, reviewer_id: None, context_id: Some(context), session_id: kaijutsu_types::SessionId::new(), confirmed: false, rc_depth: 0, privileged: false };
        for argv in [[s("context"), s("set"), s("."), s("--reviewer"), s("lead")], [s("context"), s("set"), s("."), s("--director"), s("lead")]] {
            let result = d.dispatch(&argv, &amy_caller).await;
            assert!(!result.is_ok());
            assert!(result.message().contains("settle or cancel pending asks"), "{}", result.message());
        }
        let row = d.kernel_db().lock().get_context(context).unwrap().unwrap();
        assert_eq!(row.reviewer_id, Some(amy));
        assert_eq!(row.director_id, None);
    }

    #[tokio::test]
    async fn context_info_keeps_metadata_when_explicit_reviewer_is_retired() {
        let d = test_dispatcher().await;
        let owner = PrincipalId::new();
        let retired = PrincipalId::new();
        let context = register_context(&d, Some("broken-review"), None, owner);
        {
            let db = d.kernel_db().lock();
            db.insert_character(&crate::kernel_db::CharacterRow { principal_id: retired, name: "retired".into(), created_at: 0, retired_at: Some(1), handoff_ctx: None }).unwrap();
            db.update_context_review_assignment(context, Some(owner), Some(retired), None).unwrap();
        }
        let caller = crate::kj::KjCaller { principal_id: owner, actor_id: owner, reviewer_id: None, context_id: Some(context), session_id: kaijutsu_types::SessionId::new(), confirmed: false, rc_depth: 0, privileged: false };
        let result = d.dispatch(&[s("context"), s("info"), s(".")], &caller).await;
        assert!(result.is_ok(), "{}", result.message());
        assert!(result.message().contains("Reviewer error:"));
        let KjResult::Ok { data: Some(data), .. } = result else { panic!("context info has data") };
        assert_eq!(data["reviewer_id"], serde_json::Value::Null);
        assert!(data["reviewer_error"].as_str().unwrap().contains("retired"));
        assert_eq!(data["reviewer_override_id"], retired.to_hex());
        assert_eq!(data["played_by"], owner.to_hex());
    }

    #[tokio::test]
    async fn final_assignment_rejects_reviewer_revoked_after_precheck() {
        let d = test_dispatcher().await;
        let amy = test_reviewer_principal();
        let lead = PrincipalId::new();
        let coder = PrincipalId::new();
        let context = register_context(&d, Some("stale-reviewer"), None, amy);
        {
            let db = d.kernel_db().lock();
            for (principal_id, name) in [(lead, "lead"), (coder, "coder")] {
                db.insert_character(&crate::kernel_db::CharacterRow { principal_id, name: name.into(), created_at: 0, retired_at: None, handoff_ctx: None }).unwrap();
            }
            db.set_default_approval_reviewer(amy).unwrap();
            db.update_context_review_assignment(context, None, None, Some(lead)).unwrap();
            db.grant_approval_delegation(lead, lead, amy).unwrap();
        }
        let expected = d.kernel().resolve_context_review(context).await.unwrap();
        assert_eq!(expected.reviewer.principal_id, lead);
        d.kernel_db().lock().revoke_approval_delegation(lead, amy).unwrap();
        let cfg = ContextConfig { review: Some(ReviewAssignment { actor: Some(coder), reviewer: None, director: Some(lead), caller_actor: lead, routing_changed: false, expected_reviewer: lead }), ..Default::default() };
        let error = d.apply_context_config(context, &cfg, None, None).await.unwrap_err();
        assert!(error.contains("authority changed"), "{error}");
        let row = d.kernel_db().lock().get_context(context).unwrap().unwrap();
        assert_eq!(row.played_by, None);
        assert_eq!(row.director_id, Some(lead));
    }

    #[tokio::test]
    async fn context_create_basic() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);

        let c = caller_with_context(parent);
        let result = d
            .dispatch(&[s("context"), s("create"), s("child-ctx")], &c)
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

    #[tokio::test]
    async fn context_review_assignment_belongs_to_the_director() {
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
        };
        let coder = PrincipalId::new();
        let lead = PrincipalId::new();
        for (principal_id, name) in [(coder, "coder"), (lead, "lead")] {
            d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
                principal_id, name: name.into(), created_at: 0, retired_at: None, handoff_ctx: None,
            }).unwrap();
        }
        let context = register_context(&d, Some("review-work"), None, amy.actor_id);
        let amy = crate::kj::KjCaller { context_id: Some(context), ..amy };
        let set = d.dispatch(&[s("context"), s("set"), s("."), s("--as"), s("coder"), s("--reviewer"), s("lead")], &amy).await;
        assert!(set.is_ok(), "{}", set.message());
        let assigned = d.kernel_db().lock().get_context(context).unwrap().unwrap();
        assert_eq!(assigned.played_by, Some(coder));
        assert_eq!(assigned.reviewer_id, Some(lead));
        let coder_call = amy.clone().with_actor(coder, Some(lead));
        let refused = d.dispatch(&[s("context"), s("set"), s("."), s("--as"), s("amy")], &coder_call).await;
        assert!(!refused.is_ok());
        assert!(refused.message().contains("only the default or effective reviewer"));
        let self_review = d.dispatch(&[s("context"), s("set"), s("."), s("--reviewer"), s("coder")], &amy).await;
        assert!(!self_review.is_ok());
        assert!(self_review.message().contains("cannot review"));
        let unchanged = d.kernel_db().lock().get_context(context).unwrap().unwrap();
        assert_eq!(unchanged.played_by, Some(coder));
        assert_eq!(unchanged.reviewer_id, Some(lead));
    }

    #[tokio::test]
    async fn context_create_help_describes_performer_selection() {
        let d = test_dispatcher().await;
        let result = d.dispatch(&[s("context"), s("create"), s("--help")], &test_caller()).await;
        assert!(result.is_ok(), "{}", result.message());
        let help = result.message();
        println!("{help}");
        assert!(help.contains("--as <CHARACTER>"));
        assert!(help.contains("Amy reviews"));
        let set = d.dispatch(&[s("context"), s("set"), s("."), s("--as"), s("banto")], &test_caller()).await;
        assert!(!set.is_ok(), "unknown context or character is refused");
    }

    // Creating through a shell re-enters kaish for rc; use the production rc stack.
    #[test]
    fn context_create_as_records_performer_and_loads_director_handoff() {
        std::thread::Builder::new()
            .stack_size(crate::KAISH_RC_THREAD_STACK)
            .spawn(|| {
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
            handoff_ctx: None,
        }).unwrap();
        d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
            principal_id: caller.principal_id, name: s("amy"), created_at: 1,
            retired_at: None, handoff_ctx: None,
        }).unwrap();
        let parent = register_context(&d, Some("parent-operator"), None, caller.principal_id);
        d.kernel_db().lock().update_played_by(parent, Some(caller.principal_id)).unwrap();
        caller.context_id = Some(parent);
        let note = d.dispatch(
            &[s("handoff"), s("note"), s("--for"), s("banto"), s("remember-the-operator-task")],
            &caller,
        ).await;
        assert!(note.is_ok(), "{}", note.message());
        let shell = d.materialize_context_kaish_rc(
            "create-operator", caller.principal_id, parent, caller.session_id, None,
            std::sync::Arc::new(super::super::lifecycle::NoopBlockSource),
        ).await.unwrap();
        let result = shell.execute_with_options(
            "kj context create operator --type director --as banto --env 'KJ_CHARACTER=obsolete-name'",
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
        assert!(instructions.contains("You are banto"), "{instructions}");
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

    #[tokio::test]
    async fn context_create_as_preserves_character_names_in_rc_and_handoff_advice() {
        let d = std::sync::Arc::new(test_dispatcher_rc().await);
        d.set_self_arc();
        let mut caller = test_caller();
        caller.context_id = None;
        d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
            principal_id: caller.principal_id, name: s("amy"), created_at: 1,
            retired_at: None, handoff_ctx: None,
        }).unwrap();
        for (index, name) in ["two words", "O'Brien", "null", "$literal", "1.0"].into_iter().enumerate() {
            let actor = PrincipalId::new();
            d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
                principal_id: actor, name: s(name), created_at: 1,
                retired_at: None, handoff_ctx: None,
            }).unwrap();
            let note = d.dispatch(&[s("handoff"), s("note"), s("--for"), s(name), s("name-specific-history")], &caller).await;
            assert!(note.is_ok(), "{}", note.message());
            let label = format!("named-{index}");
            let create = d.dispatch(&[s("context"), s("create"), s(&label), s("--type"), s("director"), s("--as"), s(name)], &caller).await;
            assert!(create.is_ok(), "{}", create.message());
            let id = d.kernel_db().lock().resolve_context(&label).unwrap();
            let blocks = d.block_store().block_snapshots(id).unwrap();
            let instructions = crate::llm::extract_system_prompt_sections(&blocks).join("\n");
            assert!(instructions.contains(&format!("You are {name},")), "name={name}: {instructions}");
            let handoff = blocks.iter().find(|b| b.kind == kaijutsu_types::BlockKind::Notification
                && b.content.contains("name-specific-history")).expect("performer's handoff");
            let advice = handoff.content.lines().find(|line| line.trim_start().starts_with("kj handoff note"))
                .expect("handoff command").trim();
            let shell = d.materialize_context_kaish_rc(
                "handoff-advice", caller.principal_id, id, caller.session_id, None,
                std::sync::Arc::new(super::super::lifecycle::NoopBlockSource),
            ).await.unwrap();
            let result = shell.execute_with_options(advice, kaish_kernel::ExecuteOptions::default()).await.unwrap();
            assert!(result.ok(), "name={name}, advice={advice}: {result:?}");
            let tail = d.dispatch(&[s("handoff"), s("tail"), s(name)], &caller).await;
            assert!(tail.is_ok(), "{}", tail.message());
            assert!(tail.message().contains("what you did, what is next"), "name={name}, advice={advice}, result={result:?}, tail={}", tail.message());
        }
    }

    #[tokio::test]
    async fn context_create_as_refuses_unknown_or_retired_without_mutation() {
        let d = test_dispatcher().await;
        let mut caller = test_caller();
        caller.context_id = None;
        d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
            principal_id: PrincipalId::new(), name: s("retired"), created_at: 1,
            retired_at: Some(2), handoff_ctx: None,
        }).unwrap();
        let before = d.kernel_db().lock().list_documents().unwrap().len();
        for (name, expected) in [("missing", "no character named"), ("retired", "is retired")] {
            let result = d.dispatch(
                &[s("context"), s("create"), s("refused"), s("--as"), s(name)], &caller,
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
                handoff_ctx: None,
            }).unwrap();
            db.set_default_approval_reviewer(amy).unwrap();
        }
        let context_id = ContextId::new();
        let row = crate::kernel_db::ContextRow {
            context_id,
            label: Some(s("must-not-insert")),
            provider: None,
            model: None,
            system_prompt: None,
            consent_mode: ConsentMode::Collaborative,
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
            cast_id: None,
            origin_host: None,
            played_by: Some(performer),
            reviewer_id: None,
            director_id: Some(amy),
        };
        let error = insert_new_context_checked(&d.kernel_db().lock(), &row, None).unwrap_err();
        assert!(error.to_string().contains("performer must name a live character"), "{error}");
        let db = d.kernel_db().lock();
        assert!(db.get_context(context_id).unwrap().is_none());
        assert!(!db.list_documents().unwrap().iter().any(|doc| doc.document_id == context_id));
    }

    #[tokio::test]
    async fn final_context_insert_rejects_a_reviewer_changed_to_the_performer() {
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
                handoff_ctx: None,
            }).unwrap();
            db.set_default_approval_reviewer(amy).unwrap();
            db.grant_approval_delegation(performer, performer, amy).unwrap();
        }
        let context_id = ContextId::new();
        let row = crate::kernel_db::ContextRow {
            context_id,
            label: Some(s("must-not-self-review")),
            provider: None,
            model: None,
            system_prompt: None,
            consent_mode: ConsentMode::Collaborative,
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
            cast_id: None,
            origin_host: None,
            played_by: Some(performer),
            reviewer_id: None,
            director_id: Some(performer),
        };
        let error = insert_new_context_checked(&d.kernel_db().lock(), &row, None).unwrap_err();
        assert!(error.to_string().contains("cannot review its own work"), "{error}");
        let db = d.kernel_db().lock();
        assert!(db.get_context(context_id).unwrap().is_none());
        assert!(!db.list_documents().unwrap().iter().any(|doc| doc.document_id == context_id));
    }

    #[tokio::test]
    async fn context_create_without_as_keeps_performer_unset() {
        let d = test_dispatcher().await;
        let mut caller = test_caller();
        caller.context_id = None;
        d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
            principal_id: caller.principal_id, name: s("requester"), created_at: 1,
            retired_at: None, handoff_ctx: None,
        }).unwrap();
        let result = d.dispatch(&[s("context"), s("create"), s("unassigned")], &caller).await;
        assert!(result.is_ok(), "{}", result.message());
        let db = d.kernel_db().lock();
        let row = db.get_context(db.resolve_context("unassigned").unwrap()).unwrap().unwrap();
        assert_eq!(row.created_by, caller.principal_id);
        assert_eq!(row.played_by, None);
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
        let result = d
            .dispatch(
                &[s("context"), s("create"), s("--type"), s("codr"), s("x")],
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

    #[tokio::test]
    async fn context_create_duplicate_label() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);

        let c = caller_with_context(parent);
        // First create succeeds
        let r1 = d.dispatch(&[s("context"), s("create"), s("dup")], &c).await;
        assert!(r1.is_ok());

        // Second create with same label should fail
        let r2 = d.dispatch(&[s("context"), s("create"), s("dup")], &c).await;
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

    #[tokio::test]
    async fn context_set_model() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("target"), None, principal);

        // Register mock provider so validation passes
        {
            use crate::llm::{MockClient, Provider};
            use std::sync::Arc;
            let mock = Arc::new(Provider::Mock(MockClient::new("mock")));
            let mut registry = d.kernel().llm().write().await;
            registry.register("mock", mock);
        }

        let c = caller_with_context(ctx);
        let result = d
            .dispatch(
                &[
                    s("context"),
                    s("set"),
                    s("."),
                    s("--model"),
                    s("mock/test-model"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "set failed: {}", result.message());
        assert!(
            result.message().contains("model="),
            "msg: {}",
            result.message()
        );

        // Verify in DriftRouter
        let router = d.drift_router().read();
        let handle = router.get(ctx).unwrap();
        assert_eq!(handle.provider.as_deref(), Some("mock"));
        assert_eq!(handle.model.as_deref(), Some("test-model"));
    }

    /// `--env` validates its key at write time, after `--model` has already
    /// been written. One failed field must leave the row as it was.
    #[tokio::test]
    async fn context_set_partial_failure_commits_nothing() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("atomic"), None, principal);
        {
            use crate::llm::{MockClient, Provider};
            use std::sync::Arc;
            let mock = Arc::new(Provider::Mock(MockClient::new("mock")));
            let mut registry = d.kernel().llm().write().await;
            registry.register("mock", mock);
        }
        let before = d.kernel_db().lock().get_context(ctx).unwrap().unwrap();
        assert!(before.model.is_none(), "fixture starts without a model");

        let c = caller_with_context(ctx);
        let result = d
            .dispatch(
                &[
                    s("context"),
                    s("set"),
                    s("."),
                    s("--model"),
                    s("mock/test-model"),
                    s("--env"),
                    s("1BAD=y"),
                ],
                &c,
            )
            .await;
        assert!(!result.is_ok(), "an invalid env key must fail the whole set");

        let after = d.kernel_db().lock().get_context(ctx).unwrap().unwrap();
        assert_eq!(
            after.model, before.model,
            "model must not be committed when a later field fails"
        );
        assert!(
            d.kernel_db().lock().get_context_env(ctx).unwrap().is_empty(),
            "no env row either"
        );
    }

    #[tokio::test]
    async fn context_set_invalid_provider_errors() {
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
                    s("--model"),
                    s("nonexistent/foo"),
                ],
                &c,
            )
            .await;
        assert!(!result.is_ok(), "should fail: {}", result.message());
        assert!(
            result.message().contains("unknown provider"),
            "expected 'unknown provider' error, got: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn context_set_colon_provider_model_errors_with_slash_hint() {
        // The `provider:model` footgun: a bare spec whose `:`-prefix names a
        // real provider almost certainly meant the slash form. Fail loud at
        // set time with a hint, instead of silently storing the literal on the
        // default provider and only erroring at turn time.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("target"), None, principal);
        {
            use crate::llm::{MockClient, Provider};
            use std::sync::Arc;
            let mock = Arc::new(Provider::Mock(MockClient::new("mock")));
            let mut registry = d.kernel().llm().write().await;
            registry.register("mock", mock);
        }

        let c = caller_with_context(ctx);
        let result = d
            .dispatch(
                &[
                    s("context"),
                    s("set"),
                    s("."),
                    s("--model"),
                    s("mock:test-model"),
                ],
                &c,
            )
            .await;
        assert!(!result.is_ok(), "colon form should fail: {}", result.message());
        assert!(
            result.message().contains("provider:model")
                && result.message().contains("mock/test-model"),
            "expected slash hint, got: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn context_set_bare_colon_model_is_not_mistaken_for_provider() {
        // An ollama-style tag like `gemma4:31b` must NOT trip the provider:model
        // hint — `gemma4` is not a registered provider. This collision is the
        // reason the parser separates on `/`, never `:`.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("target"), None, principal);
        {
            use crate::llm::{MockClient, Provider};
            use std::sync::Arc;
            let mock = Arc::new(Provider::Mock(MockClient::new("mock")));
            let mut registry = d.kernel().llm().write().await;
            registry.register("mock", mock);
        }

        let c = caller_with_context(ctx);
        let result = d
            .dispatch(
                &[s("context"), s("set"), s("."), s("--model"), s("gemma4:31b")],
                &c,
            )
            .await;
        assert!(
            !result.message().contains("provider:model"),
            "ollama-style tag must not trip the provider:model hint: {}",
            result.message()
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
    async fn context_move_reparent() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let a = register_context(&d, Some("a"), None, principal);
        let b = register_context(&d, Some("b"), None, principal);
        let child = register_context(&d, Some("child"), Some(a), principal);

        // Insert original structural edge a → child
        {
            let db = d.kernel_db().lock();
            db.insert_edge(&ContextEdgeRow {
                edge_id: uuid::Uuid::now_v7(),
                source_id: a,
                target_id: child,
                kind: EdgeKind::Structural,
                metadata: None,
                created_at: kaijutsu_types::now_millis() as i64,
            })
            .unwrap();
        }

        let c = caller_with_context(a);
        let result = d
            .dispatch(&[s("context"), s("move"), s("child"), s("b")], &c)
            .await;
        assert!(result.is_ok(), "move failed: {}", result.message());
        assert!(
            result.message().contains("moved"),
            "msg: {}",
            result.message()
        );

        // Verify new parent
        let db = d.kernel_db().lock();
        let parents = db.structural_parents(child).unwrap();
        assert_eq!(parents.len(), 1);
        assert_eq!(parents[0].context_id, b);
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

        let c = confirmed_caller(parent);
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
        {
            let db = d.kernel_db().lock();
            for (source, target) in [(root, old), (old, child)] {
                db.insert_edge(&ContextEdgeRow {
                    edge_id: uuid::Uuid::now_v7(),
                    source_id: source,
                    target_id: target,
                    kind: EdgeKind::Structural,
                    metadata: None,
                    created_at: kaijutsu_types::now_millis() as i64,
                })
                .unwrap();
            }
        }

        let c = confirmed_caller(root);
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
        let parents = db.structural_parents(child).unwrap();
        assert_eq!(parents.len(), 1, "the child lost its parent edge");
        assert_eq!(parents[0].context_id, old);
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

        let c = confirmed_caller(parent);
        let result = d
            .dispatch(&[s("context"), s("archive"), s("ROOT-old")], &c)
            .await;
        assert!(result.is_ok(), "archive failed: {}", result.message());

        let c = caller_with_context(parent);
        let result = d
            .dispatch(&[s("context"), s("create"), s("ROOT-old")], &c)
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

        let c = confirmed_caller(parent);
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

        let c = caller_with_context(ctx);
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
        let c = caller_with_context(ctx);

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

        // already demoted → archived, and the drift router reflects it
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

    #[tokio::test]
    async fn context_promote_and_demote_are_not_capability_gated() {
        // Same authority level as conclude: an unprivileged caller whose
        // context binding denies everything (Operator included) can still
        // promote/demote — unlike `archive`/`remove`, which `require_cap`
        // would deny here. Deny-all is written straight to the DB — the
        // authoritative store `require_cap` reads (see
        // `drive_denied_without_drive_capability` for the same pattern).
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("plain"), None, principal);
        d.kernel_db()
            .lock()
            .upsert_context_binding(ctx, &crate::mcp::ContextToolBinding::new())
            .unwrap();
        let c = caller_with_context(ctx); // caller_with_context: privileged = false

        let result = d
            .dispatch(&[s("context"), s("promote"), s("plain")], &c)
            .await;
        assert!(
            result.is_ok(),
            "promote should not be capability-gated: {}",
            result.message()
        );

        let result = d
            .dispatch(&[s("context"), s("demote"), s("plain")], &c)
            .await;
        assert!(
            result.is_ok(),
            "demote should not be capability-gated: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn context_promote_surfaces_the_ring_full_error() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();

        for i in 0..10 {
            let ctx = register_context(&d, Some(&format!("seat-{i}")), None, principal);
            let c = caller_with_context(ctx);
            let result = d
                .dispatch(&[s("context"), s("promote"), s(&format!("seat-{i}"))], &c)
                .await;
            assert!(result.is_ok(), "seat {i} promote failed: {}", result.message());
        }

        let overflow = register_context(&d, Some("overflow"), None, principal);
        let c = caller_with_context(overflow);
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

    #[tokio::test]
    async fn context_demote_on_archived_errors_loudly() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);
        let target = register_context(&d, Some("gone"), Some(parent), principal);

        let c = confirmed_caller(parent);
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

        let c = confirmed_caller(parent);
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

        let c = confirmed_caller(parent);
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
    async fn context_remove_requires_latch() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);
        let _target = register_context(&d, Some("victim"), Some(parent), principal);

        let c = caller_with_context(parent);
        let result = d
            .dispatch(&[s("context"), s("remove"), s("victim")], &c)
            .await;
        assert!(result.is_latch(), "expected latch, got: {:?}", result);
    }

    #[tokio::test]
    async fn context_remove_confirmed() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);
        let target = register_context(&d, Some("target"), Some(parent), principal);

        let c = confirmed_caller(parent);
        let result = d
            .dispatch(&[s("context"), s("remove"), s("target")], &c)
            .await;
        assert!(result.is_ok(), "remove failed: {}", result.message());

        // Verify gone from DB
        let db = d.kernel_db().lock();
        assert!(db.get_context(target).unwrap().is_none());

        // Verify gone from DriftRouter
        drop(db);
        let router = d.drift_router().read();
        assert!(router.get(target).is_none());
    }

    #[tokio::test]
    async fn context_remove_cannot_remove_current() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("current"), None, principal);

        let c = confirmed_caller(ctx);
        let result = d
            .dispatch(&[s("context"), s("remove"), s("current")], &c)
            .await;
        assert!(!result.is_ok(), "should not allow removing current context");
    }

    /// CHARACTERIZATION: `kj context remove` (once confirmed) kills any
    /// background host processes the removed context still owns
    /// (`background_exec.rs`'s `kill_all_for_context`, wired at the tail of
    /// `context_remove` above) — a removed context's processes are killed,
    /// not orphaned into invisibility. Goes through the REAL `kj context
    /// remove` verb (not a direct `kill_all_for_context` call, which
    /// `background_exec::tests::kill_all_for_context_only_touches_that_context`
    /// already pins) so this test proves the wiring, not just the primitive.
    ///
    /// Reaches into `crate::background_exec::spawn_background` directly
    /// (crate-internal API, not the `shell` MCP tool) to keep this test
    /// decoupled from broker/binding setup — if `spawn_background`'s
    /// signature disappears in the kaish-job-system swap, THIS call site is
    /// what needs rewriting, not the `kj context remove` behavior under test.
    #[tokio::test]
    async fn context_remove_kills_owned_background_processes() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);
        let target = register_context(&d, Some("victim"), Some(parent), principal);
        d.block_store()
            .create_document(target, kaijutsu_types::DocKind::Conversation, None)
            .unwrap();

        let block_id = d
            .block_store()
            .insert_block_as(
                target,
                None,
                None,
                kaijutsu_types::Role::Tool,
                kaijutsu_types::BlockKind::ToolResult,
                String::new(),
                kaijutsu_types::Status::Running,
                kaijutsu_types::ContentType::Plain,
                Some(principal),
            )
            .unwrap();

        let registry = d.kernel().background_processes();
        let bg_id = crate::background_exec::spawn_background(
            registry,
            d.block_store(),
            crate::background_exec::SpawnBackgroundParams {
                command: "sleep 30".to_string(),
                cwd: std::env::temp_dir(),
                env: vec![(
                    "PATH".to_string(),
                    std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string()),
                )],
                context_id: target,
                principal_id: principal,
                block_id,
            },
        )
        .unwrap();

        // Confirm it's actually running before we remove its context.
        let wait_start = std::time::Instant::now();
        loop {
            if registry.get_for_context(bg_id, target).filter(|s| s.status == "running").is_some() {
                break;
            }
            assert!(wait_start.elapsed() < std::time::Duration::from_secs(2), "background process never reported running");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let c = confirmed_caller(parent);
        let result = d
            .dispatch(&[s("context"), s("remove"), s("victim")], &c)
            .await;
        assert!(result.is_ok(), "context remove should succeed: {}", result.message());

        // The background process must be killed as a side effect of removing
        // its owning context — never left `running` with an orphaned entry.
        let wait_start = std::time::Instant::now();
        loop {
            match registry.get_for_context(bg_id, target) {
                Some(snap) if snap.status == "killed" => break,
                Some(_) => {}
                None => panic!("entry vanished from the registry instead of being marked killed"),
            }
            assert!(
                wait_start.elapsed() < std::time::Duration::from_secs(5),
                "background process was never killed after its owning context was removed"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn context_set_cwd() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("target"), None, principal);

        let c = caller_with_context(ctx);
        let result = d
            .dispatch(
                &[s("context"), s("set"), s("."), s("--cwd"), s("/tmp/work")],
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
        assert_eq!(shell.cwd, Some("/tmp/work".into()));
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

    /// `kj context info`'s JSON carries the raw `provider`/`model` row
    /// columns, but a consumer that wants to know what will ACTUALLY run
    /// has to walk the resolution ladder itself (deepseek review P4). This
    /// asserts the structured record and the human text both surface the
    /// SAME resolved backend/model/source `resolve_context_model` (and `kj
    /// model`) would produce — for an explicit context override, so
    /// `resolved_source` must read "context".
    #[tokio::test]
    async fn context_info_data_carries_the_resolved_effective_model() {
        use crate::kj::KjResult;
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("resolved-ctx"), None, principal);

        {
            use crate::llm::{MockClient, Provider};
            use std::sync::Arc;
            let mock = Arc::new(Provider::Mock(MockClient::new("mock")));
            let mut registry = d.kernel().llm().write().await;
            registry.register("mock", mock);
        }

        let c = caller_with_context(ctx);
        let set = d
            .dispatch(
                &[s("context"), s("set"), s("."), s("--model"), s("mock/test-model")],
                &c,
            )
            .await;
        assert!(set.is_ok(), "set failed: {}", set.message());

        let result = d.dispatch(&[s("context"), s("info")], &c).await;
        assert!(result.is_ok(), "info failed: {}", result.message());
        assert!(
            result.message().contains("Resolved: mock/test-model (context)"),
            "human text must show the resolved model: {}",
            result.message()
        );

        match result {
            KjResult::Ok { data: Some(v), .. } => {
                assert_eq!(v["resolved_backend"], "mock");
                assert_eq!(v["resolved_model"], "test-model");
                assert_eq!(v["resolved_source"], "context");
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    /// The other end of the ladder: a fresh context with no override, no
    /// cast, and a registry with no default at all must resolve to
    /// `null`/`null`/"default" — an honest absence, never a fabricated
    /// fallback pair.
    #[tokio::test]
    async fn context_info_data_resolved_model_is_null_when_nothing_configured() {
        use crate::kj::KjResult;
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("unresolved-ctx"), None, principal);

        let c = caller_with_context(ctx);
        let result = d.dispatch(&[s("context"), s("info")], &c).await;
        assert!(result.is_ok(), "info failed: {}", result.message());

        match result {
            KjResult::Ok { data: Some(v), .. } => {
                assert!(v["resolved_backend"].is_null(), "got {v}");
                assert!(v["resolved_model"].is_null(), "got {v}");
                assert_eq!(v["resolved_source"], "default");
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
        let principal = PrincipalId::new();

        // A 100_000-token window on a "mock/big-model" pair — chosen so the
        // fill fractions below (25%, 200%) are exactly representable in
        // binary floating point, ruling out an epsilon-fudged assertion.
        {
            use crate::llm::{BackendConfig, BackendKind, ModelInfo};
            // Named "mock" to match the context rows below; the KIND is what
            // picks a client, and nothing here builds one.
            let mut cfg = BackendConfig::new("mock", BackendKind::Mock);
            cfg.models.insert(
                "big-model".to_string(),
                ModelInfo {
                    context_window: Some(100_000),
                    extra: None,
                },
            );
            let mut registry = d.kernel().llm().write().await;
            registry.set_backends(vec![cfg]);
        }

        // Comfortably under the window: 20_000 + 5_000 = 25_000 / 100_000 = 25%.
        let ctx_under = register_context(&d, Some("usage-window-under"), None, principal);
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
        let ctx_over = register_context(&d, Some("usage-window-over"), None, principal);
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

    #[tokio::test]
    async fn context_set_bare_model_resolves_default_provider() {
        // `--model <bare>` (no provider) resolves the provider from the
        // registry default and configures the live DriftRouter handle —
        // parity with `kj fork`. Before the fix it only touched the DB column.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("target"), None, principal);

        {
            use crate::llm::{MockClient, Provider};
            use std::sync::Arc;
            let mock = Arc::new(Provider::Mock(MockClient::new("mock")));
            let mut registry = d.kernel().llm().write().await;
            registry.register("mock", mock);
            assert!(registry.set_default("mock"), "default should set");
        }

        let c = caller_with_context(ctx);
        let result = d
            .dispatch(
                &[s("context"), s("set"), s("."), s("--model"), s("bare-model")],
                &c,
            )
            .await;
        assert!(result.is_ok(), "set failed: {}", result.message());

        let router = d.drift_router().read();
        let handle = router.get(ctx).unwrap();
        assert_eq!(
            handle.provider.as_deref(),
            Some("mock"),
            "bare model should resolve the default provider"
        );
        assert_eq!(handle.model.as_deref(), Some("bare-model"));
    }

    #[tokio::test]
    async fn context_set_bare_model_no_default_errors() {
        // A bare model name with no default provider configured must error,
        // not silently update only the DB column.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("target"), None, principal);

        let c = caller_with_context(ctx);
        let result = d
            .dispatch(
                &[s("context"), s("set"), s("."), s("--model"), s("orphan-model")],
                &c,
            )
            .await;
        assert!(!result.is_ok(), "should fail: {}", result.message());
        assert!(
            result.message().contains("no provider configured"),
            "msg: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn context_create_with_model_configures_drift() {
        // create parity with fork: `--model` is applied inline, not via a
        // follow-up `kj context set`.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);

        // Register mock provider so validation passes.
        {
            use crate::llm::{MockClient, Provider};
            use std::sync::Arc;
            let mock = Arc::new(Provider::Mock(MockClient::new("mock")));
            let mut registry = d.kernel().llm().write().await;
            registry.register("mock", mock);
        }

        let c = caller_with_context(parent);
        let result = d
            .dispatch(
                &[
                    s("context"),
                    s("create"),
                    s("kid"),
                    s("--model"),
                    s("mock/test-model"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        let id = {
            let db = d.kernel_db().lock();
            db.resolve_context("kid").expect("kid should exist")
        };
        let router = d.drift_router().read();
        let handle = router.get(id).expect("kid registered in drift");
        assert_eq!(handle.provider.as_deref(), Some("mock"));
        assert_eq!(handle.model.as_deref(), Some("test-model"));
    }

    #[tokio::test]
    async fn context_create_resolves_model_alias() {
        // `--model local` must expand the registry alias to its concrete
        // provider/model BEFORE storage — otherwise "local" ships to the default
        // provider at turn time and 404s (`not_found_error: model: local`).
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);

        // Register a provider + a `local` alias pointing at it.
        {
            use crate::llm::ModelAlias;
            use crate::llm::{MockClient, Provider};
            use std::collections::HashMap;
            use std::sync::Arc;
            let mock = Arc::new(Provider::Mock(MockClient::new("lemonade")));
            let mut registry = d.kernel().llm().write().await;
            registry.register("lemonade", mock);
            let mut aliases = HashMap::new();
            aliases.insert(
                "local".to_string(),
                ModelAlias {
                    backend: "lemonade".to_string(),
                    model: "Gemma-4-E4B-it-GGUF".to_string(),
                },
            );
            registry.set_model_aliases(aliases);
        }

        let c = caller_with_context(parent);
        let result = d
            .dispatch(
                &[s("context"), s("create"), s("kid"), s("--model"), s("local")],
                &c,
            )
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        let id = {
            let db = d.kernel_db().lock();
            db.resolve_context("kid").expect("kid should exist")
        };
        // Both the drift handle and the persisted row carry the RESOLVED target,
        // never the literal alias "local".
        {
            let router = d.drift_router().read();
            let handle = router.get(id).expect("kid registered in drift");
            assert_eq!(handle.provider.as_deref(), Some("lemonade"));
            assert_eq!(handle.model.as_deref(), Some("Gemma-4-E4B-it-GGUF"));
        }
        let db = d.kernel_db().lock();
        let row = db.get_context(id).unwrap().expect("kid row");
        assert_eq!(row.provider.as_deref(), Some("lemonade"));
        assert_eq!(
            row.model.as_deref(),
            Some("Gemma-4-E4B-it-GGUF"),
            "stored model must be the resolved alias target, not 'local'"
        );
    }

    #[tokio::test]
    async fn context_set_resolves_model_alias() {
        // The same resolution must apply to `kj context set --model <alias>`
        // (create and set share `resolve_context_config`).
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let target = register_context(&d, Some("target"), None, principal);

        {
            use crate::llm::ModelAlias;
            use crate::llm::{MockClient, Provider};
            use std::collections::HashMap;
            use std::sync::Arc;
            let mock = Arc::new(Provider::Mock(MockClient::new("lemonade")));
            let mut registry = d.kernel().llm().write().await;
            registry.register("lemonade", mock);
            let mut aliases = HashMap::new();
            aliases.insert(
                "local".to_string(),
                ModelAlias {
                    backend: "lemonade".to_string(),
                    model: "Gemma-4-E4B-it-GGUF".to_string(),
                },
            );
            registry.set_model_aliases(aliases);
        }

        let c = caller_with_context(target);
        let result = d
            .dispatch(&[s("context"), s("set"), s("--model"), s("local")], &c)
            .await;
        assert!(result.is_ok(), "set failed: {}", result.message());

        let db = d.kernel_db().lock();
        let row = db.get_context(target).unwrap().expect("target row");
        assert_eq!(row.provider.as_deref(), Some("lemonade"));
        assert_eq!(row.model.as_deref(), Some("Gemma-4-E4B-it-GGUF"));
    }

    // ── Track D: cast-on-context ─────────────────────────────────────────

    #[tokio::test]
    async fn context_create_with_cast_assigns_it() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);
        let c = caller_with_context(parent);

        d.dispatch(&[s("cast"), s("create"), s("house")], &c).await;

        let result = d
            .dispatch(
                &[s("context"), s("create"), s("kid"), s("--cast"), s("house")],
                &c,
            )
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        let db = d.kernel_db().lock();
        let id = db.resolve_context("kid").expect("kid should exist");
        let row = db.get_context(id).unwrap().expect("kid row");
        let cast = db.get_cast(row.cast_id.expect("cast_id set")).unwrap().unwrap();
        assert_eq!(cast.label, "house");
    }

    #[tokio::test]
    async fn context_create_with_unknown_cast_fails_loud_listing_known_casts() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);
        let c = caller_with_context(parent);

        d.dispatch(&[s("cast"), s("create"), s("house")], &c).await;

        let result = d
            .dispatch(
                &[s("context"), s("create"), s("kid"), s("--cast"), s("nonesuch")],
                &c,
            )
            .await;
        assert!(!result.is_ok(), "unknown cast must be rejected");
        assert!(
            result.message().contains("unknown cast") && result.message().contains("house"),
            "error should list known casts: {}",
            result.message()
        );

        // The context must not have been created (orphan-context guard).
        let db = d.kernel_db().lock();
        assert!(db.resolve_context("kid").is_err(), "kid must not exist");
    }

    #[tokio::test]
    async fn context_set_assigns_and_unset_clears_cast() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let target = register_context(&d, Some("target"), None, principal);
        let c = caller_with_context(target);

        d.dispatch(&[s("cast"), s("create"), s("house")], &c).await;

        let result = d
            .dispatch(&[s("context"), s("set"), s("--cast"), s("house")], &c)
            .await;
        assert!(result.is_ok(), "set --cast failed: {}", result.message());
        {
            let db = d.kernel_db().lock();
            let row = db.get_context(target).unwrap().unwrap();
            let cast = db.get_cast(row.cast_id.expect("cast_id set")).unwrap().unwrap();
            assert_eq!(cast.label, "house");
        }

        let result = d
            .dispatch(&[s("context"), s("unset"), s("--cast")], &c)
            .await;
        assert!(result.is_ok(), "unset --cast failed: {}", result.message());
        let db = d.kernel_db().lock();
        let row = db.get_context(target).unwrap().unwrap();
        assert!(row.cast_id.is_none(), "cast cleared");
    }

    #[tokio::test]
    async fn context_info_and_list_surface_the_cast_label() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let target = register_context(&d, Some("staffed"), None, principal);
        let c = caller_with_context(target);

        d.dispatch(&[s("cast"), s("create"), s("house")], &c).await;
        d.dispatch(&[s("context"), s("set"), s("--cast"), s("house")], &c)
            .await;

        let info = d.dispatch(&[s("context"), s("info")], &c).await;
        assert!(info.is_ok());
        assert!(info.message().contains("Cast:    house"), "info: {}", info.message());

        let list = d.dispatch(&[s("context"), s("list")], &c).await;
        assert!(list.is_ok());
        assert!(list.message().contains("[cast:house]"), "list: {}", list.message());
    }

    #[tokio::test]
    async fn context_create_name_alias() {
        // `--name` / `-n` is accepted as an alias for the positional label,
        // matching `kj fork`.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);

        let c = caller_with_context(parent);
        let result = d
            .dispatch(&[s("context"), s("create"), s("--name"), s("aliased")], &c)
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
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);

        let c = caller_with_context(parent);
        let result = d
            .dispatch(
                &[
                    s("context"),
                    s("create"),
                    s("kid"),
                    s("--cwd"),
                    s("/tmp/work"),
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
        assert_eq!(shell.cwd, Some("/tmp/work".into()));
        let env = db.get_context_env(id).unwrap();
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].key, "RUST_LOG");
        assert_eq!(env[0].value, "debug");
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

        // Default type: no beat command.
        let r = d
            .dispatch(&[s("context"), s("create"), s("plain")], &c)
            .await;
        assert!(r.is_ok(), "default create failed: {}", r.message());
        assert!(
            rx.try_recv().is_err(),
            "a non-musician context must not arm the beat"
        );

        // Musician type: arms, with a track derived from the label.
        let r = d
            .dispatch(
                &[s("context"), s("create"), s("bassline"), s("--type"), s("musician")],
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

    #[tokio::test]
    async fn context_create_invalid_provider_leaves_no_orphan() {
        // A bad `--model` must be rejected BEFORE the context is created —
        // crashing the command is preferred over leaving an orphan context.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);

        let c = caller_with_context(parent);
        let result = d
            .dispatch(
                &[
                    s("context"),
                    s("create"),
                    s("kid"),
                    s("--model"),
                    s("nonexistent/foo"),
                ],
                &c,
            )
            .await;
        assert!(!result.is_ok(), "should fail: {}", result.message());
        assert!(
            result.message().contains("unknown provider"),
            "msg: {}",
            result.message()
        );

        // No half-created context left behind.
        let db = d.kernel_db().lock();
        assert!(
            db.resolve_context("kid").is_err(),
            "failed create must not leave an orphan context"
        );
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
    #[tokio::test]
    async fn context_prompt_uses_rc_owned_sections() {
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
            retired_at: None, handoff_ctx: None,
        }).unwrap();

        let create = d
            .dispatch(
                &[s("context"), s("create"), s("c1"), s("--type"), s("coder")],
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

        let shared = "Kaijutsu (会術・かいじゅつ) — the art of meeting.";
        assert_eq!(
            msg.match_indices(shared).count(),
            1,
            "the opted-in coder receives the shared section exactly once: {msg}"
        );
        assert!(
            !msg.contains("LEGACY-KERNEL-PROMPT-MUST-NOT-APPEAR"),
            "the old /config/kernel/system.md input must be ignored: {msg}"
        );
        // rc: S00-stance.kai's real output — both branches (crisp/guided)
        // share this opening line, so this holds regardless of which the
        // `case` picked for the (unconfigured) resolved model.
        assert!(
            msg.contains("You are coding here"),
            "rc-produced coder stance missing from prompt: {msg}"
        );

        // Order is the rc sort order: shared S00-base → coder S00-stance → situation.
        let base_pos = msg.find(shared).expect("shared section present");
        let rc_pos = msg.find("You are coding here").expect("rc present");
        let situation_pos = msg.find("<situation>").expect("situation present");
        assert!(
            base_pos < rc_pos && rc_pos < situation_pos,
            "expected shared → role → situation order; got shared={base_pos}, rc={rc_pos}, \
             situation={situation_pos}\nfull:\n{msg}"
        );

        let create = d
            .dispatch(
                &[s("context"), s("create"), s("m1"), s("--type"), s("musician")],
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
            !musician_prompt.contains(shared),
            "an unlinked type must receive no shared base: {musician_prompt}"
        );
        assert!(
            musician_prompt.contains("You're a musician here"),
            "musician's own role section is still present: {musician_prompt}"
        );
    }

    /// Rc source files are read when a lifecycle runs. Editing a shared body
    /// changes a later create, never the already-authored instruction blocks
    /// in an existing context.
    #[tokio::test]
    async fn rc_source_edit_applies_only_to_later_lifecycle_runs() {
        use crate::vfs::VfsOps;

        let d = std::sync::Arc::new(test_dispatcher_rc().await);
        d.set_self_arc();
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);
        let caller = caller_with_context(parent);

        let create = d
            .dispatch(
                &[s("context"), s("create"), s("before"), s("--type"), s("coder")],
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
                .any(|section| section.contains("Kaijutsu (会術・かいじゅつ)")),
            "first context must receive the seeded shared body: {before_sections:?}"
        );

        d.kernel()
            .vfs()
            .write_all(
                std::path::Path::new("/config/rc/lib/create/S00-base.md"),
                b"CHANGED-SHARED-BASE-FOR-LATER-CREATE",
            )
            .await
            .expect("edit shared rc source");

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
                &[s("context"), s("create"), s("after"), s("--type"), s("coder")],
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
                .any(|section| section.contains("CHANGED-SHARED-BASE-FOR-LATER-CREATE")),
            "a later lifecycle run must read the edited shared source: {after_sections:?}"
        );
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

    /// Regression: model-source half of `kj context prompt` matching the
    /// turn path. Forces the KernelDb row and the live DriftRouter handle
    /// apart (the divergence `apply_context_config` normally prevents by
    /// writing both together) and asserts the verb reports the handle's
    /// model, matching what `spawn_llm_for_prompt` would actually pick —
    /// not the row's.
    #[tokio::test]
    async fn context_prompt_model_follows_drift_handle_not_row() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("modelctx"), None, principal);
        d.block_store()
            .create_document(ctx, crate::DocumentKind::Conversation, None)
            .unwrap();

        // Row says one model; the live handle (what the turn path reads)
        // says another. A real caller can't normally produce this split —
        // `apply_context_config` writes both together — but a preview
        // racing a `kj context set`, or a future path that updates one
        // without the other, can.
        {
            let db = d.kernel_db().lock();
            db.update_model(ctx, Some("row-provider"), Some("row-only-model"))
                .unwrap();
        }
        {
            let mut drift = d.drift_router().write();
            drift
                .configure_llm(ctx, "handle-provider", "handle-only-model")
                .unwrap();
        }

        let c = caller_with_context(ctx);
        let result = d.dispatch(&[s("context"), s("prompt")], &c).await;
        assert!(result.is_ok(), "prompt failed: {}", result.message());
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                assert_eq!(
                    v["resolved_model"].as_str(),
                    Some("handle-only-model"),
                    "resolved_model must come from the DriftRouter handle (the turn path's \
                     source), not the KernelDb row: {v:?}"
                );
                assert_eq!(
                    v["resolved_backend"].as_str(),
                    Some("handle-provider"),
                    "resolved_backend must come from the DriftRouter handle: {v:?}"
                );
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
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

    use crate::kj::KjCaller;
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
        // An unjoined caller: `create` without `--parent` then resolves the
        // parent to None rather than the caller's fake context id (which would
        // trip the `forked_from` foreign key).
        let caller = KjCaller {
            context_id: None,
            ..test_caller()
        };
        // No rc scripts are installed here, so nothing binds the new context.
        let result = d
            .dispatch(&[s("context"), s("create"), s("fresh")], &caller)
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
