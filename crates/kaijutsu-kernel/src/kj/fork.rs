//! Fork subcommand: spawn a child context from the current one.
//!
//! Two strategies, by KV-cache intent (copy cost is a non-issue — storage is
//! cheap):
//! - **Full fork** (default) — take the whole context into a fresh lineage = a
//!   NEW KV cache (resume-a-session-as-another-model, orchestrator repair,
//!   drift-a-summary-back). `--exclude <block>…` drops a few named blocks (the
//!   repair case); otherwise it's a plain deep copy.
//! - **Thin fork** (`--shallow` / `--compact` / `--as`) — reuse/reduce: a leaner
//!   child for a long-running iterating player. Routes through
//!   `fork_document_filtered`. (See `docs/chameleon.md` "Players are rc
//!   programs"; the open question of a thin fork's exact copy shape — last-N vs
//!   prefix-preserving — is in `docs/issues.md`.)

use std::collections::HashMap;

use clap::Parser;
use kaijutsu_types::{ConsentMode, ContentType, ContextId, ContextState, EdgeKind, ForkKind};

use crate::kernel_db::{ContextEdgeRow, ContextRow, ContextShellRow};

use super::parse::parse_model_spec;
use super::{KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug, Default)]
#[command(name = "fork", about = "Fork the current context into a child", disable_help_subcommand = true, no_binary_name = true)]
pub(crate) struct ForkArgs {
    /// Label for the child (--name/-n)
    #[arg(long, short = 'n')]
    name: Option<String>,
    /// Seed prompt; drives the child's turn
    #[arg(long)]
    prompt: Option<String>,
    /// Preset to apply to the child
    #[arg(long)]
    preset: Option<String>,
    /// Override cwd on the forked context
    #[arg(long)]
    pwd: Option<String>,
    /// Model spec provider/model (or bare model)
    #[arg(long, short = 'm')]
    model: Option<String>,
    /// Distillation model for compact forks
    #[arg(long = "distill-model")]
    distill_model: Option<String>,
    /// Shallow-fork depth (block count)
    #[arg(long)]
    depth: Option<usize>,
    /// Subtree template context ref; presence selects subtree mode
    #[arg(long = "as")]
    as_template: Option<String>,
    /// Start the child in liminal staging state
    #[arg(long, visible_alias = "staging")]
    stage: bool,
    /// Move the session to the child after forking
    #[arg(long)]
    switch: bool,
    /// Shallow fork (last N blocks)
    #[arg(long)]
    shallow: bool,
    /// Compact (distill) fork
    #[arg(long)]
    compact: bool,
    /// Exclude specific blocks from a FULL fork (repeatable). Block key in
    /// `context:agent:seq` form. The full fork copies everything else — the
    /// orchestrator-repair path ("fork X without the block that blew it up").
    #[arg(long)]
    exclude: Vec<String>,
}

/// Resolved provider+model for a fork.
struct ResolvedModel {
    provider: Option<String>,
    model: Option<String>,
    /// True when `--model` was explicitly given (needs `configure_llm` call).
    explicit: bool,
}

impl KjDispatcher {
    /// Resolve the model for a fork: parse `--model`, validate provider, or inherit from parent.
    async fn resolve_fork_model(
        &self,
        model_spec: Option<&str>,
        source_id: ContextId,
    ) -> Result<ResolvedModel, String> {
        // Read parent's provider+model from DriftRouter (before any mutations)
        let (parent_provider, parent_model) = {
            let router = self.drift_router().read();
            router
                .get(source_id)
                .map(|h| (h.provider.clone(), h.model.clone()))
                .unwrap_or((None, None))
        };

        match model_spec {
            Some(spec) => {
                let (mut provider, model) = parse_model_spec(spec);
                let registry = self.kernel().llm().read().await;
                if let Some(ref p) = provider {
                    // Explicit provider — validate it exists
                    if registry.get(p).is_none() {
                        return Err(format!("unknown provider '{p}'"));
                    }
                } else if let Some(ref m) = model {
                    // Bare model name — resolve provider from registry
                    match registry.default_provider_name() {
                        Some(p) => provider = Some(p.to_string()),
                        None => return Err(format!("no provider configured for model '{m}'")),
                    }
                }
                Ok(ResolvedModel {
                    provider,
                    model,
                    explicit: true,
                })
            }
            None => Ok(ResolvedModel {
                provider: parent_provider,
                model: parent_model,
                explicit: false,
            }),
        }
    }

    pub(crate) async fn dispatch_fork(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let args = match ForkArgs::try_parse_from(argv) {
            Ok(a) => a,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj fork: {e}"));
            }
        };

        // All fork variants snapshot a context into a child — gated on `fork`.
        if let Err(denied) = self.require_cap(caller, crate::mcp::Capability::Fork, "fork") {
            return denied;
        }

        if args.shallow {
            return self.fork_shallow(&args, caller).await;
        }
        if args.compact {
            return self.fork_compact(&args, caller).await;
        }
        if args.as_template.is_some() {
            return self.fork_subtree(&args, caller).await;
        }

