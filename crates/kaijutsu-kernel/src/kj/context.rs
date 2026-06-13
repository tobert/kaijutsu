//! Context subcommands: list, info, switch, create, set, log, move, archive, remove, retag.

use clap::{Args, Parser, Subcommand};
use kaijutsu_types::{BlockId, ConsentMode, ContentType, ContextId, ContextState, EdgeKind};

use crate::kernel_db::{ContextEdgeRow, ContextRow, ContextShellRow};

use super::format::{
    format_context_info, format_context_table, format_context_tree, format_fork_lineage,
};
use super::parse::parse_model_spec;
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
    /// System prompt text
    #[arg(long = "system-prompt")]
    system_prompt: Option<String>,
    /// Consent mode: collaborative|autonomous
    #[arg(long)]
    consent: Option<String>,
    /// Working directory for the context's shell
    #[arg(long)]
    cwd: Option<String>,
    /// Set an env var as KEY=VALUE
    #[arg(long)]
    env: Option<String>,
    /// rc-dispatch context_type (selects which /etc/rc scripts run)
    #[arg(long = "type")]
    type_: Option<String>,
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
    Info { context: Option<String> },
    /// Print the current context.
    #[command(alias = "show")]
    Current,
    /// Switch the session to another context.
    #[command(alias = "sw")]
    Switch { context: String },
    /// Create a new context. Label is positional or `--name`.
    #[command(alias = "new")]
    Create {
        /// Label (positional form; or use --name)
        label: Option<String>,
        /// Label (flag form, fork-parity)
        #[arg(long, short = 'n')]
        name: Option<String>,
        /// Parent context to fork the structural edge from
        #[arg(long, short = 'p')]
        parent: Option<String>,
        #[command(flatten)]
        config: ContextConfigArgs,
    },
    /// Get-or-create the well-known "scratch" context.
    #[command(alias = "self")]
    Scratch,
    /// Apply settable config to an existing context (default: current).
    Set {
        context: Option<String>,
        #[command(flatten)]
        config: ContextConfigArgs,
    },
    /// Remove an env var from a context.
    Unset {
        context: Option<String>,
        /// Env var key to remove
        #[arg(long)]
        env: Option<String>,
    },
    /// Show fork lineage from a context up to root (default: current).
    Log { context: Option<String> },
    /// Reparent a context under a new parent.
    #[command(alias = "mv")]
    Move {
        context: String,
        new_parent: String,
    },
    /// Soft-delete a context (latched).
    Archive { context: String },
    /// Permanently delete a context (latched).
    #[command(alias = "rm")]
    Remove { context: String },
    /// Move a label to a different context (latched).
    Retag { label: String, context: String },
    /// Set or clear the conversation hydration window — `[0, marker] ∪ last-N`
    /// instead of the whole history (the cost guard for endless composer logs).
    Hydrate {
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
    env_spec: Option<String>,
    type_spec: Option<String>,
}

/// A `--model` spec resolved against the LLM registry: a bare model name has
/// had its provider filled in from the registry default (matching `kj fork`).
struct ResolvedModel {
    provider: Option<String>,
    model: Option<String>,
}

