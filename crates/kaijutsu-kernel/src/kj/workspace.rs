//! Workspace subcommands: list, show, create, add, bind, remove.

use clap::{Parser, Subcommand};
use kaijutsu_types::{ContentType, WorkspaceId};

use crate::kernel_db::{WorkspacePathRow, WorkspaceRow};

use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug)]
#[command(
    name = "workspace",
    about = "Workspace management — group filesystem paths for mounting into contexts",
    disable_help_subcommand = true,
    no_binary_name = true
)]
struct WorkspaceArgs {
    #[command(subcommand)]
    command: WorkspaceCommand,
}

#[derive(Subcommand, Debug)]
enum WorkspaceCommand {
    /// List all workspaces.
    #[command(alias = "ls")]
    List,
    /// Show a workspace's description and paths.
    Show {
        /// Workspace label
        label: String,
    },
    /// Create a workspace with optional description and initial paths.
    #[command(alias = "new")]
    Create {
        /// Workspace label
        label: String,
        /// Description text
        #[arg(long, alias = "description")]
        desc: Option<String>,
        /// Initial path (repeatable)
        #[arg(long)]
        path: Vec<String>,
    },
    /// Add a path to an existing workspace.
    Add {
        /// Workspace label
        label: String,
        /// Path to add
        path: String,
        /// Mount point (documented; currently unused — see note in migration)
        #[arg(long)]
        mount: Option<String>,
        /// Mark the path read-only
        #[arg(long)]
        read_only: bool,
    },
    /// Bind a workspace to a context.
    Bind {
        /// Workspace label
        label: String,
        /// Target context: . (default) | .parent | <label> | <hex prefix>
        context: Option<String>,
    },
    /// Archive a workspace (latched).
    #[command(alias = "rm")]
    Remove {
        /// Workspace label
        label: String,
    },
}