        self.fork_full(&args, caller).await
    }

    /// Apply MCP fork mode exclusions to a newly forked context.
    ///
    /// Servers with `McpForkMode::Exclude` have their tools denied via ToolFilter.
    /// Called after drift.register_fork() so the context handle exists.
    async fn apply_fork_mcp_exclusions(&self, _new_id: ContextId) {
        // MCP fork-mode exclusions were removed alongside the legacy MCP
        // pool in Phase 1 M5. A Phase 2+ replacement will live against
        // ExternalMcpServer health/policy.
    }

    async fn fork_full(&self, args: &ForkArgs, caller: &KjCaller) -> KjResult {
        let label = args.name.clone();
        let prompt = args.prompt.clone();
        let preset_label = args.preset.clone();
        let pwd_override = args.pwd.clone();
        let staging = args.stage;

        let source_id = match caller.require_context() {
            Ok(id) => id,
            Err(e) => return e,
        };
        let new_id = ContextId::new();

        // Validate --model BEFORE any mutations
        let resolved = match self.resolve_fork_model(args.model.as_deref(), source_id).await {
            Ok(r) => r,
            Err(e) => return KjResult::Err(format!("kj fork: {e}")),
        };

        // Validate --exclude block ids against the source BEFORE any mutations,
        // and build the fork filter. A full fork takes the whole context; this is
        // the escape hatch to drop a few named blocks (orchestrator repair). A
        // typo/wrong-context id fails LOUD (consistent with `kj context hydrate
        // --mark`) — a silent no-op would leave the offending block in the
        // "repaired" child. Empty --exclude → the plain unfiltered full copy.
        let exclude_filter = if args.exclude.is_empty() {
            None
        } else {
            let mut ids = std::collections::HashSet::new();
            for key in &args.exclude {
                let Some(id) = kaijutsu_types::BlockId::from_key(key) else {
                    return KjResult::Err(format!("kj fork: invalid --exclude block id '{key}'"));
                };
                match self.block_store().get_block_snapshot(source_id, &id) {
                    Ok(Some(_)) => {
                        ids.insert(id.to_key());
                    }
                    Ok(None) => {
                        return KjResult::Err(format!(
                            "kj fork: --exclude block '{key}' is not in this context"
                        ));
                    }
                    Err(e) => {
                        return KjResult::Err(format!(
                            "kj fork: could not verify --exclude block '{key}': {e}"
                        ));
                    }
                }
            }
            Some(kaijutsu_crdt::ForkBlockFilter {
                exclude_block_ids: ids,
                ..Default::default()
            })
        };

        // Deep-copy the BlockStore document. Plain full copy by default; when
        // --exclude was given, route through the filtered copy (full fork still
        // takes everything else).
        let copy = match &exclude_filter {
            Some(filter) => self.block_store().fork_document_filtered(
                source_id,
                new_id,
                kaijutsu_types::now_millis(),
                filter,
            ),
            None => self.block_store().fork_document(source_id, new_id),
        };
        if let Err(e) = copy {
            return KjResult::Err(format!("kj fork: failed to copy document: {e}"));
        }

        // Write-through: KernelDb then DriftRouter
        {
            let mut db = self.kernel_db().lock();

            // Inherit workspace from source
            let source_ws = db
                .get_context(source_id)
                .ok()
                .flatten()
                .and_then(|r| r.workspace_id);

            let row = ContextRow {
                context_id: new_id,
                                label: label.clone(),
                provider: resolved.provider.clone(),
                model: resolved.model.clone(),
                system_prompt: None,
                consent_mode: ConsentMode::Collaborative,
                context_state: if staging {
                    ContextState::Staging
                } else {
                    ContextState::Live
                },
                context_type: "default".to_string(),
                created_at: kaijutsu_types::now_millis() as i64,
                created_by: caller.principal_id,
                forked_from: Some(source_id),
                fork_kind: Some(ForkKind::Full),
                archived_at: None,
                workspace_id: source_ws,
                preset_id: None,
            };
            let default_ws =
                match db.get_or_create_default_workspace(caller.principal_id) {
                    Ok(id) => id,
                    Err(e) => return KjResult::Err(format!("kj fork: {e}")),
                };
            // Context row + shell/env/binding copy land in one transaction, so
            // a failure can't strand a committed-but-misconfigured context.
            if let Err(e) = db.insert_forked_context(&row, default_ws, source_id) {
                return KjResult::Err(format!("kj fork: {e}"));
            }

            // Apply --pwd override
            if let Some(ref pwd) = pwd_override {
                let shell = ContextShellRow {
                    context_id: new_id,
                    cwd: Some(pwd.clone()),
                    updated_at: kaijutsu_types::now_millis() as i64,
                };
                if let Err(e) = db.upsert_context_shell(&shell) {
                    return KjResult::Err(format!("kj fork: failed to set --pwd: {e}"));
                }
            }

            // Structural edge: source → new
            let edge = ContextEdgeRow {
                edge_id: uuid::Uuid::now_v7(),
                source_id,
                target_id: new_id,
                kind: EdgeKind::Structural,
                metadata: None,
                created_at: kaijutsu_types::now_millis() as i64,
            };
            if let Err(e) = db.insert_edge(&edge) {
                return KjResult::Err(format!("kj fork: failed to insert structural edge: {e}"));
            }
        }

        // Register in DriftRouter (inherits parent's model)
        {
            let mut drift = self.drift_router().write();
            if let Err(e) =
                drift.register_fork(new_id, label.as_deref(), source_id, caller.principal_id)
            {
                return KjResult::Err(format!("kj fork: parent context not in router: {e}"));
            }
            // Set staging state if --stage flag was given
            if staging
                && let Err(e) = drift.set_state(new_id, ContextState::Staging)
            {
                return KjResult::Err(format!("kj fork: failed to set staging state: {e}"));
            }
            // If --model was explicit, override the inherited model
            if resolved.explicit {
                match (&resolved.provider, &resolved.model) {
                    (Some(p), Some(m)) => {
                        if let Err(e) = drift.configure_llm(new_id, p, m) {
                            return KjResult::Err(format!(
                                "kj fork: failed to configure model: {e}"
                            ));
                        }
                    }
                    _ => {
                        return KjResult::Err(
                            "kj fork: --model resolved without both provider and model".to_string(),
                        );
                    }
                }
            }
        }

        // Apply preset if requested
        if let Some(ref preset) = preset_label
            && let Err(e) = self.apply_preset(new_id, preset).await
        {
            return KjResult::Err(format!("kj fork: failed to apply preset '{preset}': {e}"));
        }

        // If --prompt given, inject a Drift block
        if let Some(note) = &prompt
            && let Err(e) = self.inject_fork_note(new_id, source_id, note)
        {
            return KjResult::Err(format!("kj fork: failed to inject fork note: {e}"));
        }

        self.apply_fork_mcp_exclusions(new_id).await;

        // Fork marker: get source label + block count for the summary
        let source_label = {
            let db = self.kernel_db().lock();
            db.get_context(source_id)
                .ok()
                .flatten()
                .and_then(|r| r.label)
        };
        let block_count = self
            .block_store()
            .block_snapshots(new_id)
            .map(|b| b.len())
            .unwrap_or(0);
        if let Err(e) = self.inject_fork_marker(
            new_id,
            source_id,
            ForkKind::Full,
            block_count,
            source_label.as_deref(),
            staging,
        ) {
            tracing::warn!("kj fork: failed to inject fork marker: {e}");
        }

        // Inherit parent's context_type so the new context's fork-side
        // rc scripts dispatch correctly. Done post-commit because the
        // original ContextRow construction defaulted to "default".
        inherit_parent_context_type(self, new_id, source_id);

        // Run rc fork-lifecycle scripts. Failures surface as Error
        // blocks in the new context — they don't abort the fork.
        if let Err(e) = self
            .run_rc_lifecycle("fork", new_id, Some(source_id), Some(ForkKind::Full), None, caller)
            .await
        {
            tracing::warn!("rc fork lifecycle: {e}");
        }

        let switch = args.switch;
        self.request_child_turn(new_id, prompt.as_deref(), staging, caller);
        let short = new_id.short();
        let display = label.as_deref().unwrap_or(&short);
        let message = format!("forked to '{}' ({})", display, short);
        self.fork_outcome(new_id, label.as_deref(), switch, message)
    }

    async fn fork_shallow(&self, args: &ForkArgs, caller: &KjCaller) -> KjResult {
        let label = args.name.clone();
        let prompt = args.prompt.clone();
        let preset_label = args.preset.clone();
        let pwd_override = args.pwd.clone();
        let staging = args.stage;
        let depth: usize = args.depth.unwrap_or(50);

        let source_id = match caller.require_context() {
            Ok(id) => id,
            Err(e) => return e,
        };
        let new_id = ContextId::new();

        // Validate --model BEFORE any mutations
        let resolved = match self.resolve_fork_model(args.model.as_deref(), source_id).await {
            Ok(r) => r,
            Err(e) => return KjResult::Err(format!("kj fork --shallow: {e}")),
        };

        // Shallow fork with block filter — include all blocks up to now;
        // max_blocks handles truncation to the most recent N.
        let filter = kaijutsu_crdt::ForkBlockFilter {
            max_blocks: Some(depth),
            exclude_compacted: true,
            ..Default::default()
        };
        let before_timestamp = kaijutsu_types::now_millis();
        if let Err(e) =
            self.block_store()
                .fork_document_filtered(source_id, new_id, before_timestamp, &filter)
        {
            return KjResult::Err(format!("kj fork --shallow: {e}"));
        }

        // Write-through
        {
            let mut db = self.kernel_db().lock();

            let source_ws = db
                .get_context(source_id)
                .ok()
                .flatten()
                .and_then(|r| r.workspace_id);

            let row = ContextRow {
                context_id: new_id,
                                label: label.clone(),
                provider: resolved.provider.clone(),
                model: resolved.model.clone(),
                system_prompt: None,
                consent_mode: ConsentMode::Collaborative,
                context_state: if staging { ContextState::Staging } else { ContextState::Live },
                context_type: "default".to_string(),
                created_at: kaijutsu_types::now_millis() as i64,
                created_by: caller.principal_id,
                forked_from: Some(source_id),
                fork_kind: Some(ForkKind::Filtered),
                archived_at: None,
                workspace_id: source_ws,
                preset_id: None,
            };
            let default_ws =
                match db.get_or_create_default_workspace(caller.principal_id) {
                    Ok(id) => id,
                    Err(e) => return KjResult::Err(format!("kj fork --shallow: {e}")),
                };
            // Context row + shell/env/binding copy land in one transaction, so
            // a failure can't strand a committed-but-misconfigured context.
            if let Err(e) = db.insert_forked_context(&row, default_ws, source_id) {
                return KjResult::Err(format!("kj fork --shallow: {e}"));
            }

            if let Some(ref pwd) = pwd_override {
                let shell = ContextShellRow {
                    context_id: new_id,
                    cwd: Some(pwd.clone()),
                    updated_at: kaijutsu_types::now_millis() as i64,
                };
                if let Err(e) = db.upsert_context_shell(&shell) {
                    return KjResult::Err(format!("kj fork --shallow: failed to set --pwd: {e}"));
                }
            }

            let edge = ContextEdgeRow {
                edge_id: uuid::Uuid::now_v7(),
                source_id,
                target_id: new_id,
                kind: EdgeKind::Structural,
                metadata: None,
                created_at: kaijutsu_types::now_millis() as i64,
            };
            if let Err(e) = db.insert_edge(&edge) {
                return KjResult::Err(format!(
                    "kj fork --shallow: failed to insert structural edge: {e}"
                ));
            }
        }

        {
            let mut drift = self.drift_router().write();
            if let Err(e) =
                drift.register_fork(new_id, label.as_deref(), source_id, caller.principal_id)
            {
                return KjResult::Err(format!(
                    "kj fork --shallow: parent context not in router: {e}"
                ));
            }
            if staging
                && let Err(e) = drift.set_state(new_id, ContextState::Staging)
            {
                return KjResult::Err(format!("kj fork --shallow: failed to set staging state: {e}"));
            }
            if resolved.explicit {
                match (&resolved.provider, &resolved.model) {
                    (Some(p), Some(m)) => {
                        if let Err(e) = drift.configure_llm(new_id, p, m) {
                            return KjResult::Err(format!(
                                "kj fork --shallow: failed to configure model: {e}"
                            ));
                        }
                    }
                    _ => {
                        return KjResult::Err(
                            "kj fork --shallow: --model resolved without both provider and model"
                                .to_string(),
                        );
                    }
                }
            }
        }

        if let Some(ref preset) = preset_label
            && let Err(e) = self.apply_preset(new_id, preset).await
        {
            return KjResult::Err(format!(
                "kj fork --shallow: failed to apply preset '{preset}': {e}"
            ));
        }

        if let Some(note) = &prompt
            && let Err(e) = self.inject_fork_note(new_id, source_id, note)
        {
            return KjResult::Err(format!(
                "kj fork --shallow: failed to inject fork note: {e}"
            ));
        }

        self.apply_fork_mcp_exclusions(new_id).await;

        let source_label = {
            let db = self.kernel_db().lock();
            db.get_context(source_id)
                .ok()
                .flatten()
                .and_then(|r| r.label)
        };
        let block_count = self
            .block_store()
            .block_snapshots(new_id)
            .map(|b| b.len())
            .unwrap_or(0);
        if let Err(e) = self.inject_fork_marker(
            new_id,
            source_id,
            ForkKind::Filtered,
            block_count,
            source_label.as_deref(),
            staging,
        ) {
            tracing::warn!("kj fork --shallow: failed to inject fork marker: {e}");
        }

        inherit_parent_context_type(self, new_id, source_id);
        if let Err(e) = self
            .run_rc_lifecycle(
                "fork",
                new_id,
                Some(source_id),
                Some(ForkKind::Filtered),
                None,
                caller,
            )
            .await
        {
            tracing::warn!("rc fork lifecycle (shallow): {e}");
        }

        // POSIX-style: drive the child's autonomous turn (if --prompt) after all
        // fork-time block injections + rc lifecycle, then honor stay-on-parent
        // default / --switch via fork_outcome.
        let switch = args.switch;
        self.request_child_turn(new_id, prompt.as_deref(), staging, caller);
        let short = new_id.short();
        let display = label.as_deref().unwrap_or(&short);
        let message = format!(
            "shallow-forked to '{}' ({}, depth {})",
            display,
            new_id.short(),
            depth
        );
        self.fork_outcome(new_id, label.as_deref(), switch, message)
    }

    async fn fork_compact(&self, args: &ForkArgs, caller: &KjCaller) -> KjResult {
        let label = args.name.clone();
        let prompt = args.prompt.clone();
        let pwd_override = args.pwd.clone();
        let staging = args.stage;
        // M5-F5: optional cheaper model for the distillation step.
        // Distillation is a one-shot summary — using Opus to summarize for
        // a Haiku follow-up is wasteful. Fall through to the source
        // context's chat model when not specified.
        let distill_model = args.distill_model.clone();

        let source_id = match caller.require_context() {
            Ok(id) => id,
            Err(e) => return e,
        };
        let new_id = ContextId::new();

        // Validate --model BEFORE any mutations
        let resolved = match self.resolve_fork_model(args.model.as_deref(), source_id).await {
            Ok(r) => r,
            Err(e) => return KjResult::Err(format!("kj fork --compact: {e}")),
        };

        // Summarize source context via LLM (use --distill-model when set).
        let summary = match self
            .summarize_with_model(source_id, None, distill_model.as_deref())
            .await
        {
            Ok(s) => s,
            Err(e) => return KjResult::Err(format!("kj fork --compact: {e}")),
        };

        // Create empty document for the new context
        if let Err(e) =
            self.block_store()
                .create_document(new_id, crate::DocumentKind::Conversation, None)
        {
            return KjResult::Err(format!("kj fork --compact: failed to create document: {e}"));
        }

        // Seed with distilled summary as a Drift block
        {
            let source_model = {
                let router = self.drift_router().read();
                router.get(source_id).and_then(|h| h.model.clone())
            };
            if let Err(e) = self.block_store().insert_drift_block(
                new_id,
                None,
                None,
                &summary,
                source_id,
                source_model,
                kaijutsu_crdt::DriftKind::Distill,
            ) {
                return KjResult::Err(format!("kj fork --compact: failed to insert summary: {e}"));
            }
        }

        // If --prompt given, inject a fork note after the summary
        if let Some(note) = &prompt
            && let Err(e) = self.inject_fork_note(new_id, source_id, note)
        {
            tracing::warn!("failed to inject fork note: {e}");
        }

        // Write-through: KernelDb then DriftRouter
        {
            let mut db = self.kernel_db().lock();

            let source_ws = db
                .get_context(source_id)
                .ok()
                .flatten()
                .and_then(|r| r.workspace_id);

            let row = ContextRow {
                context_id: new_id,
                                label: label.clone(),
                provider: resolved.provider.clone(),
                model: resolved.model.clone(),
                system_prompt: None,
                consent_mode: ConsentMode::Collaborative,
                context_state: if staging { ContextState::Staging } else { ContextState::Live },
                context_type: "default".to_string(),
                created_at: kaijutsu_types::now_millis() as i64,
                created_by: caller.principal_id,
                forked_from: Some(source_id),
                fork_kind: Some(ForkKind::Compact),
                archived_at: None,
                workspace_id: source_ws,
                preset_id: None,
            };
            let default_ws =
                match db.get_or_create_default_workspace(caller.principal_id) {
                    Ok(id) => id,
                    Err(e) => return KjResult::Err(format!("kj fork --compact: {e}")),
                };
            // Context row + shell/env/binding copy land in one transaction, so
            // a failure can't strand a committed-but-misconfigured context.
            if let Err(e) = db.insert_forked_context(&row, default_ws, source_id) {
                return KjResult::Err(format!("kj fork --compact: {e}"));
            }

            if let Some(ref pwd) = pwd_override {
                let shell = ContextShellRow {
                    context_id: new_id,
                    cwd: Some(pwd.clone()),
                    updated_at: kaijutsu_types::now_millis() as i64,
                };
                if let Err(e) = db.upsert_context_shell(&shell) {
                    return KjResult::Err(format!("kj fork --compact: failed to set --pwd: {e}"));
                }
            }

            let edge = ContextEdgeRow {
                edge_id: uuid::Uuid::now_v7(),
                source_id,
                target_id: new_id,
                kind: EdgeKind::Structural,
                metadata: None,
                created_at: kaijutsu_types::now_millis() as i64,
            };
            if let Err(e) = db.insert_edge(&edge) {
                return KjResult::Err(format!(
                    "kj fork --compact: failed to insert structural edge: {e}"
                ));
            }
        }

        {
            let mut drift = self.drift_router().write();
            if let Err(e) =
                drift.register_fork(new_id, label.as_deref(), source_id, caller.principal_id)
            {
                return KjResult::Err(format!(
                    "kj fork --compact: parent context not in router: {e}"
                ));
            }
            if staging
                && let Err(e) = drift.set_state(new_id, ContextState::Staging)
            {
                return KjResult::Err(format!("kj fork --compact: failed to set staging state: {e}"));
            }
            if resolved.explicit {
                match (&resolved.provider, &resolved.model) {
                    (Some(p), Some(m)) => {
                        if let Err(e) = drift.configure_llm(new_id, p, m) {
                            return KjResult::Err(format!(
                                "kj fork --compact: failed to configure model: {e}"
                            ));
                        }
                    }
                    _ => {
                        return KjResult::Err(
                            "kj fork --compact: --model resolved without both provider and model"
                                .to_string(),
                        );
                    }
                }
            }
        }

        self.apply_fork_mcp_exclusions(new_id).await;

        let source_label = {
            let db = self.kernel_db().lock();
            db.get_context(source_id)
                .ok()
                .flatten()
                .and_then(|r| r.label)
        };
        let block_count = self
            .block_store()
            .block_snapshots(new_id)
            .map(|b| b.len())
            .unwrap_or(0);
        if let Err(e) = self.inject_fork_marker(
            new_id,
            source_id,
            ForkKind::Compact,
            block_count,
            source_label.as_deref(),
            staging,
        ) {
            tracing::warn!("kj fork --compact: failed to inject fork marker: {e}");
        }

        inherit_parent_context_type(self, new_id, source_id);
        if let Err(e) = self
            .run_rc_lifecycle(
                "fork",
                new_id,
                Some(source_id),
                Some(ForkKind::Compact),
                None,
                caller,
            )
            .await
        {
            tracing::warn!("rc fork lifecycle (compact): {e}");
        }

        // POSIX-style: drive the child's autonomous turn (if --prompt) after all
        // fork-time block injections + rc lifecycle, then honor stay-on-parent
        // default / --switch via fork_outcome.
        let switch = args.switch;
        self.request_child_turn(new_id, prompt.as_deref(), staging, caller);
        let short = new_id.short();
        let display = label.as_deref().unwrap_or(&short);
        let message = format!("compact-forked to '{}' ({})", display, new_id.short());
        self.fork_outcome(new_id, label.as_deref(), switch, message)
    }

    async fn fork_subtree(&self, args: &ForkArgs, caller: &KjCaller) -> KjResult {
        let template_ref = match args.as_template.clone() {
            Some(r) => r,
            None => {
                return KjResult::Err(
                    "kj fork --as: requires a template context reference".to_string(),
                );
            }
        };
        let name = match args.name.clone() {
            Some(n) => n,
            None => {
                return KjResult::Err(
                    "kj fork --as: requires --name for the new subtree".to_string(),
                );
            }
        };
        let staging = args.stage;
        let prompt = args.prompt.clone();

        let source_id = match caller.require_context() {
            Ok(id) => id,
            Err(e) => return e,
        };


        // Resolve template root
        let template_root_id = {
            let db = self.kernel_db().lock();
            match db.resolve_context(&template_ref) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj fork --as: {e}")),
            }
        };

        // Get the template subtree shape
        let template_nodes = {
            let db = self.kernel_db().lock();
            match db.subtree_snapshot(template_root_id) {
                Ok(nodes) => nodes,
                Err(e) => return KjResult::Err(format!("kj fork --as: {e}")),
            }
        };

        if template_nodes.is_empty() {
            return KjResult::Err("kj fork --as: template context not found".to_string());
        }

        // Validate all template node providers BEFORE any mutations
        {
            let registry = self.kernel().llm().read().await;
            for (row, _depth) in &template_nodes {
                if let Some(ref p) = row.provider
                    && registry.get(p).is_none()
                {
                    return KjResult::Err(format!(
                        "kj fork --as: template node '{}' references unknown provider '{}'",
                        row.label.as_deref().unwrap_or("(unnamed)"),
                        p,
                    ));
                }
            }
        }

        // Build ID mapping: old → new
        let mut id_map: HashMap<ContextId, ContextId> = HashMap::new();
        for (row, _depth) in &template_nodes {
            id_map.insert(row.context_id, ContextId::new());
        }

        let new_root_id = id_map[&template_root_id];

        // Create new contexts (BFS order — template_nodes is already ordered by depth)
        {
            let mut db = self.kernel_db().lock();

            for (row, _depth) in &template_nodes {
                let new_id = id_map[&row.context_id];
                let is_root = row.context_id == template_root_id;

                let new_label = if is_root {
                    Some(name.clone())
                } else {
                    row.label.as_ref().map(|l| format!("{name}/{l}"))
                };

                // Map forked_from to the new parent (if it's in the subtree),
                // otherwise point to caller's context
                let new_forked_from = row
                    .forked_from
                    .and_then(|fid| id_map.get(&fid).copied())
                    .or(caller.context_id);

                let new_row = ContextRow {
                    context_id: new_id,
                                        label: new_label,
                    provider: row.provider.clone(),
                    model: row.model.clone(),
                    system_prompt: row.system_prompt.clone(),
                    consent_mode: row.consent_mode,
                    context_state: if staging { ContextState::Staging } else { ContextState::Live },
                    context_type: "default".to_string(),
                    created_at: kaijutsu_types::now_millis() as i64,
                    created_by: caller.principal_id,
                    forked_from: new_forked_from,
                    fork_kind: Some(ForkKind::Subtree),
                    archived_at: None,
                    workspace_id: row.workspace_id,
                    preset_id: row.preset_id,
                };
                let default_ws =
                    match db.get_or_create_default_workspace(caller.principal_id) {
                        Ok(id) => id,
                        Err(e) => return KjResult::Err(format!("kj fork --as: {e}")),
                    };
                // Context row + shell/env/binding copy (from the template
                // context) land in one transaction, so a failure can't strand a
                // committed-but-misconfigured context.
                if let Err(e) = db.insert_forked_context(&new_row, default_ws, row.context_id) {
                    return KjResult::Err(format!("kj fork --as: failed to create context: {e}"));
                }

                // Create empty document for each new context
                if let Err(e) = self.block_store().create_document(
                    new_id,
                    crate::DocumentKind::Conversation,
                    None,
                ) {
                    return KjResult::Err(format!("kj fork --as: failed to create document: {e}"));
                }
            }

            // Insert structural edges mirroring the template
            for (row, _depth) in &template_nodes {
                let old_parent = row.context_id;
                let new_parent = id_map[&old_parent];

                // Get template's structural children
                let children = match db.structural_children(old_parent) {
                    Ok(c) => c,
                    Err(e) => {
                        return KjResult::Err(format!(
                            "kj fork --as: failed to read template edges: {e}"
                        ));
                    }
                };
                for child in children {
                    if let Some(&new_child) = id_map.get(&child.context_id) {
                        let edge = ContextEdgeRow {
                            edge_id: uuid::Uuid::now_v7(),
                            source_id: new_parent,
                            target_id: new_child,
                            kind: EdgeKind::Structural,
                            metadata: None,
                            created_at: kaijutsu_types::now_millis() as i64,
                        };
                        if let Err(e) = db.insert_edge(&edge) {
                            return KjResult::Err(format!(
                                "kj fork --as: failed to insert subtree edge: {e}"
                            ));
                        }
                    }
                }
            }

            // Edge from caller's context to the new root
            let root_edge = ContextEdgeRow {
                edge_id: uuid::Uuid::now_v7(),
                source_id,
                target_id: new_root_id,
                kind: EdgeKind::Structural,
                metadata: None,
                created_at: kaijutsu_types::now_millis() as i64,
            };
            if let Err(e) = db.insert_edge(&root_edge) {
                return KjResult::Err(format!("kj fork --as: failed to insert root edge: {e}"));
            }
        }

        // Register all new contexts in DriftRouter
        {
            let mut drift = self.drift_router().write();
            for (row, _depth) in &template_nodes {
                let new_id = id_map[&row.context_id];
                let is_root = row.context_id == template_root_id;
                let label = if is_root {
                    Some(name.as_str())
                } else {
                    row.label.as_deref()
                };
                let forked_from = row
                    .forked_from
                    .and_then(|fid| id_map.get(&fid).copied())
                    .or(caller.context_id);
                if let Some(parent) = forked_from {
                    if let Err(e) = drift.register_fork(new_id, label, parent, caller.principal_id)
                    {
                        return KjResult::Err(format!(
                            "kj fork --as: parent context not in router: {e}"
                        ));
                    }
                } else if let Err(e) = drift.register(new_id, label, None, caller.principal_id) {
                    return KjResult::Err(format!("kj fork --as: {e}"));
                }
                if staging
                    && let Err(e) = drift.set_state(new_id, ContextState::Staging)
                {
                    return KjResult::Err(format!("kj fork --as: failed to set staging state: {e}"));
                }
            }
        }

        self.apply_fork_mcp_exclusions(new_root_id).await;

        // If --prompt given, inject the fork note on the subtree root before the
        // fork marker — matching fork_full's placement so the autonomous turn's
        // anchor lands at the true tail.
        if let Some(note) = &prompt
            && let Err(e) = self.inject_fork_note(new_root_id, source_id, note)
        {
            return KjResult::Err(format!("kj fork --as: failed to inject fork note: {e}"));
        }

        if let Err(e) = self.inject_fork_marker(
            new_root_id,
            source_id,
            ForkKind::Subtree,
            template_nodes.len(),
            Some(&template_ref),
            staging,
        ) {
            tracing::warn!("kj fork --as: failed to inject fork marker: {e}");
        }

        inherit_parent_context_type(self, new_root_id, source_id);
        if let Err(e) = self
            .run_rc_lifecycle(
                "fork",
                new_root_id,
                Some(source_id),
                Some(ForkKind::Subtree),
                None,
                caller,
            )
            .await
        {
            tracing::warn!("rc fork lifecycle (subtree): {e}");
        }

        // POSIX-style: the prompt/turn targets the subtree root. Drive it after
        // all fork-time block injections + rc lifecycle, then honor the
        // stay-on-parent default / --switch via fork_outcome.
        let switch = args.switch;
        self.request_child_turn(new_root_id, prompt.as_deref(), staging, caller);
        let message = format!(
            "subtree-forked '{}' ({} contexts) from template '{}'",
            name,
            template_nodes.len(),
            template_ref
        );
        self.fork_outcome(new_root_id, Some(name.as_str()), switch, message)
    }

    /// Apply a preset's settings to a context (post-fork).
    async fn apply_preset(&self, context_id: ContextId, preset_label: &str) -> Result<(), String> {
        let preset = {
            let db = self.kernel_db().lock();
            db.get_preset_by_label(preset_label)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("preset '{}' not found", preset_label))?
        };

        // Update DB
        {
            let db = self.kernel_db().lock();
            if preset.provider.is_some() || preset.model.is_some() {
                db.update_model(
                    context_id,
                    preset.provider.as_deref(),
                    preset.model.as_deref(),
                )
                .map_err(|e| e.to_string())?;
            }
            db.update_settings(
                context_id,
                preset.system_prompt.as_deref(),
                preset.consent_mode,
            )
            .map_err(|e| e.to_string())?;
        }

        // Update DriftRouter
        {
            let mut drift = self.drift_router().write();
            if let (Some(p), Some(m)) = (&preset.provider, &preset.model) {
                let _ = drift.configure_llm(context_id, p, m);
            }
        }

        Ok(())
    }

    /// Build the terminal result for a completed fork.
    ///
    /// POSIX semantics: by default the caller stays on the parent and keeps
    /// running — the child id is returned in `data` so `for x in $(kj fork …)`
    /// and `kaish-last` can pick it up. `--switch` opts into moving the caller
    /// into the child (the old unconditional behaviour).
    fn fork_outcome(
        &self,
        new_id: ContextId,
        label: Option<&str>,
        switch: bool,
        message: String,
    ) -> KjResult {
        if switch {
            KjResult::Switch(new_id, message)
        } else {
            KjResult::Ok {
                message,
                content_type: ContentType::Plain,
                ephemeral: false,
                data: Some(serde_json::json!({
                    "context_id": new_id.to_hex(),
                    "label": label,
                })),
            }
        }
    }

    /// Publish a single `TurnFlow::Requested` and return how many subscribers
    /// received it (the turn-driver count). This is the one shared bridge from
    /// kernel-side commands (`kj fork --prompt`, `kj drive`) to the server's
    /// turn driver — the kernel can't call the server directly, so it clocks a
    /// turn by publishing on the FlowBus. A `delivered == 0` return means no
    /// driver is listening; callers decide how to surface that (fork writes an
    /// Error block; `kj drive` returns an error to the user directly).
    pub(crate) fn publish_turn_request(
        &self,
        context_id: ContextId,
        after_block_id: kaijutsu_types::BlockId,
        content: &str,
        principal_id: kaijutsu_types::PrincipalId,
    ) -> usize {
        self.kernel()
            .turn_flows()
            .publish(crate::flows::TurnFlow::Requested {
                context_id,
                after_block_id,
                content: content.to_string(),
                principal_id,
                model: None,
            })
    }

    /// Ask the server to drive one autonomous turn in the freshly forked child,
    /// so a `kj fork --prompt "…"` child starts acting immediately while the
    /// parent's fork call returns and keeps running (POSIX fork()).
    ///
    /// No-op when there's no seed (a bare fork is an inert snapshot) or when the
    /// child is staged (it's awaiting human curation). The seed already lives in
    /// the child's block log as the fork note, so this only publishes the
    /// request — it does not re-insert the seed. Must run after all fork-time
    /// block injections so `after_block_id` anchors at the true tail.
    fn request_child_turn(
        &self,
        new_id: ContextId,
        prompt: Option<&str>,
        staging: bool,
        caller: &KjCaller,
    ) {
        let Some(note) = prompt else { return };
        if staging {
            return;
        }
        let Some(after) = self.block_store().last_block_id(new_id) else {
            tracing::warn!(
                context_id = %new_id,
                "kj fork --prompt: child has no blocks to anchor an autonomous turn"
            );
            return;
        };
        let delivered =
            self.publish_turn_request(new_id, after, note, caller.principal_id);

        // Zero subscribers means no turn driver is listening — the autonomous
        // turn was requested but will never run. Don't silently no-op: warn and
        // surface a visible Error block in the child (same API rc lifecycle uses)
        // so the inert child is explained rather than mysterious.
        if delivered == 0 {
            tracing::warn!(
                context_id = %new_id,
                "kj fork --prompt: no turn driver subscribed; autonomous turn will not run"
            );
            let summary = "kj fork --prompt: no turn driver is active, so the requested \
                           autonomous turn will not run. This child was seeded but will \
                           stay idle until a turn is driven."
                .to_string();
            // Same BlockKind::Error / insert_block_as idiom rc lifecycle uses
            // (see kj/lifecycle.rs insert_rc_failure_block): a plain Error block
            // anchored at the tail, no structured ErrorPayload parent required.
            let after = self.block_store().last_block_id(new_id);
            if let Err(insert_err) = self.block_store().insert_block_as(
                new_id,
                None,
                after.as_ref(),
                kaijutsu_crdt::Role::System,
                kaijutsu_crdt::BlockKind::Error,
                summary,
                kaijutsu_crdt::Status::Error,
                kaijutsu_crdt::ContentType::Plain,
                Some(caller.principal_id),
            ) {
                tracing::warn!(
                    context_id = %new_id,
                    "kj fork --prompt: failed to insert no-driver error block: {insert_err}"
                );
            }
        }
    }

    fn inject_fork_note(
        &self,
        target_id: ContextId,
        source_id: ContextId,
        note: &str,
    ) -> Result<(), String> {
        use kaijutsu_crdt::DriftKind;

        let after = self.block_store().last_block_id(target_id);
        self.block_store()
            .insert_drift_block(
                target_id,
                None,
                after.as_ref(),
                note,
                source_id,
                None,
                DriftKind::Push,
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Insert an ephemeral fork marker block at the end of the forked document.
    ///
    /// The marker summarizes the fork operation (source, kind, block count) and is
    /// excluded from LLM hydration so it doesn't waste model context.
    fn inject_fork_marker(
        &self,
        target_id: ContextId,
        source_id: ContextId,
        fork_kind: ForkKind,
        block_count: usize,
        source_label: Option<&str>,
        staging: bool,
    ) -> Result<(), String> {
        use kaijutsu_crdt::DriftKind;

        let source_short = source_id.short();
        let source_display = source_label.unwrap_or(&source_short);
        let content = format!(
            "forked from '{}' ({}) — {} copy, {} blocks",
            source_display,
            source_short,
            fork_kind.as_str(),
            block_count,
        );

        let after = self.block_store().last_block_id(target_id);
        let block_id = self
            .block_store()
            .insert_drift_block(
                target_id,
                None,
                after.as_ref(),
                &content,
                source_id,
                None,
                DriftKind::Fork,
            )
            .map_err(|e| e.to_string())?;

        self.block_store()
            .set_ephemeral(target_id, &block_id, true)
            .map_err(|e| e.to_string())?;

        // In staging mode, fork marker starts excluded (user opts in)
        if staging {
            self.block_store()
                .set_excluded(target_id, &block_id, true)
                .map_err(|e| e.to_string())?;
        }

        Ok(())
    }

}

/// Copy the parent's `context_type` onto the freshly-forked child so the
/// child's fork-side rc lifecycle dispatches against the parent's type.
/// All four fork variants commit their child with `context_type='default'`
/// at insert time, so this is a post-commit fixup.
///
/// On any error (parent missing, update fails) we leave the child as
/// 'default' and log — failure here would corrupt fewer guarantees than
/// aborting a successful fork.
fn inherit_parent_context_type(
    dispatcher: &KjDispatcher,
    child_id: ContextId,
    parent_id: ContextId,
) {
    let parent_type = {
        let db = dispatcher.kernel_db().lock();
        match db.get_context(parent_id) {
            Ok(Some(row)) => row.context_type,
            Ok(None) => {
                tracing::warn!(
                    "rc fork: parent context {} not found; child {} stays 'default'",
                    parent_id.short(),
                    child_id.short()
                );
                return;
            }
            Err(e) => {
                tracing::warn!("rc fork: cannot read parent context_type: {e}");
                return;
            }
        }
    };
    if parent_type == "default" {
        return; // already the default
    }
    let db = dispatcher.kernel_db().lock();
    if let Err(e) = db.update_context_type(child_id, &parent_type) {
        tracing::warn!(
            "rc fork: failed to set context_type='{}' on child {}: {e}",
            parent_type,
            child_id.short()
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::kj::test_helpers::*;
    use kaijutsu_types::PrincipalId;

    fn s(v: &str) -> String {
        v.to_string()
    }

    #[tokio::test]
    async fn fork_basic() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("source"), None, principal);

        // Create a document for the source context
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(source);
        let result = d.dispatch(&[s("fork"), s("--name"), s("branch")], &c).await;
        assert!(result.is_ok(), "fork failed: {}", result.message());
        assert!(
            result.message().contains("branch"),
            "msg: {}",
            result.message()
        );

        // Verify new context exists in DB
        let db = d.kernel_db().lock();
        let contexts = db.list_active_contexts().unwrap();
        assert!(
            contexts
                .iter()
                .any(|r| r.label.as_deref() == Some("branch"))
        );
    }

    /// Insert a Text block into `ctx` and return its id — for exercising
    /// `--exclude` against a known block.
    fn insert_text(
        d: &crate::KjDispatcher,
        ctx: kaijutsu_types::ContextId,
        principal: PrincipalId,
        body: &str,
    ) -> kaijutsu_crdt::BlockId {
        d.block_store()
            .insert_block_as(
                ctx,
                None,
                None,
                kaijutsu_crdt::Role::User,
                kaijutsu_crdt::BlockKind::Text,
                body.to_string(),
                kaijutsu_crdt::Status::Done,
                kaijutsu_crdt::ContentType::Plain,
                Some(principal),
            )
            .unwrap()
    }

    /// Full fork's power path: `--exclude <block>` drops that block from the
    /// child (the orchestrator-repair case — "fork X without the huge block that
    /// blew it up") while copying everything else. Today full fork copies
    /// everything; this wires the existing ForkBlockFilter onto it.
    #[tokio::test]
    async fn fork_exclude_drops_named_block_keeps_rest() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("source"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        insert_text(&d, source, principal, "keep1");
        let drop_id = insert_text(&d, source, principal, "DROPME");
        insert_text(&d, source, principal, "keep2");

        let c = caller_with_context(source);
        let result = d
            .dispatch(
                &[s("fork"), s("--name"), s("repaired"), s("--exclude"), s(&drop_id.to_key())],
                &c,
            )
            .await;
        assert!(result.is_ok(), "fork --exclude failed: {}", result.message());

        let child = d
            .kernel_db()
            .lock()
            .find_context_by_label("repaired")
            .unwrap()
            .unwrap()
            .context_id;
        let contents: Vec<String> = d
            .block_store()
            .block_snapshots(child)
            .unwrap()
            .iter()
            .map(|b| b.content.clone())
            .collect();
        assert!(contents.iter().any(|c| c.contains("keep1")), "kept blocks: {contents:?}");
        assert!(contents.iter().any(|c| c.contains("keep2")), "kept blocks: {contents:?}");
        assert!(
            !contents.iter().any(|c| c.contains("DROPME")),
            "the excluded block must not be copied into the child: {contents:?}"
        );
    }

    /// Fail-loud (consistent with `kj context hydrate --mark`): a `--exclude`
    /// block id that doesn't exist in the source is a typo, not a silent no-op
    /// (which would leave the offending block in the repaired child).
    #[tokio::test]
    async fn fork_exclude_rejects_block_not_in_source() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("source"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        insert_text(&d, source, principal, "real");
        let phantom = kaijutsu_crdt::BlockId::new(source, PrincipalId::new(), 9999).to_key();

        let c = caller_with_context(source);
        let result = d
            .dispatch(&[s("fork"), s("--exclude"), s(&phantom)], &c)
            .await;
        assert!(!result.is_ok(), "a --exclude block not in the source must error");
        assert!(
            result.message().contains("not in") || result.message().contains("not found"),
            "msg: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn fork_no_name() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("src"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(source);
        let result = d.dispatch(&[s("fork")], &c).await;
        assert!(result.is_ok(), "fork failed: {}", result.message());
        assert!(result.message().contains("forked to"));
    }

    #[tokio::test]
    async fn fork_with_prompt() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("src"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(source);
        let result = d
            .dispatch(
                &[
                    s("fork"),
                    s("--name"),
                    s("noted"),
                    s("--prompt"),
                    s("explore auth bug"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "fork failed: {}", result.message());
    }

    #[tokio::test]
    async fn fork_help() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d.dispatch(&[s("fork"), s("--help")], &c).await;
        assert!(result.is_ok());
        assert!(
            result.message().contains("Usage") && result.message().contains("--prompt"),
            "clap help should list usage + flags: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn fork_compact_empty_source_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("empty-src"), None, principal);

        // Create empty document
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(source);
        let result = d
            .dispatch(
                &[s("fork"), s("--compact"), s("--name"), s("compacted")],
                &c,
            )
            .await;
        assert!(
            !result.is_ok(),
            "should fail on empty source: {}",
            result.message()
        );
        assert!(
            result.message().contains("no blocks"),
            "msg: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn fork_inherits_config() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("src"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        // Set shell config and env on source
        {
            let db = d.kernel_db().lock();
            db.upsert_context_shell(&crate::kernel_db::ContextShellRow {
                context_id: source,
                cwd: Some("/home/user/project".into()),
                updated_at: kaijutsu_types::now_millis() as i64,
            })
            .unwrap();
            db.set_context_env(source, "RUST_LOG", "debug").unwrap();
            db.set_context_env(source, "EDITOR", "vim").unwrap();
        }

        let c = caller_with_context(source);
        let result = d.dispatch(&[s("fork"), s("--name"), s("child")], &c).await;
        assert!(result.is_ok(), "fork failed: {}", result.message());

        // Find the new context and verify config was copied
        let db = d.kernel_db().lock();
        let child = db
            .find_context_by_label("child")
            .unwrap()
            .unwrap();
        let shell = db.get_context_shell(child.context_id).unwrap().unwrap();
        assert_eq!(shell.cwd, Some("/home/user/project".into()));
        let env = db.get_context_env(child.context_id).unwrap();
        assert_eq!(env.len(), 2);
    }

    #[tokio::test]
    async fn fork_pwd_override() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("src"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        // Set cwd on source
        {
            let db = d.kernel_db().lock();
            db.upsert_context_shell(&crate::kernel_db::ContextShellRow {
                context_id: source,
                cwd: Some("/home/user/project".into()),
                updated_at: kaijutsu_types::now_millis() as i64,
            })
            .unwrap();
        }

        let c = caller_with_context(source);
        let result = d
            .dispatch(
                &[
                    s("fork"),
                    s("--name"),
                    s("research"),
                    s("--pwd"),
                    s("/home/user/src/bevy_vello"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "fork failed: {}", result.message());

        let db = d.kernel_db().lock();
        let child = db
            .find_context_by_label("research")
            .unwrap()
            .unwrap();
        let shell = db.get_context_shell(child.context_id).unwrap().unwrap();
        assert_eq!(shell.cwd, Some("/home/user/src/bevy_vello".into()));
    }

    /// Register a mock LLM provider on the kernel so --model validation passes.
    async fn register_mock_provider(d: &super::super::KjDispatcher) {
        use crate::llm::{MockClient, Provider};
        use std::sync::Arc;
        let mock = Arc::new(Provider::Mock(MockClient::new("mock response")));
        let mut registry = d.kernel().llm().write().await;
        registry.register("mock", mock);
    }

    /// Configure provider+model on a context in DriftRouter.
    async fn configure_context_model(
        d: &super::super::KjDispatcher,
        id: kaijutsu_types::ContextId,
        provider: &str,
        model: &str,
    ) {
        let mut drift = d.drift_router().write();
        let _ = drift.configure_llm(id, provider, model);
    }

    #[tokio::test]
    async fn fork_inherits_parent_model_in_db() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        // Set parent's model in DriftRouter
        register_mock_provider(&d).await;
        configure_context_model(&d, source, "mock", "mock-model").await;

        let c = caller_with_context(source);
        let result = d.dispatch(&[s("fork"), s("--name"), s("child")], &c).await;
        assert!(result.is_ok(), "fork failed: {}", result.message());

        // Verify child inherited provider+model in DB
        let db = d.kernel_db().lock();
        let child = db
            .find_context_by_label("child")
            .unwrap()
            .unwrap();
        assert_eq!(
            child.provider.as_deref(),
            Some("mock"),
            "child should inherit parent provider"
        );
        assert_eq!(
            child.model.as_deref(),
            Some("mock-model"),
            "child should inherit parent model"
        );
    }

    #[tokio::test]
    async fn fork_model_flag_overrides_parent() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        register_mock_provider(&d).await;
        configure_context_model(&d, source, "mock", "mock-model").await;

        let c = caller_with_context(source);
        let result = d
            .dispatch(
                &[
                    s("fork"),
                    s("--name"),
                    s("override"),
                    s("--model"),
                    s("mock/custom-model"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "fork failed: {}", result.message());

        // Verify child has overridden model in DB
        let db = d.kernel_db().lock();
        let child = db
            .find_context_by_label("override")
            .unwrap()
            .unwrap();
        assert_eq!(child.provider.as_deref(), Some("mock"));
        assert_eq!(child.model.as_deref(), Some("custom-model"));

        // And in DriftRouter
        drop(db);
        let drift = d.drift_router().read();
        let handle = drift
            .get(child.context_id)
            .expect("child should be in DriftRouter");
        assert_eq!(handle.provider.as_deref(), Some("mock"));
        assert_eq!(handle.model.as_deref(), Some("custom-model"));
    }

    #[tokio::test]
    async fn fork_invalid_provider_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(source);
        let result = d
            .dispatch(
                &[
                    s("fork"),
                    s("--name"),
                    s("bad"),
                    s("--model"),
                    s("nonexistent/foo"),
                ],
                &c,
            )
            .await;
        assert!(!result.is_ok(), "should have failed: {}", result.message());
        assert!(
            result.message().contains("unknown provider"),
            "expected 'unknown provider' error, got: {}",
            result.message()
        );

        // Verify no context was created (mutation didn't happen)
        let db = d.kernel_db().lock();
        let found = db.find_context_by_label("bad").unwrap();
        assert!(
            found.is_none(),
            "no context should have been created for invalid provider"
        );
    }

    /// Bare model name (no provider/ prefix) should resolve provider from registry.
    /// This is the bug that caused `kj fork --model claude-sonnet-4-6` to silently
    /// keep the parent's model.
    #[tokio::test]
    async fn fork_bare_model_resolves_provider() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        register_mock_provider(&d).await;
        // Set mock as default so bare model names resolve to it
        {
            let mut registry = d.kernel().llm().write().await;
            registry.set_default("mock");
        }
        configure_context_model(&d, source, "mock", "old-model").await;

        let c = caller_with_context(source);
        // Bare model name — no "mock/" prefix
        let result = d
            .dispatch(
                &[
                    s("fork"),
                    s("--name"),
                    s("bare"),
                    s("--model"),
                    s("new-model"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "fork failed: {}", result.message());

        // Verify provider was resolved from registry default
        let db = d.kernel_db().lock();
        let child = db
            .find_context_by_label("bare")
            .unwrap()
            .unwrap();
        assert_eq!(
            child.provider.as_deref(),
            Some("mock"),
            "provider should be resolved from registry"
        );
        assert_eq!(child.model.as_deref(), Some("new-model"));

        // And in DriftRouter
        drop(db);
        let drift = d.drift_router().read();
        let handle = drift
            .get(child.context_id)
            .expect("child should be in DriftRouter");
        assert_eq!(
            handle.provider.as_deref(),
            Some("mock"),
            "DriftRouter provider should match"
        );
        assert_eq!(
            handle.model.as_deref(),
            Some("new-model"),
            "DriftRouter model should match"
        );
    }

    /// Default fork is POSIX-style: the caller stays on the parent. The child
    /// id is surfaced via `data`, not by switching.
    #[tokio::test]
    async fn fork_default_stays_on_parent() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(source);
        let result = d.dispatch(&[s("fork"), s("--name"), s("child")], &c).await;
        match &result {
            super::super::KjResult::Ok { data, message, .. } => {
                assert!(message.contains("child"), "msg: {message}");
                let ctx = data
                    .as_ref()
                    .and_then(|d| d.get("context_id"))
                    .and_then(|v| v.as_str())
                    .expect("fork should surface child context_id in data");
                assert_ne!(ctx, source.to_hex(), "child must be a new context");
            }
            other => panic!("expected Ok (stay on parent), got: {}", other.message()),
        }
    }

    /// `--switch` opts back into the old behaviour: move the caller to the child.
    #[tokio::test]
    async fn fork_switch_flag_moves_to_child() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(source);
        let result = d
            .dispatch(&[s("fork"), s("--name"), s("child"), s("--switch")], &c)
            .await;
        match &result {
            super::super::KjResult::Switch(id, msg) => {
                assert_ne!(*id, source, "should switch to new context");
                assert!(msg.contains("child"), "msg: {msg}");
            }
            other => panic!("expected Switch with --switch, got: {}", other.message()),
        }
    }

    #[tokio::test]
    async fn fork_with_prompt_requests_turn() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        let c = caller_with_context(source);
        let mut sub = d.kernel().turn_flows().subscribe("turn.requested");

        let result = d
            .dispatch(
                &[
                    s("fork"),
                    s("--name"),
                    s("child"),
                    s("--prompt"),
                    s("go explore"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "fork failed: {}", result.message());

        let msg = sub
            .try_recv()
            .expect("fork --prompt should publish a turn request");
        match msg.payload {
            crate::flows::TurnFlow::Requested {
                context_id,
                principal_id,
                content,
                ..
            } => {
                assert_ne!(context_id, source, "the turn targets the child, not parent");
                assert_eq!(principal_id, c.principal_id);
                assert_eq!(content, "go explore");
            }
            other => panic!("expected Requested, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fork_without_prompt_requests_no_turn() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        let c = caller_with_context(source);
        let mut sub = d.kernel().turn_flows().subscribe("turn.requested");

        let result = d.dispatch(&[s("fork"), s("--name"), s("child")], &c).await;
        assert!(result.is_ok(), "fork failed: {}", result.message());
        assert!(
            sub.try_recv().is_none(),
            "a bare fork is an inert snapshot — it must not request a turn"
        );
    }

    #[tokio::test]
    async fn fork_staged_with_prompt_requests_no_turn() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        let c = caller_with_context(source);
        let mut sub = d.kernel().turn_flows().subscribe("turn.requested");

        let result = d
            .dispatch(
                &[
                    s("fork"),
                    s("--name"),
                    s("child"),
                    s("--prompt"),
                    s("go explore"),
                    s("--stage"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "fork failed: {}", result.message());
        assert!(
            sub.try_recv().is_none(),
            "a staged child is awaiting curation — no autonomous turn yet"
        );
    }

    #[tokio::test]
    async fn fork_inherits_workspace() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("src"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        // Bind a workspace to source
        let ws_id = kaijutsu_types::WorkspaceId::new();
        {
            let db = d.kernel_db().lock();
            db.insert_workspace(&crate::kernel_db::WorkspaceRow {
                workspace_id: ws_id,
                                label: "test-ws".into(),
                description: None,
                created_at: kaijutsu_types::now_millis() as i64,
                created_by: principal,
                archived_at: None,
            })
            .unwrap();
            db.update_workspace(source, Some(ws_id)).unwrap();
        }

        let c = caller_with_context(source);
        let result = d.dispatch(&[s("fork"), s("--name"), s("child")], &c).await;
        assert!(result.is_ok(), "fork failed: {}", result.message());

        let db = d.kernel_db().lock();
        let child = db
            .find_context_by_label("child")
            .unwrap()
            .unwrap();
        assert_eq!(child.workspace_id, Some(ws_id));
    }

    // ====================================================================
    // POSIX parity across fork kinds: --prompt drives a turn, --switch moves
    // the caller, bare fork drives nothing. Mirrors the fork_full reference
    // tests above (fork_with_prompt_requests_turn / _without_ / _switch_).
    // ====================================================================

    #[tokio::test]
    async fn fork_shallow_with_prompt_requests_turn() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        let c = caller_with_context(source);
        let mut sub = d.kernel().turn_flows().subscribe("turn.requested");

        let result = d
            .dispatch(
                &[s("fork"), s("--shallow"), s("--prompt"), s("explore shallow")],
                &c,
            )
            .await;
        assert!(result.is_ok(), "fork failed: {}", result.message());

        let msg = sub
            .try_recv()
            .expect("fork --shallow --prompt should publish a turn request");
        match msg.payload {
            crate::flows::TurnFlow::Requested {
                context_id,
                principal_id,
                content,
                ..
            } => {
                assert_ne!(context_id, source, "the turn targets the child, not parent");
                assert_eq!(principal_id, c.principal_id);
                assert_eq!(content, "explore shallow");
            }
            other => panic!("expected Requested, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fork_shallow_without_prompt_requests_no_turn() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        let c = caller_with_context(source);
        let mut sub = d.kernel().turn_flows().subscribe("turn.requested");

        let result = d.dispatch(&[s("fork"), s("--shallow")], &c).await;
        assert!(result.is_ok(), "fork failed: {}", result.message());
        assert!(
            sub.try_recv().is_none(),
            "a bare shallow fork must not request a turn"
        );
    }

    #[tokio::test]
    async fn fork_shallow_switch_flag_moves_to_child() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        let c = caller_with_context(source);

        let result = d
            .dispatch(&[s("fork"), s("--shallow"), s("--switch")], &c)
            .await;
        match &result {
            super::super::KjResult::Switch(id, _msg) => {
                assert_ne!(*id, source, "--switch should move to a new child context");
            }
            other => panic!("expected Switch with --switch, got: {}", other.message()),
        }
    }

    /// Register a mock LLM provider (set as default), configure it on `source`,
    /// and seed `source` with a block so compact's distillation step (an LLM
    /// call over non-empty content) can run in tests.
    async fn setup_compact_source(
        d: &super::super::KjDispatcher,
        source: kaijutsu_types::ContextId,
        principal: PrincipalId,
    ) {
        use crate::llm::{MockClient, Provider};
        use std::sync::Arc;
        {
            let mut registry = d.kernel().llm().write().await;
            registry.register("mock", Arc::new(Provider::Mock(MockClient::new("summary"))));
            registry.set_default("mock");
        }
        {
            let mut drift = d.drift_router().write();
            let _ = drift.configure_llm(source, "mock", "mock-model");
        }
        // compact summarizes the source's included blocks; without content it
        // errors (see fork_compact_empty_source_errors).
        d.block_store()
            .insert_block_as(
                source,
                None,
                None,
                kaijutsu_crdt::Role::User,
                kaijutsu_crdt::BlockKind::Text,
                "hello world".to_string(),
                kaijutsu_crdt::Status::Done,
                kaijutsu_crdt::ContentType::Plain,
                Some(principal),
            )
            .unwrap();
    }

    #[tokio::test]
    async fn fork_compact_with_prompt_requests_turn() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        setup_compact_source(&d, source, principal).await;
        let c = caller_with_context(source);
        let mut sub = d.kernel().turn_flows().subscribe("turn.requested");

        let result = d
            .dispatch(
                &[s("fork"), s("--compact"), s("--prompt"), s("explore compact")],
                &c,
            )
            .await;
        assert!(result.is_ok(), "fork failed: {}", result.message());

        let msg = sub
            .try_recv()
            .expect("fork --compact --prompt should publish a turn request");
        match msg.payload {
            crate::flows::TurnFlow::Requested {
                context_id,
                principal_id,
                content,
                ..
            } => {
                assert_ne!(context_id, source, "the turn targets the child, not parent");
                assert_eq!(principal_id, c.principal_id);
                assert_eq!(content, "explore compact");
            }
            other => panic!("expected Requested, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fork_compact_without_prompt_requests_no_turn() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        setup_compact_source(&d, source, principal).await;
        let c = caller_with_context(source);
        let mut sub = d.kernel().turn_flows().subscribe("turn.requested");

        let result = d.dispatch(&[s("fork"), s("--compact")], &c).await;
        assert!(result.is_ok(), "fork failed: {}", result.message());
        assert!(
            sub.try_recv().is_none(),
            "a bare compact fork must not request a turn"
        );
    }

    #[tokio::test]
    async fn fork_compact_switch_flag_moves_to_child() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        setup_compact_source(&d, source, principal).await;
        let c = caller_with_context(source);

        let result = d
            .dispatch(&[s("fork"), s("--compact"), s("--switch")], &c)
            .await;
        match &result {
            super::super::KjResult::Switch(id, _msg) => {
                assert_ne!(*id, source, "--switch should move to a new child context");
            }
            other => panic!("expected Switch with --switch, got: {}", other.message()),
        }
    }

    #[tokio::test]
    async fn fork_subtree_with_prompt_requests_turn() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        let c = caller_with_context(source);
        let mut sub = d.kernel().turn_flows().subscribe("turn.requested");

        let result = d
            .dispatch(
                &[
                    s("fork"),
                    s("--as"),
                    s("parent"),
                    s("--name"),
                    s("tmpl"),
                    s("--prompt"),
                    s("explore subtree"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "fork failed: {}", result.message());

        // The turn targets the subtree root.
        let msg = sub
            .try_recv()
            .expect("fork --as --prompt should publish a turn request");
        match msg.payload {
            crate::flows::TurnFlow::Requested {
                context_id,
                principal_id,
                content,
                ..
            } => {
                assert_ne!(context_id, source, "the turn targets the new root, not parent");
                assert_eq!(principal_id, c.principal_id);
                assert_eq!(content, "explore subtree");
            }
            other => panic!("expected Requested, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fork_subtree_without_prompt_requests_no_turn() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        let c = caller_with_context(source);
        let mut sub = d.kernel().turn_flows().subscribe("turn.requested");

        let result = d
            .dispatch(
                &[s("fork"), s("--as"), s("parent"), s("--name"), s("tmpl")],
                &c,
            )
            .await;
        assert!(result.is_ok(), "fork failed: {}", result.message());
        assert!(
            sub.try_recv().is_none(),
            "a bare subtree fork must not request a turn"
        );
    }
}