impl KjDispatcher {
    /// Validate user-supplied config and resolve `--model` BEFORE any mutation.
    ///
    /// Checks provider existence, consent-mode spelling, and `--env KEY=VALUE`
    /// shape, and resolves a bare model name (no `provider/` prefix) to the
    /// registry's default provider — erroring if none is configured, exactly
    /// like `kj fork`. Pure checks plus an async registry read, no DB writes,
    /// so callers can bail out cleanly without leaving a half-configured (or
    /// orphan) context behind. Returns the resolved model when `--model` was
    /// given, else `None`.
    async fn resolve_context_config(
        &self,
        cfg: &ContextConfig,
    ) -> Result<Option<ResolvedModel>, String> {
        let resolved_model = match cfg.model_spec {
            Some(ref spec) if !spec.is_empty() => {
                let (mut provider, model) = parse_model_spec(spec);
                let registry = self.kernel().llm().read().await;
                if let Some(ref p) = provider {
                    // Explicit provider — must exist.
                    if registry.get(p).is_none() {
                        return Err(format!("unknown provider '{p}'"));
                    }
                } else if let Some(ref m) = model {
                    // Bare model name — resolve provider from the registry default.
                    match registry.default_provider_name() {
                        Some(p) => provider = Some(p.to_string()),
                        None => return Err(format!("no provider configured for model '{m}'")),
                    }
                }
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
        if let Some(ref env) = cfg.env_spec
            && !env.contains('=')
        {
            return Err("--env requires KEY=VALUE format".to_string());
        }
        Ok(resolved_model)
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
    ) -> Result<Vec<String>, String> {
        let (changes, model_for_drift) = {
            let db = self.kernel_db().lock();
            let mut changes = Vec::new();
            let mut model_for_drift: Option<(String, String)> = None;

            if let Some(rm) = resolved_model {
                db.update_model(target_id, rm.provider.as_deref(), rm.model.as_deref())
                    .map_err(|e| e.to_string())?;
                // `model_spec` is the original argv string — guaranteed present
                // here since `resolved_model` is Some only when it was given.
                let spec = cfg.model_spec.as_deref().unwrap_or("?");
                changes.push(format!("model={spec}"));
                if let (Some(p), Some(m)) = (&rm.provider, &rm.model) {
                    model_for_drift = Some((p.clone(), m.clone()));
                }
            }

            // consent_spec is validated upstream; treat a parse miss as absent.
            let consent_mode = cfg
                .consent_spec
                .as_ref()
                .and_then(|s| s.parse::<ConsentMode>().ok());

            if cfg.system_prompt.is_some() || consent_mode.is_some() {
                let current = db
                    .get_context(target_id)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| "context not found".to_string())?;
                let new_prompt = cfg
                    .system_prompt
                    .as_deref()
                    .or(current.system_prompt.as_deref());
                let new_consent = consent_mode.unwrap_or(current.consent_mode);
                db.update_settings(target_id, new_prompt, new_consent)
                    .map_err(|e| e.to_string())?;
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
                db.upsert_context_shell(&shell).map_err(|e| e.to_string())?;
                changes.push(format!("cwd={cwd}"));
            }

            if let Some(ref env) = cfg.env_spec {
                // KEY=VALUE shape validated upstream.
                if let Some((key, value)) = env.split_once('=') {
                    db.set_context_env(target_id, key, value)
                        .map_err(|e| e.to_string())?;
                    changes.push(format!("env {key}={value}"));
                }
            }

            if let Some(ref t) = cfg.type_spec {
                db.update_context_type(target_id, t)
                    .map_err(|e| e.to_string())?;
                changes.push(format!("type={t}"));
            }

            (changes, model_for_drift)
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
        if matches!(
            parsed.command,
            ContextCommand::Set { .. }
                | ContextCommand::Unset { .. }
                | ContextCommand::Move { .. }
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
            ContextCommand::Info { context } => self.context_info(context.as_deref(), caller),
            ContextCommand::Current => self.context_current(caller).await,
            ContextCommand::Switch { context } => self.context_switch(&context, caller).await,
            ContextCommand::Create {
                label,
                name,
                parent,
                config,
            } => {
                self.context_create(
                    name.or(label).as_deref(),
                    parent.as_deref(),
                    config.into(),
                    caller,
                )
                .await
            }
            ContextCommand::Scratch => self.context_scratch(caller).await,
            ContextCommand::Set { context, config } => {
                self.context_set(context.as_deref(), config.into(), caller).await
            }
            ContextCommand::Unset { context, env } => {
                self.context_unset(context.as_deref(), env.as_deref(), caller)
            }
            ContextCommand::Log { context } => self.context_log(context.as_deref(), caller),
            ContextCommand::Move {
                context,
                new_parent,
            } => self.context_move(&context, &new_parent, caller).await,
            ContextCommand::Archive { context } => self.context_archive(&context, caller).await,
            ContextCommand::Remove { context } => self.context_remove(&context, caller).await,
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
                    let text = format_context_tree(&dag, caller.context_id);
                    let ids = context_handles(dag.iter().map(|(row, _)| row));
                    KjResult::ok_with_data(text, ids)
                }
                Err(e) => KjResult::Err(format!("kj context list: {e}")),
            }
        } else {
            match db.list_active_contexts() {
                Ok(contexts) => {
                    let text = format_context_table(&contexts, caller.context_id);
                    let ids = context_handles(contexts.iter());
                    KjResult::ok_with_data(text, ids)
                }
                Err(e) => KjResult::Err(format!("kj context list: {e}")),
            }
        }
    }

