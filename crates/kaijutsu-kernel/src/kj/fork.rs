//! Fork subcommand: deep-copy the current context's document.

use std::collections::HashMap;

use kaijutsu_types::{ConsentMode, ContextId, EdgeKind, ForkKind};

use crate::kernel_db::{ContextEdgeRow, ContextRow, ContextShellRow};

use super::parse::{extract_named_arg, has_flag};
use super::{KjCaller, KjDispatcher, KjResult};

impl KjDispatcher {
    pub(crate) async fn dispatch_fork(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if has_flag(argv, &["help", "--help", "-h"]) {
            return KjResult::ok_typed(self.fork_help(), "text/markdown");
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
                provider: None,
                model: None,
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
                tracing::warn!("failed to copy context config on fork: {e}");
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
                    tracing::warn!("failed to set --pwd on fork: {e}");
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
                tracing::warn!("failed to insert fork edge: {e}");
            }
        }

        // Register in DriftRouter (inherits parent's model)
        {
            let mut drift = self.drift_router().write().await;
            drift.register_fork(new_id, label.as_deref(), source_id, caller.principal_id);
        }

        // Apply preset if requested
        if let Some(ref preset) = preset_label {
            if let Err(e) = self.apply_preset(new_id, preset).await {
                tracing::warn!("failed to apply preset '{}': {e}", preset);
            }
        }

        // If --prompt given, inject a Drift block
        if let Some(note) = prompt {
            if let Err(e) = self.inject_fork_note(new_id, caller, &note) {
                tracing::warn!("failed to inject fork note: {e}");
            }
        }

        let short = new_id.short();
        let display = label
            .as_deref()
            .unwrap_or(&short);
        KjResult::ok(format!("forked to '{}' ({})", display, new_id.short()))
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
                provider: None,
                model: None,
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
                tracing::warn!("failed to copy context config on shallow fork: {e}");
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
                    tracing::warn!("failed to set --pwd on shallow fork: {e}");
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
                tracing::warn!("failed to insert fork edge: {e}");
            }
        }

        {
            let mut drift = self.drift_router().write().await;
            drift.register_fork(new_id, label.as_deref(), source_id, caller.principal_id);
        }

        if let Some(ref preset) = preset_label {
            if let Err(e) = self.apply_preset(new_id, preset).await {
                tracing::warn!("failed to apply preset '{}': {e}", preset);
            }
        }

        if let Some(note) = prompt {
            if let Err(e) = self.inject_fork_note(new_id, caller, &note) {
                tracing::warn!("failed to inject fork note: {e}");
            }
        }

        let short = new_id.short();
        let display = label.as_deref().unwrap_or(&short);
        KjResult::ok(format!("shallow-forked to '{}' ({}, depth {})", display, new_id.short(), depth))
    }

    async fn fork_compact(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let label = extract_named_arg(argv, &["--name", "-n"]);
        let prompt = extract_named_arg(argv, &["--prompt"]);
        let pwd_override = extract_named_arg(argv, &["--pwd"]);

        let source_id = caller.context_id;
        let new_id = ContextId::new();
        let kernel_id = self.kernel_id();

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
                provider: None,
                model: None,
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
                tracing::warn!("failed to copy context config on compact fork: {e}");
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
                    tracing::warn!("failed to set --pwd on compact fork: {e}");
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
                tracing::warn!("failed to insert compact fork edge: {e}");
            }
        }

        {
            let mut drift = self.drift_router().write().await;
            drift.register_fork(new_id, label.as_deref(), source_id, caller.principal_id);
        }

        let short = new_id.short();
        let display = label.as_deref().unwrap_or(&short);
        KjResult::ok(format!("compact-forked to '{}' ({})", display, new_id.short()))
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
                    tracing::warn!("failed to copy context config for subtree fork: {e}");
                }

                // Create empty document for each new context
                if let Err(e) = self.block_store().create_document(
                    new_id,
                    crate::DocumentKind::Conversation,
                    None,
                ) {
                    tracing::warn!("failed to create document for subtree fork: {e}");
                }
            }

            // Insert structural edges mirroring the template
            for (row, _depth) in &template_nodes {
                let old_parent = row.context_id;
                let new_parent = id_map[&old_parent];

                // Get template's structural children
                if let Ok(children) = db.structural_children(old_parent) {
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
                                tracing::warn!("failed to insert subtree edge: {e}");
                            }
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
                tracing::warn!("failed to insert subtree root edge: {e}");
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
                    drift.register_fork(new_id, label, parent, caller.principal_id);
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
