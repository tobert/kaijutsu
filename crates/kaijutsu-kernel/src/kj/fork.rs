//! Fork subcommand: deep-copy the current context's document.

use std::collections::HashMap;

use kaijutsu_types::{ConsentMode, ContextId, EdgeKind, ForkKind};

use crate::kernel_db::{ContextEdgeRow, ContextRow, ContextShellRow};

use super::parse::{extract_named_arg, has_flag, parse_model_spec};
use super::{KjCaller, KjDispatcher, KjResult};

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
        argv: &[String],
        source_id: ContextId,
    ) -> Result<ResolvedModel, String> {
        let model_spec = extract_named_arg(argv, &["--model", "-m"]);

        // Read parent's provider+model from DriftRouter (before any mutations)
        let (parent_provider, parent_model) = {
            let router = self.drift_router().read().await;
            router
                .get(source_id)
                .map(|h| (h.provider.clone(), h.model.clone()))
                .unwrap_or((None, None))
        };

        match model_spec {
            Some(spec) => {
                let (mut provider, model) = parse_model_spec(&spec);
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
        if has_flag(argv, &["help", "--help", "-h"]) {
            return KjResult::ok_ephemeral(self.fork_help(), "text/markdown");
        }

        if has_flag(argv, &["--shallow"]) {
            return self.fork_shallow(argv, caller).await;
        }
        if has_flag(argv, &["--compact"]) {
            return self.fork_compact(argv, caller).await;
        }
        if has_flag(argv, &["--as"]) {
            return self.fork_subtree(argv, caller).await;
        }

        self.fork_full(argv, caller).await
    }

    fn fork_help(&self) -> String {
        include_str!("../../docs/help/kj-fork.md").to_string()
    }

    async fn fork_full(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        // Parse --name / -n
        let label = extract_named_arg(argv, &["--name", "-n"]);

        // Parse --prompt
        let prompt = extract_named_arg(argv, &["--prompt"]);

        // Parse --preset
        let preset_label = extract_named_arg(argv, &["--preset"]);

        // Parse --pwd (override cwd on forked context)
        let pwd_override = extract_named_arg(argv, &["--pwd"]);

        let source_id = caller.context_id;
        let new_id = ContextId::new();
        let kernel_id = self.kernel_id();

        // Validate --model BEFORE any mutations
        let resolved = match self.resolve_fork_model(argv, source_id).await {
            Ok(r) => r,
            Err(e) => return KjResult::Err(format!("kj fork: {e}")),
        };

        // Deep-copy the BlockStore document
        if let Err(e) = self.block_store().fork_document(source_id, new_id) {
            return KjResult::Err(format!("kj fork: failed to copy document: {e}"));
        }

        // Write-through: KernelDb then DriftRouter
        {
            let db = self.kernel_db().lock().unwrap();

            // Inherit workspace from source
            let source_ws = db.get_context(source_id).ok()
                .flatten()
                .and_then(|r| r.workspace_id);

            let row = ContextRow {
                context_id: new_id,
                kernel_id,
                label: label.clone(),
                provider: resolved.provider.clone(),
                model: resolved.model.clone(),
                system_prompt: None,
                tool_filter: None,
                consent_mode: ConsentMode::Collaborative,
                created_at: kaijutsu_types::now_millis() as i64,
                created_by: caller.principal_id,
                forked_from: Some(source_id),
                fork_kind: Some(ForkKind::Full),
                archived_at: None,
                workspace_id: source_ws,
                preset_id: None,
            };
            let default_ws = match db.get_or_create_default_workspace(kernel_id, caller.principal_id) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj fork: {e}")),
            };
            if let Err(e) = db.insert_context_with_document(&row, default_ws) {
                return KjResult::Err(format!("kj fork: {e}"));
            }

            // Copy shell config + env vars from source
            if let Err(e) = db.fork_context_config(source_id, new_id) {
                return KjResult::Err(format!("kj fork: failed to copy context config: {e}"));
            }

            // Apply --pwd override
            if let Some(ref pwd) = pwd_override {
                let shell = ContextShellRow {
                    context_id: new_id,
                    cwd: Some(pwd.clone()),
                    init_script: db.get_context_shell(source_id).ok()
                        .flatten()
                        .and_then(|s| s.init_script),
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
            let mut drift = self.drift_router().write().await;
            if let Err(e) = drift.register_fork(new_id, label.as_deref(), source_id, caller.principal_id) {
                return KjResult::Err(format!("kj fork: parent context not in router: {e}"));
            }
            // If --model was explicit, override the inherited model
            if resolved.explicit {
                match (&resolved.provider, &resolved.model) {
                    (Some(p), Some(m)) => {
                        if let Err(e) = drift.configure_llm(new_id, p, m) {
                            return KjResult::Err(format!("kj fork: failed to configure model: {e}"));
                        }
                    }
                    _ => {
                        return KjResult::Err(
                            "kj fork: --model resolved without both provider and model".to_string()
                        );
                    }
                }
            }
        }

        // Apply preset if requested
        if let Some(ref preset) = preset_label {
            if let Err(e) = self.apply_preset(new_id, preset).await {
                return KjResult::Err(format!("kj fork: failed to apply preset '{preset}': {e}"));
            }
        }

        // If --prompt given, inject a Drift block
        if let Some(note) = prompt {
            if let Err(e) = self.inject_fork_note(new_id, caller, &note) {
                return KjResult::Err(format!("kj fork: failed to inject fork note: {e}"));
            }
        }

        let short = new_id.short();
        let display = label
            .as_deref()
            .unwrap_or(&short);
        KjResult::Switch(new_id, format!("forked to '{}' ({})", display, new_id.short()))
    }

    async fn fork_shallow(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let label = extract_named_arg(argv, &["--name", "-n"]);
        let prompt = extract_named_arg(argv, &["--prompt"]);
        let preset_label = extract_named_arg(argv, &["--preset"]);
        let pwd_override = extract_named_arg(argv, &["--pwd"]);
        let depth: usize = extract_named_arg(argv, &["--depth"])
            .and_then(|d| d.parse().ok())
            .unwrap_or(50);

        let source_id = caller.context_id;
        let new_id = ContextId::new();
        let kernel_id = self.kernel_id();

        // Validate --model BEFORE any mutations
        let resolved = match self.resolve_fork_model(argv, source_id).await {
            Ok(r) => r,
            Err(e) => return KjResult::Err(format!("kj fork --shallow: {e}")),
        };

        // Get current version for the source document
        let version = self.block_store().get(source_id)
            .map(|e| e.version())
            .unwrap_or(0);

        // Shallow fork with block filter
        let filter = kaijutsu_crdt::ForkBlockFilter {
            max_blocks: Some(depth),
            exclude_compacted: true,
            ..Default::default()
        };
        if let Err(e) = self.block_store().fork_document_filtered(source_id, new_id, version, &filter) {
            return KjResult::Err(format!("kj fork --shallow: {e}"));
        }

        // Write-through
        {
            let db = self.kernel_db().lock().unwrap();

            let source_ws = db.get_context(source_id).ok()
                .flatten()
                .and_then(|r| r.workspace_id);

            let row = ContextRow {
                context_id: new_id,
                kernel_id,
                label: label.clone(),
                provider: resolved.provider.clone(),
                model: resolved.model.clone(),
                system_prompt: None,
                tool_filter: None,
                consent_mode: ConsentMode::Collaborative,
                created_at: kaijutsu_types::now_millis() as i64,
                created_by: caller.principal_id,
                forked_from: Some(source_id),
                fork_kind: Some(ForkKind::Shallow),
                archived_at: None,
                workspace_id: source_ws,
                preset_id: None,
            };
            let default_ws = match db.get_or_create_default_workspace(kernel_id, caller.principal_id) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj fork --shallow: {e}")),
            };
            if let Err(e) = db.insert_context_with_document(&row, default_ws) {
                return KjResult::Err(format!("kj fork --shallow: {e}"));
            }

            if let Err(e) = db.fork_context_config(source_id, new_id) {
                return KjResult::Err(format!("kj fork --shallow: failed to copy context config: {e}"));
            }

            if let Some(ref pwd) = pwd_override {
                let shell = ContextShellRow {
                    context_id: new_id,
                    cwd: Some(pwd.clone()),
                    init_script: db.get_context_shell(source_id).ok()
                        .flatten()
                        .and_then(|s| s.init_script),
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
                return KjResult::Err(format!("kj fork --shallow: failed to insert structural edge: {e}"));
            }
        }

        {
            let mut drift = self.drift_router().write().await;
            if let Err(e) = drift.register_fork(new_id, label.as_deref(), source_id, caller.principal_id) {
                return KjResult::Err(format!("kj fork --shallow: parent context not in router: {e}"));
            }
            if resolved.explicit {
                match (&resolved.provider, &resolved.model) {
                    (Some(p), Some(m)) => {
                        if let Err(e) = drift.configure_llm(new_id, p, m) {
                            return KjResult::Err(format!("kj fork --shallow: failed to configure model: {e}"));
                        }
                    }
                    _ => {
                        return KjResult::Err(
                            "kj fork --shallow: --model resolved without both provider and model".to_string()
                        );
                    }
                }
            }
        }

        if let Some(ref preset) = preset_label {
            if let Err(e) = self.apply_preset(new_id, preset).await {
                return KjResult::Err(format!("kj fork --shallow: failed to apply preset '{preset}': {e}"));
            }
        }

        if let Some(note) = prompt {
            if let Err(e) = self.inject_fork_note(new_id, caller, &note) {
                return KjResult::Err(format!("kj fork --shallow: failed to inject fork note: {e}"));
            }
        }

        let short = new_id.short();
        let display = label.as_deref().unwrap_or(&short);
        KjResult::Switch(new_id, format!("shallow-forked to '{}' ({}, depth {})", display, new_id.short(), depth))
    }

    async fn fork_compact(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let label = extract_named_arg(argv, &["--name", "-n"]);
        let prompt = extract_named_arg(argv, &["--prompt"]);
        let pwd_override = extract_named_arg(argv, &["--pwd"]);

        let source_id = caller.context_id;
        let new_id = ContextId::new();
        let kernel_id = self.kernel_id();

        // Validate --model BEFORE any mutations
        let resolved = match self.resolve_fork_model(argv, source_id).await {
            Ok(r) => r,
            Err(e) => return KjResult::Err(format!("kj fork --compact: {e}")),
        };

        // Summarize source context via LLM
        let summary = match self.summarize(source_id, None).await {
            Ok(s) => s,
            Err(e) => return KjResult::Err(format!("kj fork --compact: {e}")),
        };

        // Create empty document for the new context
        if let Err(e) = self.block_store().create_document(
            new_id,
            crate::DocumentKind::Conversation,
            None,
        ) {
            return KjResult::Err(format!("kj fork --compact: failed to create document: {e}"));
        }

        // Seed with distilled summary as a Drift block
        {
            let source_model = {
                let router = self.drift_router().read().await;
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
        if let Some(note) = prompt {
            if let Err(e) = self.inject_fork_note(new_id, caller, &note) {
                tracing::warn!("failed to inject fork note: {e}");
            }
        }

        // Write-through: KernelDb then DriftRouter
        {
            let db = self.kernel_db().lock().unwrap();

            let source_ws = db.get_context(source_id).ok()
                .flatten()
                .and_then(|r| r.workspace_id);

            let row = ContextRow {
                context_id: new_id,
                kernel_id,
                label: label.clone(),
                provider: resolved.provider.clone(),
                model: resolved.model.clone(),
                system_prompt: None,
                tool_filter: None,
                consent_mode: ConsentMode::Collaborative,
                created_at: kaijutsu_types::now_millis() as i64,
                created_by: caller.principal_id,
                forked_from: Some(source_id),
                fork_kind: Some(ForkKind::Compact),
                archived_at: None,
                workspace_id: source_ws,
                preset_id: None,
            };
            let default_ws = match db.get_or_create_default_workspace(kernel_id, caller.principal_id) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj fork --compact: {e}")),
            };
            if let Err(e) = db.insert_context_with_document(&row, default_ws) {
                return KjResult::Err(format!("kj fork --compact: {e}"));
            }

            if let Err(e) = db.fork_context_config(source_id, new_id) {
                return KjResult::Err(format!("kj fork --compact: failed to copy context config: {e}"));
            }

            if let Some(ref pwd) = pwd_override {
                let shell = ContextShellRow {
                    context_id: new_id,
                    cwd: Some(pwd.clone()),
                    init_script: db.get_context_shell(source_id).ok()
                        .flatten()
                        .and_then(|s| s.init_script),
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
                return KjResult::Err(format!("kj fork --compact: failed to insert structural edge: {e}"));
            }
        }

        {
            let mut drift = self.drift_router().write().await;
            if let Err(e) = drift.register_fork(new_id, label.as_deref(), source_id, caller.principal_id) {
                return KjResult::Err(format!("kj fork --compact: parent context not in router: {e}"));
            }
            if resolved.explicit {
                match (&resolved.provider, &resolved.model) {
                    (Some(p), Some(m)) => {
                        if let Err(e) = drift.configure_llm(new_id, p, m) {
                            return KjResult::Err(format!("kj fork --compact: failed to configure model: {e}"));
                        }
                    }
                    _ => {
                        return KjResult::Err(
                            "kj fork --compact: --model resolved without both provider and model".to_string()
                        );
                    }
                }
            }
        }

        let short = new_id.short();
        let display = label.as_deref().unwrap_or(&short);
        KjResult::Switch(new_id, format!("compact-forked to '{}' ({})", display, new_id.short()))
    }

    async fn fork_subtree(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let template_ref = match extract_named_arg(argv, &["--as"]) {
            Some(r) => r,
            None => return KjResult::Err("kj fork --as: requires a template context reference".to_string()),
        };
        let name = match extract_named_arg(argv, &["--name", "-n"]) {
            Some(n) => n,
            None => return KjResult::Err("kj fork --as: requires --name for the new subtree".to_string()),
        };

        let kernel_id = self.kernel_id();

        // Resolve template root
        let template_root_id = {
            let db = self.kernel_db().lock().unwrap();
            match db.resolve_context(kernel_id, &template_ref) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj fork --as: {e}")),
            }
        };

        // Get the template subtree shape
        let template_nodes = {
            let db = self.kernel_db().lock().unwrap();
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
                if let Some(ref p) = row.provider {
                    if registry.get(p).is_none() {
                        return KjResult::Err(format!(
                            "kj fork --as: template node '{}' references unknown provider '{}'",
                            row.label.as_deref().unwrap_or("(unnamed)"),
                            p,
                        ));
                    }
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
            let db = self.kernel_db().lock().unwrap();

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
                let new_forked_from = row.forked_from
                    .and_then(|fid| id_map.get(&fid).copied())
                    .or(Some(caller.context_id));

                let new_row = ContextRow {
                    context_id: new_id,
                    kernel_id,
                    label: new_label,
                    provider: row.provider.clone(),
                    model: row.model.clone(),
                    system_prompt: row.system_prompt.clone(),
                    tool_filter: row.tool_filter.clone(),
                    consent_mode: row.consent_mode,
                    created_at: kaijutsu_types::now_millis() as i64,
                    created_by: caller.principal_id,
                    forked_from: new_forked_from,
                    fork_kind: Some(ForkKind::Subtree),
                    archived_at: None,
                    workspace_id: row.workspace_id,
                    preset_id: row.preset_id,
                };
                let default_ws = match db.get_or_create_default_workspace(kernel_id, caller.principal_id) {
                    Ok(id) => id,
                    Err(e) => return KjResult::Err(format!("kj fork --as: {e}")),
                };
                if let Err(e) = db.insert_context_with_document(&new_row, default_ws) {
                    return KjResult::Err(format!("kj fork --as: failed to create context: {e}"));
                }

                // Copy shell config + env vars from template context
                if let Err(e) = db.fork_context_config(row.context_id, new_id) {
                    return KjResult::Err(format!("kj fork --as: failed to copy context config: {e}"));
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
                    Err(e) => return KjResult::Err(format!("kj fork --as: failed to read template edges: {e}")),
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
                            return KjResult::Err(format!("kj fork --as: failed to insert subtree edge: {e}"));
                        }
                    }
                }
            }

            // Edge from caller's context to the new root
            let root_edge = ContextEdgeRow {
                edge_id: uuid::Uuid::now_v7(),
                source_id: caller.context_id,
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
            let mut drift = self.drift_router().write().await;
            for (row, _depth) in &template_nodes {
                let new_id = id_map[&row.context_id];
                let is_root = row.context_id == template_root_id;
                let label = if is_root {
                    Some(name.as_str())
                } else {
                    row.label.as_deref()
                };
                let forked_from = row.forked_from
                    .and_then(|fid| id_map.get(&fid).copied())
                    .or(Some(caller.context_id));
                if let Some(parent) = forked_from {
                    if let Err(e) = drift.register_fork(new_id, label, parent, caller.principal_id) {
                        return KjResult::Err(format!("kj fork --as: parent context not in router: {e}"));
                    }
                } else {
                    drift.register(new_id, label, None, caller.principal_id);
                }
            }
        }

        KjResult::Switch(new_root_id, format!(
            "subtree-forked '{}' ({} contexts) from template '{}'",
            name, template_nodes.len(), template_ref
        ))
    }

    /// Apply a preset's settings to a context (post-fork).
    async fn apply_preset(&self, context_id: ContextId, preset_label: &str) -> Result<(), String> {
        let kernel_id = self.kernel_id();
        let preset = {
            let db = self.kernel_db().lock().unwrap();
            db.get_preset_by_label(kernel_id, preset_label)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("preset '{}' not found", preset_label))?
        };

        // Update DB
        {
            let db = self.kernel_db().lock().unwrap();
            if preset.provider.is_some() || preset.model.is_some() {
                db.update_model(
                    context_id,
                    preset.provider.as_deref(),
                    preset.model.as_deref(),
                ).map_err(|e| e.to_string())?;
            }
            db.update_settings(
                context_id,
                preset.system_prompt.as_deref(),
                &preset.tool_filter,
                preset.consent_mode,
            ).map_err(|e| e.to_string())?;
        }

        // Update DriftRouter
        {
            let mut drift = self.drift_router().write().await;
            if let (Some(p), Some(m)) = (&preset.provider, &preset.model) {
                let _ = drift.configure_llm(context_id, p, m);
            }
            if preset.tool_filter.is_some() {
                let _ = drift.configure_tools(context_id, preset.tool_filter.clone());
            }
        }

        Ok(())
    }

    fn inject_fork_note(
        &self,
        target_id: ContextId,
        caller: &KjCaller,
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
                caller.context_id,
                None,
                DriftKind::Push,
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
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
        let source = register_context(&d, Some("source"), None, principal).await;

        // Create a document for the source context
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(source);
        let result = d.dispatch(&[s("fork"), s("--name"), s("branch")], &c).await;
        assert!(result.is_ok(), "fork failed: {}", result.message());
        assert!(result.message().contains("branch"), "msg: {}", result.message());

        // Verify new context exists in DB
        let db = d.kernel_db().lock().unwrap();
        let contexts = db.list_active_contexts(d.kernel_id()).unwrap();
        assert!(contexts.iter().any(|r| r.label.as_deref() == Some("branch")));
    }

    #[tokio::test]
    async fn fork_no_name() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("src"), None, principal).await;
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
        let source = register_context(&d, Some("src"), None, principal).await;
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(source);
        let result = d
            .dispatch(
                &[s("fork"), s("--name"), s("noted"), s("--prompt"), s("explore auth bug")],
                &c,
            )
            .await;
        assert!(result.is_ok(), "fork failed: {}", result.message());
    }

    #[tokio::test]
    async fn fork_help() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d.dispatch(&[s("fork"), s("help")], &c).await;
        assert!(result.is_ok());
        assert!(result.message().contains("## Fork Kinds"));
    }

    #[tokio::test]
    async fn fork_compact_empty_source_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("empty-src"), None, principal).await;

        // Create empty document
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(source);
        let result = d.dispatch(&[s("fork"), s("--compact"), s("--name"), s("compacted")], &c).await;
        assert!(!result.is_ok(), "should fail on empty source: {}", result.message());
        assert!(result.message().contains("no blocks"), "msg: {}", result.message());
    }

    #[tokio::test]
    async fn fork_inherits_config() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("src"), None, principal).await;
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        // Set shell config and env on source
        {
            let db = d.kernel_db().lock().unwrap();
            db.upsert_context_shell(&crate::kernel_db::ContextShellRow {
                context_id: source,
                cwd: Some("/home/user/project".into()),
                init_script: None,
                updated_at: kaijutsu_types::now_millis() as i64,
            }).unwrap();
            db.set_context_env(source, "RUST_LOG", "debug").unwrap();
            db.set_context_env(source, "EDITOR", "vim").unwrap();
        }

        let c = caller_with_context(source);
        let result = d.dispatch(&[s("fork"), s("--name"), s("child")], &c).await;
        assert!(result.is_ok(), "fork failed: {}", result.message());

        // Find the new context and verify config was copied
        let db = d.kernel_db().lock().unwrap();
        let child = db.find_context_by_label(d.kernel_id(), "child").unwrap().unwrap();
        let shell = db.get_context_shell(child.context_id).unwrap().unwrap();
        assert_eq!(shell.cwd, Some("/home/user/project".into()));
        let env = db.get_context_env(child.context_id).unwrap();
        assert_eq!(env.len(), 2);
    }

    #[tokio::test]
    async fn fork_pwd_override() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("src"), None, principal).await;
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        // Set cwd on source
        {
            let db = d.kernel_db().lock().unwrap();
            db.upsert_context_shell(&crate::kernel_db::ContextShellRow {
                context_id: source,
                cwd: Some("/home/user/project".into()),
                init_script: None,
                updated_at: kaijutsu_types::now_millis() as i64,
            }).unwrap();
        }

        let c = caller_with_context(source);
        let result = d.dispatch(
            &[s("fork"), s("--name"), s("research"), s("--pwd"), s("/home/user/src/bevy_vello")],
            &c,
        ).await;
        assert!(result.is_ok(), "fork failed: {}", result.message());

        let db = d.kernel_db().lock().unwrap();
        let child = db.find_context_by_label(d.kernel_id(), "research").unwrap().unwrap();
        let shell = db.get_context_shell(child.context_id).unwrap().unwrap();
        assert_eq!(shell.cwd, Some("/home/user/src/bevy_vello".into()));
    }

    /// Register a mock LLM provider on the kernel so --model validation passes.
    async fn register_mock_provider(d: &super::super::KjDispatcher) {
        use std::sync::Arc;
        use crate::llm::{MockClient, RigProvider};
        let mock = Arc::new(RigProvider::Mock(MockClient::new("mock response")));
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
        let mut drift = d.drift_router().write().await;
        let _ = drift.configure_llm(id, provider, model);
    }

    #[tokio::test]
    async fn fork_inherits_parent_model_in_db() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal).await;
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
        let db = d.kernel_db().lock().unwrap();
        let child = db.find_context_by_label(d.kernel_id(), "child").unwrap().unwrap();
        assert_eq!(child.provider.as_deref(), Some("mock"), "child should inherit parent provider");
        assert_eq!(child.model.as_deref(), Some("mock-model"), "child should inherit parent model");
    }

    #[tokio::test]
    async fn fork_model_flag_overrides_parent() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal).await;
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        register_mock_provider(&d).await;
        configure_context_model(&d, source, "mock", "mock-model").await;

        let c = caller_with_context(source);
        let result = d.dispatch(
            &[s("fork"), s("--name"), s("override"), s("--model"), s("mock/custom-model")],
            &c,
        ).await;
        assert!(result.is_ok(), "fork failed: {}", result.message());

        // Verify child has overridden model in DB
        let db = d.kernel_db().lock().unwrap();
        let child = db.find_context_by_label(d.kernel_id(), "override").unwrap().unwrap();
        assert_eq!(child.provider.as_deref(), Some("mock"));
        assert_eq!(child.model.as_deref(), Some("custom-model"));

        // And in DriftRouter
        drop(db);
        let drift = d.drift_router().read().await;
        let handle = drift.get(child.context_id).expect("child should be in DriftRouter");
        assert_eq!(handle.provider.as_deref(), Some("mock"));
        assert_eq!(handle.model.as_deref(), Some("custom-model"));
    }

    #[tokio::test]
    async fn fork_invalid_provider_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal).await;
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(source);
        let result = d.dispatch(
            &[s("fork"), s("--name"), s("bad"), s("--model"), s("nonexistent/foo")],
            &c,
        ).await;
        assert!(!result.is_ok(), "should have failed: {}", result.message());
        assert!(
            result.message().contains("unknown provider"),
            "expected 'unknown provider' error, got: {}",
            result.message()
        );

        // Verify no context was created (mutation didn't happen)
        let db = d.kernel_db().lock().unwrap();
        let found = db.find_context_by_label(d.kernel_id(), "bad").unwrap();
        assert!(found.is_none(), "no context should have been created for invalid provider");
    }

    /// Bare model name (no provider/ prefix) should resolve provider from registry.
    /// This is the bug that caused `kj fork --model claude-sonnet-4-6` to silently
    /// keep the parent's model.
    #[tokio::test]
    async fn fork_bare_model_resolves_provider() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal).await;
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
        let result = d.dispatch(
            &[s("fork"), s("--name"), s("bare"), s("--model"), s("new-model")],
            &c,
        ).await;
        assert!(result.is_ok(), "fork failed: {}", result.message());

        // Verify provider was resolved from registry default
        let db = d.kernel_db().lock().unwrap();
        let child = db.find_context_by_label(d.kernel_id(), "bare").unwrap().unwrap();
        assert_eq!(child.provider.as_deref(), Some("mock"), "provider should be resolved from registry");
        assert_eq!(child.model.as_deref(), Some("new-model"));

        // And in DriftRouter
        drop(db);
        let drift = d.drift_router().read().await;
        let handle = drift.get(child.context_id).expect("child should be in DriftRouter");
        assert_eq!(handle.provider.as_deref(), Some("mock"), "DriftRouter provider should match");
        assert_eq!(handle.model.as_deref(), Some("new-model"), "DriftRouter model should match");
    }

    /// Fork should return Switch so the session moves to the new context.
    #[tokio::test]
    async fn fork_returns_switch() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal).await;
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(source);
        let result = d.dispatch(&[s("fork"), s("--name"), s("child")], &c).await;
        match &result {
            super::super::KjResult::Switch(id, msg) => {
                assert_ne!(*id, source, "should switch to new context, not stay on source");
                assert!(msg.contains("child"), "msg: {msg}");
            }
            other => panic!("expected Switch, got: {}", other.message()),
        }
    }

    #[tokio::test]
    async fn fork_inherits_workspace() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("src"), None, principal).await;
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        // Bind a workspace to source
        let ws_id = kaijutsu_types::WorkspaceId::new();
        {
            let db = d.kernel_db().lock().unwrap();
            db.insert_workspace(&crate::kernel_db::WorkspaceRow {
                workspace_id: ws_id,
                kernel_id: d.kernel_id(),
                label: "test-ws".into(),
                description: None,
                created_at: kaijutsu_types::now_millis() as i64,
                created_by: principal,
                archived_at: None,
            }).unwrap();
            db.update_workspace(source, Some(ws_id)).unwrap();
        }

        let c = caller_with_context(source);
        let result = d.dispatch(&[s("fork"), s("--name"), s("child")], &c).await;
        assert!(result.is_ok(), "fork failed: {}", result.message());

        let db = d.kernel_db().lock().unwrap();
        let child = db.find_context_by_label(d.kernel_id(), "child").unwrap().unwrap();
        assert_eq!(child.workspace_id, Some(ws_id));
    }
}