    fn context_info(&self, context: Option<&str>, caller: &KjCaller) -> KjResult {
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
        let mut info = format_context_info(
            &row,
            children_count,
            drift_from + drift_to,
            is_current,
            trace_id,
        );

        // Shell config — captured into the structured record below as well.
        let shell = db.get_context_shell(target_id).ok().flatten();
        if let Some(ref s) = shell
            && let Some(cwd) = &s.cwd
        {
            info.push_str(&format!("\nCwd:     {cwd}"));
        }

        // Env vars
        let env_vars = db.get_context_env(target_id).unwrap_or_default();
        if !env_vars.is_empty() {
            info.push_str("\nEnv:");
            for v in &env_vars {
                info.push_str(&format!("\n  {}={}", v.key, v.value));
            }
        }

        // Workspace paths
        let workspace_paths = db.context_workspace_paths(target_id).ok().flatten();
        let workspace_label = row
            .workspace_id
            .and_then(|wsid| db.get_workspace(wsid).ok().flatten())
            .map(|ws| ws.label);
        if let Some(paths) = workspace_paths.as_ref().filter(|p| !p.is_empty()) {
            let ws_label = workspace_label.clone().unwrap_or_else(|| "?".into());
            info.push_str(&format!("\nWorkspace: {ws_label}"));
            for p in paths {
                let ro = if p.read_only { " (ro)" } else { "" };
                info.push_str(&format!("\n  {}{ro}", p.path));
            }
        }

        // Structured record: full ids and the same fields the text view
        // surfaces, so `kaish-last` round-trips and per-field jq queries work.
        let record = serde_json::json!({
            "context_id": row.context_id.to_hex(),
            "label": row.label,
            "provider": row.provider,
            "model": row.model,
            "consent_mode": format!("{:?}", row.consent_mode),
            "context_state": format!("{:?}", row.context_state),
            "context_type": row.context_type,
            "trace_id": trace_id.map(crate::kj::format::hex32),
            "forked_from": row.forked_from.map(|id| id.to_hex()),
            "fork_kind": row.fork_kind.as_ref().map(|k| format!("{k:?}")),
            "children_count": children_count,
            "drift_count": drift_from + drift_to,
            "is_current": is_current,
            "workspace_id": row.workspace_id.map(|id| id.to_hex()),
            "workspace_label": workspace_label,
            "preset_id": row.preset_id.map(|id| id.to_hex()),
            "cwd": shell.as_ref().and_then(|s| s.cwd.clone()),
            "env": env_vars.iter()
                .map(|v| (v.key.clone(), serde_json::Value::String(v.value.clone())))
                .collect::<serde_json::Map<_, _>>(),
        });

        KjResult::ok_with_data(info, record)
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
        // /etc/rc/default/<verb>/.
        let context_type = cfg.type_spec.take().unwrap_or_else(|| "default".to_string());

        // For composers, derive the beat lane (track) from the label up front,
        // so a label that yields no valid track id fails BEFORE we create an
        // orphan context. The arm itself happens after the rc lifecycle (below).
        let composer_track = if context_type == "composer" {
            match kaijutsu_types::TrackId::new(label)
                .ok()
                .or_else(|| kaijutsu_types::TrackId::slugify(label))
            {
                Some(t) => Some(t),
                None => {
                    return KjResult::Err(format!(
                        "kj context create: composer label {label:?} yields no valid \
                         track id (slug is empty) — refusing a silent shared lane"
                    ));
                }
            }
        } else {
            None
        };

        // Validate + resolve the rest before any mutation so a typo'd
        // --model/--consent/--env can't leave an orphan context behind.
        let resolved_model = match self.resolve_context_config(&cfg).await {
            Ok(r) => r,
            Err(e) => return KjResult::Err(format!("kj context create: {e}")),
        };
        let new_id = ContextId::new();

        // Write-through: KernelDb first, then DriftRouter
        {
            let db = self.kernel_db().lock();
            let default_ws = match db.get_or_create_default_workspace(caller.principal_id) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj context create: {e}")),
            };

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
            };
            if let Err(e) = db.insert_context_with_document(&row, default_ws) {
                return KjResult::Err(format!("kj context create: {e}"));
            }

