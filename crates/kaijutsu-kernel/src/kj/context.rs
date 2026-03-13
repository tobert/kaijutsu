//! Context subcommands: list, info, switch, create, set, log, move, archive, remove, retag.

use kaijutsu_types::{ConsentMode, ContextId, EdgeKind};

use crate::kernel_db::{ContextEdgeRow, ContextRow, ContextShellRow};

use super::format::{format_context_info, format_context_table, format_context_tree, format_fork_lineage};
use super::parse::{extract_named_arg, parse_model_spec, parse_tool_filter_spec};
use super::refs::{parse_context_ref, resolve_context_ref};
use super::{KjCaller, KjDispatcher, KjResult};

impl KjDispatcher {
    pub(crate) async fn dispatch_context(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return KjResult::Err(self.context_help());
        }

        match argv[0].as_str() {
            "list" | "ls" => self.context_list(argv, caller).await,
            "info" => self.context_info(argv, caller),
            "switch" | "sw" => self.context_switch(argv, caller).await,
            "create" | "new" => self.context_create(argv, caller).await,
            "set" => self.context_set(argv, caller).await,
            "unset" => self.context_unset(argv, caller),
            "log" => self.context_log(argv, caller),
            "move" | "mv" => self.context_move(argv, caller).await,
            "archive" => self.context_archive(argv, caller).await,
            "remove" | "rm" => self.context_remove(argv, caller).await,
            "retag" => self.context_retag(argv, caller).await,
            "help" | "--help" | "-h" => KjResult::Ok(self.context_help()),
            other => KjResult::Err(format!(
                "kj context: unknown subcommand '{}'\n\n{}",
                other,
                self.context_help()
            )),
        }
    }

    fn context_help(&self) -> String {
        include_str!("../../docs/help/kj-context.md").to_string()
    }

    async fn context_list(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let tree = argv.iter().any(|a| a == "--tree" || a == "-t");
        let kernel_id = self.kernel_id();

        let db = self.kernel_db().lock().unwrap();
        if tree {
            match db.context_dag(kernel_id) {
                Ok(dag) => KjResult::Ok(format_context_tree(&dag, caller.context_id)),
                Err(e) => KjResult::Err(format!("kj context list: {e}")),
            }
        } else {
            match db.list_active_contexts(kernel_id) {
                Ok(contexts) => KjResult::Ok(format_context_table(&contexts, caller.context_id)),
                Err(e) => KjResult::Err(format!("kj context list: {e}")),
            }
        }
    }

    fn context_info(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let db = self.kernel_db().lock().unwrap();
        let kernel_id = self.kernel_id();

        // Resolve target context (default: current)
        let target_arg = argv.get(1).map(|s| s.as_str());
        let target_id = match super::refs::resolve_context_arg(target_arg, caller, &db, kernel_id) {
            Ok(id) => id,
            Err(e) => return KjResult::Err(format!("kj context info: {e}")),
        };

        let row = match db.get_context(target_id) {
            Ok(Some(r)) => r,
            Ok(None) => return KjResult::Err(format!("kj context info: not found")),
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

        let is_current = target_id == caller.context_id;
        let mut info = format_context_info(
            &row,
            children_count,
            drift_from + drift_to,
            is_current,
        );

        // Shell config
        if let Ok(Some(shell)) = db.get_context_shell(target_id) {
            if let Some(cwd) = &shell.cwd {
                info.push_str(&format!("\nCwd:     {cwd}"));
            }
            if let Some(init) = &shell.init_script {
                let preview = if init.len() > 60 { &init[..60] } else { init };
                info.push_str(&format!("\nInit:    {preview}..."));
            }
        }

        // Env vars
        if let Ok(vars) = db.get_context_env(target_id) {
            if !vars.is_empty() {
                info.push_str("\nEnv:");
                for v in &vars {
                    info.push_str(&format!("\n  {}={}", v.key, v.value));
                }
            }
        }

        // Workspace paths
        if let Ok(Some(paths)) = db.context_workspace_paths(target_id) {
            if !paths.is_empty() {
                // Get workspace label
                let ws_label = row.workspace_id
                    .and_then(|wsid| db.get_workspace(wsid).ok().flatten())
                    .map(|ws| ws.label)
                    .unwrap_or_else(|| "?".into());
                info.push_str(&format!("\nWorkspace: {ws_label}"));
                for p in &paths {
                    let ro = if p.read_only { " (ro)" } else { "" };
                    info.push_str(&format!("\n  {}{ro}", p.path));
                }
            }
        }

        KjResult::Ok(info)
    }

    async fn context_switch(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let query = match argv.get(1) {
            Some(q) => q.as_str(),
            None => return KjResult::Err("kj context switch: requires a context reference".to_string()),
        };

        let ctx_ref = parse_context_ref(query);

        // Resolve using DriftRouter for live state (not just DB)
        let resolved = {
            let db = self.kernel_db().lock().unwrap();
            resolve_context_ref(&ctx_ref, caller, &db, self.kernel_id())
        };

        match resolved {
            Ok(target_id) => {
                if target_id == caller.context_id {
                    return KjResult::Ok("already in that context".to_string());
                }
                // Get label for display
                let label = {
                    let router = self.drift_router().read().await;
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

    async fn context_create(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        // Parse args: kj context create <label> [--parent <ctx>]
        let label = match argv.get(1) {
            Some(l) => l.as_str(),
            None => return KjResult::Err("kj context create: requires a label".to_string()),
        };

        // Find --parent flag
        let parent_id = if let Some(idx) = argv.iter().position(|a| a == "--parent" || a == "-p") {
            let parent_ref = match argv.get(idx + 1) {
                Some(r) => r.as_str(),
                None => {
                    return KjResult::Err(
                        "kj context create: --parent requires a context reference".to_string(),
                    )
                }
            };
            let db = self.kernel_db().lock().unwrap();
            match super::refs::resolve_context_arg(Some(parent_ref), caller, &db, self.kernel_id())
            {
                Ok(id) => Some(id),
                Err(e) => return KjResult::Err(format!("kj context create: {e}")),
            }
        } else {
            // Default parent: current context
            Some(caller.context_id)
        };

        let new_id = ContextId::new();
        let kernel_id = self.kernel_id();

        // Write-through: KernelDb first, then DriftRouter
        {
            let db = self.kernel_db().lock().unwrap();
            let row = ContextRow {
                context_id: new_id,
                kernel_id,
                label: Some(label.to_string()),
                provider: None,
                model: None,
                system_prompt: None,
                tool_filter: None,
                consent_mode: ConsentMode::Collaborative,
                created_at: kaijutsu_types::now_millis() as i64,
                created_by: caller.principal_id,
                forked_from: parent_id,
                fork_kind: None,
                archived_at: None,
                workspace_id: None,
                preset_id: None,
            };
            if let Err(e) = db.insert_context(&row) {
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
            let mut drift = self.drift_router().write().await;
            drift.register(new_id, Some(label), parent_id, caller.principal_id);
        }

        KjResult::Ok(format!("created context '{}' ({})", label, new_id.short()))
    }

    /// `kj context set <ctx> [--model p/m] [--system-prompt text] [--tool-filter spec] [--consent mode] [--cwd path] [--env KEY=VALUE]`
    async fn context_set(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let kernel_id = self.kernel_id();

        // Parse all args upfront (no locks needed)
        let target_arg = argv.get(1)
            .filter(|a| !a.starts_with('-'))
            .map(|s| s.as_str());
        let model_spec = extract_named_arg(argv, &["--model", "-m"]);
        let system_prompt = extract_named_arg(argv, &["--system-prompt"]);
        let tool_filter_spec = extract_named_arg(argv, &["--tool-filter"]);
        let consent_spec = extract_named_arg(argv, &["--consent"]);
        let cwd_spec = extract_named_arg(argv, &["--cwd"]);
        let env_spec = extract_named_arg(argv, &["--env"]);

        // Resolve target + apply DB changes (lock scope)
        let (target_id, changes, tool_filter_for_drift, model_for_drift) = {
            let db = self.kernel_db().lock().unwrap();

            let target_id = match super::refs::resolve_context_arg(target_arg, caller, &db, kernel_id) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj context set: {e}")),
            };

            let mut changes = Vec::new();
            let mut tool_filter_for_drift = None;
            let mut model_for_drift: Option<(String, String)> = None;

            // Update model
            if let Some(ref spec) = model_spec {
                let (provider, model) = parse_model_spec(spec);
                if let Err(e) = db.update_model(target_id, provider.as_deref(), model.as_deref()) {
                    return KjResult::Err(format!("kj context set: {e}"));
                }
                changes.push(format!("model={spec}"));
                if let (Some(p), Some(m)) = (provider, model) {
                    model_for_drift = Some((p, m));
                }
            }

            // Parse tool filter
            let tool_filter = match tool_filter_spec {
                Some(ref spec) => match parse_tool_filter_spec(spec) {
                    Ok(tf) => Some(tf),
                    Err(e) => return KjResult::Err(format!("kj context set: {e}")),
                },
                None => None,
            };

            // Parse consent mode
            let consent_mode = match consent_spec {
                Some(ref spec) => {
                    match spec.parse::<ConsentMode>() {
                        Ok(cm) => Some(cm),
                        Err(_) => return KjResult::Err(format!("kj context set: invalid consent mode '{}' — use 'collaborative' or 'autonomous'", spec)),
                    }
                }
                None => None,
            };

            // Apply settings
            if system_prompt.is_some() || tool_filter.is_some() || consent_mode.is_some() {
                let current = match db.get_context(target_id) {
                    Ok(Some(row)) => row,
                    Ok(None) => return KjResult::Err("kj context set: context not found".to_string()),
                    Err(e) => return KjResult::Err(format!("kj context set: {e}")),
                };

                let new_prompt = system_prompt.as_deref().or(current.system_prompt.as_deref());
                let new_filter = if tool_filter.is_some() { &tool_filter } else { &current.tool_filter };
                let new_consent = consent_mode.unwrap_or(current.consent_mode);

                if let Err(e) = db.update_settings(target_id, new_prompt, new_filter, new_consent) {
                    return KjResult::Err(format!("kj context set: {e}"));
                }

                if system_prompt.is_some() { changes.push("system-prompt".to_string()); }
                if tool_filter.is_some() {
                    changes.push(format!("tool-filter={}", tool_filter_spec.as_deref().unwrap_or("?")));
                    tool_filter_for_drift = tool_filter;
                }
                if consent_mode.is_some() {
                    changes.push(format!("consent={}", consent_spec.as_deref().unwrap_or("?")));
                }
            }

            // Update cwd
            if let Some(ref cwd) = cwd_spec {
                let existing = db.get_context_shell(target_id).ok().flatten();
                let shell = ContextShellRow {
                    context_id: target_id,
                    cwd: Some(cwd.clone()),
                    init_script: existing.and_then(|s| s.init_script),
                    updated_at: kaijutsu_types::now_millis() as i64,
                };
                if let Err(e) = db.upsert_context_shell(&shell) {
                    return KjResult::Err(format!("kj context set: {e}"));
                }
                changes.push(format!("cwd={cwd}"));
            }

            // Update env var (KEY=VALUE)
            if let Some(ref env) = env_spec {
                if let Some((key, value)) = env.split_once('=') {
                    if let Err(e) = db.set_context_env(target_id, key, value) {
                        return KjResult::Err(format!("kj context set: {e}"));
                    }
                    changes.push(format!("env {key}={value}"));
                } else {
                    return KjResult::Err("kj context set: --env requires KEY=VALUE format".to_string());
                }
            }

            (target_id, changes, tool_filter_for_drift, model_for_drift)
        };
        // db lock released here

        // Update DriftRouter (async, no db lock held)
        if model_for_drift.is_some() || tool_filter_for_drift.is_some() {
            let mut drift = self.drift_router().write().await;
            if let Some((ref p, ref m)) = model_for_drift {
                let _ = drift.configure_llm(target_id, p, m);
            }
            if let Some(tf) = tool_filter_for_drift {
                let _ = drift.configure_tools(target_id, Some(tf));
            }
        }

        if changes.is_empty() {
            return KjResult::Ok("no changes specified".to_string());
        }

        KjResult::Ok(format!("updated: {}", changes.join(", ")))
    }

    /// `kj context unset [<ctx>] --env KEY` — remove an env var from a context.
    fn context_unset(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let kernel_id = self.kernel_id();

        let target_arg = argv.get(1)
            .filter(|a| !a.starts_with('-'))
            .map(|s| s.as_str());
        let env_key = extract_named_arg(argv, &["--env"]);

        let db = self.kernel_db().lock().unwrap();
        let target_id = match super::refs::resolve_context_arg(target_arg, caller, &db, kernel_id) {
            Ok(id) => id,
            Err(e) => return KjResult::Err(format!("kj context unset: {e}")),
        };

        if let Some(key) = env_key {
            match db.delete_context_env(target_id, &key) {
                Ok(true) => KjResult::Ok(format!("unset env {key}")),
                Ok(false) => KjResult::Err(format!("kj context unset: env var '{}' not set", key)),
                Err(e) => KjResult::Err(format!("kj context unset: {e}")),
            }
        } else {
            KjResult::Err("kj context unset: requires --env KEY".to_string())
        }
    }

    /// `kj context log [<ctx>]` — show fork lineage from context up to root.
    fn context_log(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let db = self.kernel_db().lock().unwrap();
        let kernel_id = self.kernel_id();

        let target_arg = argv.get(1).map(|s| s.as_str());
        let target_id = match super::refs::resolve_context_arg(target_arg, caller, &db, kernel_id) {
            Ok(id) => id,
            Err(e) => return KjResult::Err(format!("kj context log: {e}")),
        };

        match db.fork_lineage(target_id) {
            Ok(lineage) => KjResult::Ok(format_fork_lineage(&lineage, caller.context_id)),
            Err(e) => KjResult::Err(format!("kj context log: {e}")),
        }
    }

    /// `kj context move <ctx> <new-parent>` — reparent a context.
    async fn context_move(&self, argv: &[String], _caller: &KjCaller) -> KjResult {
        let ctx_ref = match argv.get(1) {
            Some(r) => r.as_str(),
            None => return KjResult::Err("kj context move: requires a context reference".to_string()),
        };
        let new_parent_ref = match argv.get(2) {
            Some(r) => r.as_str(),
            None => return KjResult::Err("kj context move: requires a new parent reference".to_string()),
        };

        let kernel_id = self.kernel_id();

        // All DB work in a single lock scope, no await
        let db = self.kernel_db().lock().unwrap();

        let ctx_id = match db.resolve_context(kernel_id, ctx_ref) {
            Ok(id) => id,
            Err(e) => return KjResult::Err(format!("kj context move: {e}")),
        };
        let new_parent_id = match db.resolve_context(kernel_id, new_parent_ref) {
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

        let ctx_label = db.get_context(ctx_id).ok()
            .flatten()
            .and_then(|r| r.label)
            .unwrap_or_else(|| ctx_id.short());
        let parent_label = db.get_context(new_parent_id).ok()
            .flatten()
            .and_then(|r| r.label)
            .unwrap_or_else(|| new_parent_id.short());

        KjResult::Ok(format!("moved '{}' under '{}'", ctx_label, parent_label))
    }

    /// `kj context archive <ctx>` — soft-delete a context (latched).
    async fn context_archive(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let ctx_ref = match argv.get(1) {
            Some(r) => r.as_str(),
            None => return KjResult::Err("kj context archive: requires a context reference".to_string()),
        };

        let kernel_id = self.kernel_id();
        let (target_id, target_label) = {
            let db = self.kernel_db().lock().unwrap();
            let target_id = match super::refs::resolve_context_arg(Some(ctx_ref), caller, &db, kernel_id) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj context archive: {e}")),
            };
            let label = db.get_context(target_id).ok()
                .flatten()
                .and_then(|r| r.label)
                .unwrap_or_else(|| target_id.short());
            (target_id, label)
        };

        if !caller.confirmed {
            // Gather stats for latch message
            let db = self.kernel_db().lock().unwrap();
            let block_count = self.block_store().get(target_id)
                .map(|e| e.doc.block_count())
                .unwrap_or(0);
            let children_count = db.structural_children(target_id)
                .map(|c| c.len())
                .unwrap_or(0);
            let drift_from = db.edges_from(target_id, Some(EdgeKind::Drift)).map(|e| e.len()).unwrap_or(0);
            let drift_to = db.edges_to(target_id, Some(EdgeKind::Drift)).map(|e| e.len()).unwrap_or(0);

            return KjResult::Latch {
                command: "kj context archive".to_string(),
                target: target_label,
                message: format!("{} blocks | {} children | {} drift edges", block_count, children_count, drift_from + drift_to),
            };
        }

        // Archive the target + recursive children
        let db = self.kernel_db().lock().unwrap();
        let subtree = db.subtree_snapshot(target_id).unwrap_or_default();
        let mut archived = 0;
        for (row, _depth) in &subtree {
            if db.archive_context(row.context_id).unwrap_or(false) {
                archived += 1;
            }
        }

        KjResult::Ok(format!("archived {} context(s)", archived))
    }

    /// `kj context remove <ctx>` — permanently delete a context (latched).
    async fn context_remove(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let ctx_ref = match argv.get(1) {
            Some(r) => r.as_str(),
            None => return KjResult::Err("kj context remove: requires a context reference".to_string()),
        };

        let kernel_id = self.kernel_id();
        let (target_id, target_label) = {
            let db = self.kernel_db().lock().unwrap();
            let target_id = match super::refs::resolve_context_arg(Some(ctx_ref), caller, &db, kernel_id) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj context remove: {e}")),
            };
            let label = db.get_context(target_id).ok()
                .flatten()
                .and_then(|r| r.label)
                .unwrap_or_else(|| target_id.short());
            (target_id, label)
        };

        if target_id == caller.context_id {
            return KjResult::Err("kj context remove: cannot remove the current context".to_string());
        }

        if !caller.confirmed {
            let db = self.kernel_db().lock().unwrap();
            let block_count = self.block_store().get(target_id)
                .map(|e| e.doc.block_count())
                .unwrap_or(0);
            let children_count = db.structural_children(target_id)
                .map(|c| c.len())
                .unwrap_or(0);

            return KjResult::Latch {
                command: "kj context remove".to_string(),
                target: target_label,
                message: format!("{} blocks | {} children — this is permanent", block_count, children_count),
            };
        }

        // Delete from DB (CASCADE deletes edges)
        {
            let db = self.kernel_db().lock().unwrap();
            if let Err(e) = db.delete_context(target_id) {
                return KjResult::Err(format!("kj context remove: {e}"));
            }
        }

        // Remove document from BlockStore
        let _ = self.block_store().delete_document(target_id);

        // Unregister from DriftRouter (no db lock held)
        let mut drift = self.drift_router().write().await;
        drift.unregister(target_id);

        KjResult::Ok(format!("removed context '{}'", target_label))
    }

    /// `kj context retag <label> <ctx>` — move a label to a different context (latched).
    async fn context_retag(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let label = match argv.get(1) {
            Some(l) => l.as_str(),
            None => return KjResult::Err("kj context retag: requires a label".to_string()),
        };
        let ctx_ref = match argv.get(2) {
            Some(r) => r.as_str(),
            None => return KjResult::Err("kj context retag: requires a target context reference".to_string()),
        };

        let kernel_id = self.kernel_id();

        // Resolve the new holder and find old holder (single lock scope)
        let (new_holder_id, old_holder) = {
            let db = self.kernel_db().lock().unwrap();
            let new_holder_id = match super::refs::resolve_context_arg(Some(ctx_ref), caller, &db, kernel_id) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj context retag: {e}")),
            };
            let old_holder = db.find_context_by_label(kernel_id, label)
                .ok()
                .flatten();
            (new_holder_id, old_holder)
        };

        if !caller.confirmed {
            let current_holder = old_holder.as_ref()
                .map(|r| {
                    let old_short = r.context_id.short();
                    format!("currently held by {} ({})", r.label.as_deref().unwrap_or(&old_short), old_short)
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
            let db = self.kernel_db().lock().unwrap();
            if let Some(ref old) = old_holder {
                if let Err(e) = db.update_label(old.context_id, None) {
                    return KjResult::Err(format!("kj context retag: failed to clear old label: {e}"));
                }
            }
            if let Err(e) = db.update_label(new_holder_id, Some(label)) {
                return KjResult::Err(format!("kj context retag: {e}"));
            }
        }

        // Update DriftRouter labels (no db lock held)
        let mut drift = self.drift_router().write().await;
        if let Some(ref old) = old_holder {
            let _ = drift.rename(old.context_id, None);
        }
        let _ = drift.rename(new_holder_id, Some(label));

        KjResult::Ok(format!("retagged '{}' → {}", label, new_holder_id.short()))
    }
}

#[cfg(test)]
mod tests {
    use crate::kernel_db::ContextEdgeRow;
    use crate::kj::test_helpers::*;
    #[allow(unused_imports)]
    use crate::kj::KjResult;
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
        let ctx_id = register_context(&d, Some("default"), None, principal).await;
        let _ = register_context(&d, Some("alt"), None, principal).await;

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
        let root = register_context(&d, Some("root"), None, principal).await;

        // Add structural edge for child
        let child = register_context(&d, Some("child"), Some(root), principal).await;
        {
            let db = d.kernel_db().lock().unwrap();
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
        let ctx_id = register_context(&d, Some("myctx"), None, principal).await;

        let c = caller_with_context(ctx_id);
        let result = d.dispatch(&[s("context"), s("info")], &c).await;
        assert!(result.is_ok());
        let msg = result.message();
        assert!(msg.contains("myctx *"), "output: {msg}");
    }

    #[tokio::test]
    async fn context_switch_by_label() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx_a = register_context(&d, Some("alpha"), None, principal).await;
        let _ctx_b = register_context(&d, Some("beta"), None, principal).await;

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
        let ctx = register_context(&d, Some("only"), None, principal).await;

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
        let parent = register_context(&d, Some("parent"), None, principal).await;

        let c = caller_with_context(parent);
        let result = d
            .dispatch(&[s("context"), s("create"), s("child-ctx")], &c)
            .await;
        assert!(result.is_ok());
        assert!(result.message().contains("child-ctx"), "msg: {}", result.message());

        // Verify it's in the DB
        let db = d.kernel_db().lock().unwrap();
        let contexts = db.list_active_contexts(d.kernel_id()).unwrap();
        assert!(contexts.iter().any(|r| r.label.as_deref() == Some("child-ctx")));
    }

    #[tokio::test]
    async fn context_create_duplicate_label() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal).await;

        let c = caller_with_context(parent);
        // First create succeeds
        let r1 = d
            .dispatch(&[s("context"), s("create"), s("dup")], &c)
            .await;
        assert!(r1.is_ok());

        // Second create with same label should fail
        let r2 = d
            .dispatch(&[s("context"), s("create"), s("dup")], &c)
            .await;
        assert!(!r2.is_ok(), "expected error, got: {}", r2.message());
    }

    #[tokio::test]
    async fn context_help() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d.dispatch(&[s("context"), s("help")], &c).await;
        assert!(result.is_ok());
        assert!(result.message().contains("## Subcommands"));
    }

    #[tokio::test]
    async fn context_set_model() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("target"), None, principal).await;

        let c = caller_with_context(ctx);
        let result = d
            .dispatch(&[s("context"), s("set"), s("."), s("--model"), s("anthropic/claude-opus-4-6")], &c)
            .await;
        assert!(result.is_ok(), "set failed: {}", result.message());
        assert!(result.message().contains("model="), "msg: {}", result.message());

        // Verify in DriftRouter
        let router = d.drift_router().read().await;
        let handle = router.get(ctx).unwrap();
        assert_eq!(handle.provider.as_deref(), Some("anthropic"));
        assert_eq!(handle.model.as_deref(), Some("claude-opus-4-6"));
    }

    #[tokio::test]
    async fn context_log_shows_lineage() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let root = register_context(&d, Some("root"), None, principal).await;
        let child = register_context(&d, Some("child"), Some(root), principal).await;

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
        let a = register_context(&d, Some("a"), None, principal).await;
        let b = register_context(&d, Some("b"), None, principal).await;
        let child = register_context(&d, Some("child"), Some(a), principal).await;

        // Insert original structural edge a → child
        {
            let db = d.kernel_db().lock().unwrap();
            db.insert_edge(&ContextEdgeRow {
                edge_id: uuid::Uuid::now_v7(),
                source_id: a,
                target_id: child,
                kind: EdgeKind::Structural,
                metadata: None,
                created_at: kaijutsu_types::now_millis() as i64,
            }).unwrap();
        }

        let c = caller_with_context(a);
        let result = d
            .dispatch(&[s("context"), s("move"), s("child"), s("b")], &c)
            .await;
        assert!(result.is_ok(), "move failed: {}", result.message());
        assert!(result.message().contains("moved"), "msg: {}", result.message());

        // Verify new parent
        let db = d.kernel_db().lock().unwrap();
        let parents = db.structural_parents(child).unwrap();
        assert_eq!(parents.len(), 1);
        assert_eq!(parents[0].context_id, b);
    }

    #[tokio::test]
    async fn context_archive_requires_latch() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("doomed"), None, principal).await;

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
        let parent = register_context(&d, Some("parent"), None, principal).await;
        let target = register_context(&d, Some("target"), Some(parent), principal).await;

        let c = confirmed_caller(parent);
        let result = d
            .dispatch(&[s("context"), s("archive"), s("target")], &c)
            .await;
        assert!(result.is_ok(), "archive failed: {}", result.message());
        assert!(result.message().contains("archived"), "msg: {}", result.message());

        // Verify archived
        let db = d.kernel_db().lock().unwrap();
        let row = db.get_context(target).unwrap().unwrap();
        assert!(row.archived_at.is_some());
    }

    #[tokio::test]
    async fn context_remove_requires_latch() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal).await;
        let target = register_context(&d, Some("victim"), Some(parent), principal).await;

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
        let parent = register_context(&d, Some("parent"), None, principal).await;
        let target = register_context(&d, Some("target"), Some(parent), principal).await;

        let c = confirmed_caller(parent);
        let result = d
            .dispatch(&[s("context"), s("remove"), s("target")], &c)
            .await;
        assert!(result.is_ok(), "remove failed: {}", result.message());

        // Verify gone from DB
        let db = d.kernel_db().lock().unwrap();
        assert!(db.get_context(target).unwrap().is_none());

        // Verify gone from DriftRouter
        drop(db);
        let router = d.drift_router().read().await;
        assert!(router.get(target).is_none());
    }

    #[tokio::test]
    async fn context_remove_cannot_remove_current() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("current"), None, principal).await;

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
        let ctx = register_context(&d, Some("target"), None, principal).await;

        let c = caller_with_context(ctx);
        let result = d
            .dispatch(&[s("context"), s("set"), s("."), s("--cwd"), s("/tmp/work")], &c)
            .await;
        assert!(result.is_ok(), "set --cwd failed: {}", result.message());
        assert!(result.message().contains("cwd="), "msg: {}", result.message());

        let db = d.kernel_db().lock().unwrap();
        let shell = db.get_context_shell(ctx).unwrap().unwrap();
        assert_eq!(shell.cwd, Some("/tmp/work".into()));
    }

    #[tokio::test]
    async fn context_set_env() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("target"), None, principal).await;

        let c = caller_with_context(ctx);
        let result = d
            .dispatch(&[s("context"), s("set"), s("."), s("--env"), s("RUST_LOG=debug")], &c)
            .await;
        assert!(result.is_ok(), "set --env failed: {}", result.message());
        assert!(result.message().contains("env RUST_LOG=debug"), "msg: {}", result.message());

        let db = d.kernel_db().lock().unwrap();
        let env = db.get_context_env(ctx).unwrap();
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].key, "RUST_LOG");
        assert_eq!(env[0].value, "debug");
    }

    #[tokio::test]
    async fn context_set_env_bad_format() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("target"), None, principal).await;

        let c = caller_with_context(ctx);
        let result = d
            .dispatch(&[s("context"), s("set"), s("."), s("--env"), s("NOEQUALS")], &c)
            .await;
        assert!(!result.is_ok(), "should fail without =: {}", result.message());
        assert!(result.message().contains("KEY=VALUE"), "msg: {}", result.message());
    }

    #[tokio::test]
    async fn context_unset_env() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("target"), None, principal).await;

        // Set env var first
        {
            let db = d.kernel_db().lock().unwrap();
            db.set_context_env(ctx, "FOO", "bar").unwrap();
        }

        let c = caller_with_context(ctx);
        let result = d
            .dispatch(&[s("context"), s("unset"), s("."), s("--env"), s("FOO")], &c)
            .await;
        assert!(result.is_ok(), "unset failed: {}", result.message());
        assert!(result.message().contains("unset env FOO"), "msg: {}", result.message());

        // Verify it's gone
        let db = d.kernel_db().lock().unwrap();
        let env = db.get_context_env(ctx).unwrap();
        assert!(env.is_empty());
    }

    #[tokio::test]
    async fn context_unset_env_missing() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("target"), None, principal).await;

        let c = caller_with_context(ctx);
        let result = d
            .dispatch(&[s("context"), s("unset"), s("."), s("--env"), s("NOPE")], &c)
            .await;
        assert!(!result.is_ok(), "should error for missing var: {}", result.message());
    }

    #[tokio::test]
    async fn context_info_shows_shell_config() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("enriched"), None, principal).await;

        // Set shell config and env
        {
            let db = d.kernel_db().lock().unwrap();
            db.upsert_context_shell(&crate::kernel_db::ContextShellRow {
                context_id: ctx,
                cwd: Some("/home/user/project".into()),
                init_script: None,
                updated_at: kaijutsu_types::now_millis() as i64,
            }).unwrap();
            db.set_context_env(ctx, "RUST_LOG", "debug").unwrap();
        }

        let c = caller_with_context(ctx);
        let result = d.dispatch(&[s("context"), s("info")], &c).await;
        assert!(result.is_ok(), "info failed: {}", result.message());
        let msg = result.message();
        assert!(msg.contains("Cwd:"), "should show cwd: {msg}");
        assert!(msg.contains("/home/user/project"), "should show cwd path: {msg}");
        assert!(msg.contains("Env:"), "should show env: {msg}");
        assert!(msg.contains("RUST_LOG=debug"), "should show env var: {msg}");
    }
}