impl KjDispatcher {
    pub(crate) fn dispatch_workspace(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<WorkspaceArgs>();
        }
        let parsed = match WorkspaceArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj workspace: {e}"));
            }
        };

        // Workspace mutation is operator authority; list/show stay ungated.
        if matches!(
            parsed.command,
            WorkspaceCommand::Create { .. }
                | WorkspaceCommand::Add { .. }
                | WorkspaceCommand::Bind { .. }
                | WorkspaceCommand::Remove { .. }
        ) && let Err(denied) =
            self.require_cap(caller, crate::mcp::Capability::Operator, "workspace")
        {
            return denied;
        }

        match parsed.command {
            WorkspaceCommand::List => self.workspace_list(),
            WorkspaceCommand::Show { label } => self.workspace_show(&label),
            WorkspaceCommand::Create { label, desc, path } => {
                self.workspace_create(&label, desc, &path, caller)
            }
            WorkspaceCommand::Add {
                label,
                path,
                mount: _,
                read_only,
            } => self.workspace_add(&label, &path, read_only),
            WorkspaceCommand::Bind { label, context } => {
                self.workspace_bind(&label, context.as_deref(), caller)
            }
            WorkspaceCommand::Remove { label } => self.workspace_remove(&label, caller),
        }
    }

    fn workspace_list(&self) -> KjResult {
        let db = self.kernel_db().lock();
        match db.list_workspaces() {
            Ok(workspaces) => {
                // Workspace labels are the resolver key (`get_workspace_by_label`)
                // and are required by the schema, so labels are the canonical
                // full handle — no truncation.
                let labels = serde_json::Value::Array(
                    workspaces
                        .iter()
                        .map(|w| serde_json::Value::String(w.label.clone()))
                        .collect(),
                );
                if workspaces.is_empty() {
                    return KjResult::ok_with_data("(no workspaces)".to_string(), labels);
                }
                let lines: Vec<String> = workspaces
                    .iter()
                    .map(|w| {
                        let desc = w
                            .description
                            .as_deref()
                            .map(|d| format!("  — {d}"))
                            .unwrap_or_default();
                        format!("  {}{}", w.label, desc)
                    })
                    .collect();
                KjResult::ok_with_data(lines.join("\n"), labels)
            }
            Err(e) => KjResult::Err(format!("kj workspace list: {e}")),
        }
    }

    fn workspace_show(&self, label: &str) -> KjResult {
        let db = self.kernel_db().lock();
        match db.get_workspace_by_label(label) {
            Ok(Some(w)) => {
                let mut lines = vec![format!("Workspace: {}", w.label)];
                if let Some(desc) = &w.description {
                    lines.push(format!("Description: {desc}"));
                }

                // List paths
                match db.list_workspace_paths(w.workspace_id) {
                    Ok(paths) => {
                        if !paths.is_empty() {
                            lines.push("Paths:".to_string());
                            for p in &paths {
                                let ro = if p.read_only { " (ro)" } else { "" };
                                lines.push(format!("  {}{}", p.path, ro));
                            }
                        }
                    }
                    Err(e) => {
                        lines.push(format!("(paths error: {e})"));
                    }
                }

                KjResult::ok(lines.join("\n"))
            }
            Ok(None) => KjResult::Err(format!("kj workspace show: '{}' not found", label)),
            Err(e) => KjResult::Err(format!("kj workspace show: {e}")),
        }
    }

    /// `kj workspace create <label> [--desc text] [--path p ...]`
    fn workspace_create(
        &self,
        label: &str,
        desc: Option<String>,
        paths: &[String],
        caller: &KjCaller,
    ) -> KjResult {
        let db = self.kernel_db().lock();
        let ws_id = WorkspaceId::new();

        let row = WorkspaceRow {
            workspace_id: ws_id,
                        label: label.to_string(),
            description: desc,
            created_at: kaijutsu_types::now_millis() as i64,
            created_by: caller.principal_id,
            archived_at: None,
        };
        if let Err(e) = db.insert_workspace(&row) {
            return KjResult::Err(format!("kj workspace create: {e}"));
        }

        // Add initial paths
        for path in paths {
            let path_row = WorkspacePathRow {
                workspace_id: ws_id,
                path: path.clone(),
                read_only: false,
                created_at: kaijutsu_types::now_millis() as i64,
            };
            if let Err(e) = db.insert_workspace_path(&path_row) {
                tracing::warn!("failed to add path '{}': {e}", path);
            }
        }

        let path_msg = if paths.is_empty() {
            String::new()
        } else {
            format!(" ({} paths)", paths.len())
        };
        KjResult::ok(format!("created workspace '{}'{}", label, path_msg))
    }

    /// `kj workspace add <label> <path> [--mount m] [--read-only]`
    fn workspace_add(&self, label: &str, path: &str, read_only: bool) -> KjResult {
        let db = self.kernel_db().lock();

        let ws = match db.get_workspace_by_label(label) {
            Ok(Some(w)) => w,
            Ok(None) => {
                return KjResult::Err(format!("kj workspace add: workspace '{}' not found", label));
            }
            Err(e) => return KjResult::Err(format!("kj workspace add: {e}")),
        };

        let path_row = WorkspacePathRow {
            workspace_id: ws.workspace_id,
            path: path.to_string(),
            read_only,
            created_at: kaijutsu_types::now_millis() as i64,
        };
        match db.insert_workspace_path(&path_row) {
            Ok(()) => KjResult::ok(format!("added path '{}' to workspace '{}'", path, label)),
            Err(e) => KjResult::Err(format!("kj workspace add: {e}")),
        }
    }

    /// `kj workspace bind <label> [ctx]`
    fn workspace_bind(&self, label: &str, ctx_arg: Option<&str>, caller: &KjCaller) -> KjResult {
        let db = self.kernel_db().lock();

        let target_id = match super::refs::resolve_context_arg(ctx_arg, caller, &db) {
            Ok(id) => id,
            Err(e) => return KjResult::Err(format!("kj workspace bind: {e}")),
        };

        let ws = match db.get_workspace_by_label(label) {
            Ok(Some(w)) => w,
            Ok(None) => {
                return KjResult::Err(format!(
                    "kj workspace bind: workspace '{}' not found",
                    label
                ));
            }
            Err(e) => return KjResult::Err(format!("kj workspace bind: {e}")),
        };

        if let Err(e) = db.update_workspace(target_id, Some(ws.workspace_id)) {
            return KjResult::Err(format!("kj workspace bind: {e}"));
        }

        // Set cwd to workspace's first rw path if context has no cwd yet
        let has_cwd = db
            .get_context_shell(target_id)
            .ok()
            .flatten()
            .and_then(|s| s.cwd)
            .is_some();

        if !has_cwd
            && let Ok(paths) = db.list_workspace_paths(ws.workspace_id)
            && let Some(first_rw) = paths.iter().find(|p| !p.read_only)
        {
            let shell = crate::kernel_db::ContextShellRow {
                context_id: target_id,
                cwd: Some(first_rw.path.clone()),
                updated_at: kaijutsu_types::now_millis() as i64,
            };
            if let Err(e) = db.upsert_context_shell(&shell) {
                tracing::warn!("failed to set cwd on workspace bind: {e}");
            }
        }

        KjResult::ok(format!(
            "bound workspace '{}' to context {}",
            label,
            target_id.short()
        ))
    }

    /// `kj workspace remove <label>` — archive a workspace (latched).
    fn workspace_remove(&self, label: &str, caller: &KjCaller) -> KjResult {
        let db = self.kernel_db().lock();

        let ws = match db.get_workspace_by_label(label) {
            Ok(Some(w)) => w,
            Ok(None) => {
                return KjResult::Err(format!("kj workspace remove: '{}' not found", label));
            }
            Err(e) => return KjResult::Err(format!("kj workspace remove: {e}")),
        };

        if !caller.confirmed {
            let usage_count = db
                .contexts_using_workspace(ws.workspace_id)
                .unwrap_or(0);
            return KjResult::Latch {
                command: "kj workspace remove".to_string(),
                target: label.to_string(),
                message: format!("{} context(s) using this workspace", usage_count),
            };
        }

        match db.archive_workspace(ws.workspace_id) {
            Ok(true) => KjResult::ok(format!("archived workspace '{}'", label)),
            Ok(false) => {
                KjResult::Err(format!("kj workspace remove: '{}' already archived", label))
            }
            Err(e) => KjResult::Err(format!("kj workspace remove: {e}")),
        }
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
    async fn workspace_list_empty() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d.dispatch(&[s("workspace"), s("list")], &c).await;
        assert!(result.is_ok());
        // __default workspace is auto-created by test_dispatcher
        assert!(result.message().contains("__default"));
    }

    /// Workspaces emit their labels as the iteration handle.
    #[tokio::test]
    async fn workspace_list_emits_label_array() {
        use crate::kj::KjResult;
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("ctx"), None, principal);
        let c = caller_with_context(ctx);

        d.dispatch(&[s("workspace"), s("create"), s("alpha-ws")], &c).await;
        d.dispatch(&[s("workspace"), s("create"), s("beta-ws")], &c).await;

        let result = d.dispatch(&[s("workspace"), s("list")], &c).await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                let labels: Vec<&str> = v
                    .as_array()
                    .expect("array")
                    .iter()
                    .filter_map(|x| x.as_str())
                    .collect();
                assert!(labels.contains(&"alpha-ws"), "got: {labels:?}");
                assert!(labels.contains(&"beta-ws"), "got: {labels:?}");
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn workspace_show_not_found() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d
            .dispatch(&[s("workspace"), s("show"), s("nonexistent")], &c)
            .await;
        assert!(!result.is_ok());
        assert!(result.message().contains("not found"));
    }

    #[tokio::test]
    async fn workspace_alias() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d.dispatch(&[s("ws"), s("list")], &c).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn workspace_create_and_list() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("ctx"), None, principal);
        let c = caller_with_context(ctx);

        let result = d
            .dispatch(
                &[
                    s("workspace"),
                    s("create"),
                    s("myws"),
                    s("--desc"),
                    s("Test workspace"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());
        assert!(result.message().contains("myws"));

        let result = d.dispatch(&[s("workspace"), s("list")], &c).await;
        assert!(result.is_ok());
        assert!(
            result.message().contains("myws"),
            "msg: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn workspace_create_with_paths() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("ctx"), None, principal);
        let c = caller_with_context(ctx);

        let result = d
            .dispatch(
                &[
                    s("workspace"),
                    s("create"),
                    s("ws2"),
                    s("--path"),
                    s("/src"),
                    s("--path"),
                    s("/docs"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());
        assert!(
            result.message().contains("2 paths"),
            "msg: {}",
            result.message()
        );

        // Show should list paths
        let result = d.dispatch(&[s("workspace"), s("show"), s("ws2")], &c).await;
        assert!(result.is_ok());
        let msg = result.message();
        assert!(msg.contains("/src"), "msg: {msg}");
        assert!(msg.contains("/docs"), "msg: {msg}");
    }

    #[tokio::test]
    async fn workspace_add_path() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("ctx"), None, principal);
        let c = caller_with_context(ctx);

        d.dispatch(&[s("workspace"), s("create"), s("ws3")], &c)
            .await;

        let result = d
            .dispatch(
                &[
                    s("workspace"),
                    s("add"),
                    s("ws3"),
                    s("/extra"),
                    s("--mount"),
                    s("/mnt/extra"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "add failed: {}", result.message());
    }

    #[tokio::test]
    async fn workspace_bind_to_context() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("ctx"), None, principal);
        let c = caller_with_context(ctx);

        d.dispatch(&[s("workspace"), s("create"), s("ws4")], &c)
            .await;

        let result = d.dispatch(&[s("workspace"), s("bind"), s("ws4")], &c).await;
        assert!(result.is_ok(), "bind failed: {}", result.message());
        assert!(
            result.message().contains("bound"),
            "msg: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn workspace_remove_requires_latch() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("ctx"), None, principal);
        let c = caller_with_context(ctx);

        d.dispatch(&[s("workspace"), s("create"), s("ws5")], &c)
            .await;

        let result = d
            .dispatch(&[s("workspace"), s("remove"), s("ws5")], &c)
            .await;
        assert!(result.is_latch(), "expected latch, got: {:?}", result);
    }

    #[tokio::test]
    async fn workspace_remove_confirmed() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("ctx"), None, principal);

        let c = caller_with_context(ctx);
        d.dispatch(&[s("workspace"), s("create"), s("ws6")], &c)
            .await;

        let c = confirmed_caller(ctx);
        let result = d
            .dispatch(&[s("workspace"), s("remove"), s("ws6")], &c)
            .await;
        assert!(result.is_ok(), "remove failed: {}", result.message());
        assert!(
            result.message().contains("archived"),
            "msg: {}",
            result.message()
        );
    }
}