            // Insert structural edge if parent specified
            if let Some(pid) = parent_id {
                let edge = ContextEdgeRow {
                    edge_id: uuid::Uuid::now_v7(),
                    source_id: pid,
                    target_id: new_id,
                    kind: EdgeKind::Structural,
                    metadata: None,
                    created_at: kaijutsu_types::now_millis() as i64,
                };
                if let Err(e) = db.insert_edge(&edge) {
                    tracing::warn!("failed to insert structural edge: {e}");
                }
            }
        }

        // Register in DriftRouter
        {
            let mut drift = self.drift_router().write();
            if let Err(e) = drift.register(new_id, Some(label), parent_id, caller.principal_id) {
                return KjResult::Err(format!("kj context create: {e}"));
            }
        }

        // Apply settable config (model, system-prompt, consent, cwd, env) now
        // that the row and drift handle exist. Validated above; only DB I/O
        // errors surface here.
        let config_changes = match self
            .apply_context_config(new_id, &cfg, resolved_model.as_ref())
            .await
        {
            Ok(changes) => changes,
            Err(e) => return KjResult::Err(format!("kj context create: {e}")),
        };

        // Run rc create-lifecycle scripts. Failures surface as Error
        // blocks in the new context — they don't abort context creation.
        if let Err(e) = self
            .run_rc_lifecycle("create", new_id, parent_id, None, None, caller)
            .await
        {
            tracing::warn!("rc create lifecycle: {e}");
        }

        // Arm the beat for composer contexts so the scheduler drives the
        // playhead. This mirrors the capnp `CreateContext` path
        // (`create_context_inner` in kaijutsu-server/src/rpc.rs) — without it, a
        // composer created via `kj` (the path the Chameleon player-spawn rc
        // uses) is never armed: `kj transport play` is ignored ("play on
        // un-armed context") and the OODA Act never crystallizes. Absent a
        // scheduler (embedded/test) `send_beat_command` returns false → no-op.
        if let Some(track) = composer_track {
            let armed = self.kernel().send_beat_command(crate::hyoushigi::BeatCommand::Arm {
                context_id: new_id,
                policy: crate::hyoushigi::BeatPolicy::composer_default(),
                track,
            });
            if !armed {
                tracing::warn!(
                    "composer {} created but no beat scheduler is wired — it will not beat",
                    new_id.short()
                );
            }
        }

        let mut msg = format!("created context '{}' ({})", label, new_id.short());
        if !config_changes.is_empty() {
            msg.push_str(&format!(" [{}]", config_changes.join(", ")));
        }
        KjResult::ok(msg)
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

    /// `kj context set <ctx> [--model p/m] [--system-prompt text] [--consent mode] [--cwd path] [--env KEY=VALUE] [--type t]`
    async fn context_set(
        &self,
        target_arg: Option<&str>,
        cfg: ContextConfig,
        caller: &KjCaller,
    ) -> KjResult {
        // Validate + resolve the model before touching the DB.
        let resolved_model = match self.resolve_context_config(&cfg).await {
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

        match self
            .apply_context_config(target_id, &cfg, resolved_model.as_ref())
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
    /// its whole history — the cost guard for endless composer logs (design:
    /// `docs/chameleon.md`, the hydration marker). The prefix marker defaults to
    /// the context's current tail (pin everything so far, slide a window over what
    /// comes next); a composer's `create` rc sets this once. `--clear` reverts to
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

    /// `kj context archive <ctx>` — soft-delete a context (latched).
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

        if !caller.confirmed {
            // Gather stats for latch message
            let db = self.kernel_db().lock();
            let block_count = self
                .block_store()
                .get(target_id)
                .map(|e| e.doc.block_count())
                .unwrap_or(0);
            let children_count = db
                .structural_children(target_id)
                .map(|c| c.len())
                .unwrap_or(0);
            let drift_from = db
                .edges_from(target_id, Some(EdgeKind::Drift))
                .map(|e| e.len())
                .unwrap_or(0);
            let drift_to = db
                .edges_to(target_id, Some(EdgeKind::Drift))
                .map(|e| e.len())
                .unwrap_or(0);

            return KjResult::Latch {
                command: "kj context archive".to_string(),
                target: target_label,
                message: format!(
                    "{} blocks | {} children | {} drift edges",
                    block_count,
                    children_count,
                    drift_from + drift_to
                ),
            };
        }

        // Archive the target + recursive children
        let archived_ids: Vec<ContextId>;
        {
            let db = self.kernel_db().lock();
            let subtree = db.subtree_snapshot(target_id).unwrap_or_default();
            archived_ids = subtree
                .iter()
                .filter(|(row, _)| db.archive_context(row.context_id).unwrap_or(false))
                .map(|(row, _)| row.context_id)
                .collect();
        }

        // Sync the in-memory drift router with the on-disk state (M2-B3).
        // Without this the drift router still has the contexts as Live, and
        // any active session can write a drift op that resurrects them — the
        // archive-while-joined bug from the constellation flow.
        {
            let mut drift = self.drift_router().write();
            for id in &archived_ids {
                let _ = drift.set_state(*id, ContextState::Archived);
            }
        }

        // MCP subscription cleanup removed alongside the legacy MCP pool
        // in Phase 1 M5. Phase 2 will re-introduce via broker + coalescer.

        KjResult::ok(format!("archived {} context(s)", archived_ids.len()))
    }

    /// `kj context remove <ctx>` — permanently delete a context (latched).
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

        if !caller.confirmed {
            let db = self.kernel_db().lock();
            let block_count = self
                .block_store()
                .get(target_id)
                .map(|e| e.doc.block_count())
                .unwrap_or(0);
            let children_count = db
                .structural_children(target_id)
                .map(|c| c.len())
                .unwrap_or(0);

            return KjResult::Latch {
                command: "kj context remove".to_string(),
                target: target_label,
                message: format!(
                    "{} blocks | {} children — this is permanent",
                    block_count, children_count
                ),
            };
        }

        // MCP subscription cleanup removed alongside the legacy MCP pool
        // in Phase 1 M5.

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

    /// `kj context retag <label> <ctx>` — move a label to a different context (latched).
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

        if !caller.confirmed {
            let current_holder = old_holder
                .as_ref()
                .map(|r| {
                    let old_short = r.context_id.short();
                    format!(
                        "currently held by {} ({})",
                        r.label.as_deref().unwrap_or(&old_short),
                        old_short
                    )
                })
                .unwrap_or_else(|| "label is free".to_string());

            return KjResult::Latch {
                command: "kj context retag".to_string(),
                target: label.to_string(),
                message: current_holder,
            };
        }

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

#[cfg(test)]
mod tests {
    use crate::kernel_db::ContextEdgeRow;
    #[allow(unused_imports)]
    use crate::kj::KjResult;
    use crate::kj::test_helpers::*;
    use kaijutsu_types::{EdgeKind, PrincipalId};

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

        let c = caller_with_context(ctx_id);
        let result = d.dispatch(&[s("context"), s("info")], &c).await;
        assert!(result.is_ok());
        let msg = result.message();
        assert!(msg.contains("myctx *"), "output: {msg}");
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
            result.message().contains("Usage") && result.message().contains("switch"),
            "clap help should list usage + verbs: {}",
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
                kaijutsu_crdt::Role::User,
                kaijutsu_crdt::BlockKind::Text,
                "seed".to_string(),
                kaijutsu_crdt::Status::Done,
                kaijutsu_crdt::ContentType::Plain,
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
                kaijutsu_crdt::Role::User,
                kaijutsu_crdt::BlockKind::Text,
                "seed".to_string(),
                kaijutsu_crdt::Status::Done,
                kaijutsu_crdt::ContentType::Plain,
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
                kaijutsu_crdt::Role::User,
                kaijutsu_crdt::BlockKind::Text,
                "seed".to_string(),
                kaijutsu_crdt::Status::Done,
                kaijutsu_crdt::ContentType::Plain,
                Some(principal),
            )
            .unwrap();
        let c = caller_with_context(ctx);

        // Parseable BlockId, but never inserted into this context.
        let phantom = kaijutsu_crdt::BlockId::new(ctx, PrincipalId::new(), 9999).to_key();
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
    async fn context_create_composer_arms_the_beat() {
        // A composer created via `kj` MUST arm the beat scheduler. The OODA Act
        // (ABC→cell→MIDI) never fires for an un-armed context and
        // `kj transport play` is silently ignored ("play on un-armed context").
        // Regression: the arm lived only in the capnp CreateContext path
        // (`create_context_inner`), so `kj`-spawned players — the Chameleon
        // player-spawn path — were created but never beat. A non-composer must
        // NOT arm.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);

        // Own the only beat ingress so we can observe what create sends.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(d.kernel().set_beat_ingress(tx), "test owns the ingress");

        let c = caller_with_context(parent);

        // Default type: no beat command.
        let r = d
            .dispatch(&[s("context"), s("create"), s("plain")], &c)
            .await;
        assert!(r.is_ok(), "default create failed: {}", r.message());
        assert!(
            rx.try_recv().is_err(),
            "a non-composer context must not arm the beat"
        );

        // Composer type: arms, with a track derived from the label.
        let r = d
            .dispatch(
                &[s("context"), s("create"), s("bassline"), s("--type"), s("composer")],
                &c,
            )
            .await;
        assert!(r.is_ok(), "composer create failed: {}", r.message());

        let id = {
            let db = d.kernel_db().lock();
            db.resolve_context("bassline").expect("bassline exists")
        };
        let expected_track = kaijutsu_types::TrackId::new("bassline")
            .ok()
            .or_else(|| kaijutsu_types::TrackId::slugify("bassline"))
            .expect("bassline yields a valid track");
        match rx.try_recv() {
            Ok(crate::hyoushigi::BeatCommand::Arm {
                context_id, track, ..
            }) => {
                assert_eq!(context_id, id, "arms the composer we just created");
                assert_eq!(track, expected_track, "track derives from the label");
            }
            other => panic!("composer create must send BeatCommand::Arm, got {other:?}"),
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
                    handles.iter().any(|h| *h == "alpha"),
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
}
