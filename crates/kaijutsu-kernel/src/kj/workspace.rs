//! Workspace subcommands: list, show, create, add, bind, remove.

use kaijutsu_types::WorkspaceId;

use crate::kernel_db::{WorkspacePathRow, WorkspaceRow};

use super::parse::{extract_named_arg, extract_all_named_args, has_flag};
use super::{KjCaller, KjDispatcher, KjResult};

impl KjDispatcher {
    pub(crate) fn dispatch_workspace(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return KjResult::Err(self.workspace_help());
        }

        match argv[0].as_str() {
            "list" | "ls" => self.workspace_list(),
            "show" => self.workspace_show(argv),
            "create" | "new" => self.workspace_create(argv, caller),
            "add" => self.workspace_add(argv),
            "bind" => self.workspace_bind(argv, caller),
            "remove" | "rm" => self.workspace_remove(argv, caller),
            "help" | "--help" | "-h" => KjResult::Ok(self.workspace_help()),
            other => KjResult::Err(format!(
                "kj workspace: unknown subcommand '{}'\n\n{}",
                other,
                self.workspace_help()
            )),
        }
    }

    fn workspace_help(&self) -> String {
        "\
kj workspace — workspace management

USAGE:
    kj workspace <subcommand> [args...]

SUBCOMMANDS:
    list                    List active workspaces
    show <label>            Show workspace details and paths
    create <label> [flags]  Create a workspace
    add <label> <path> [--mount m] [--read-only]  Add a path to a workspace
    bind <label> [ctx]      Bind a workspace to a context
    remove <label>          Archive a workspace (latched)

CREATE FLAGS:
    --desc <text>           Description
    --path <p>              Add path(s) (repeatable)"
            .to_string()
    }

    fn workspace_list(&self) -> KjResult {
        let db = self.kernel_db().lock().unwrap();
        match db.list_workspaces(self.kernel_id()) {
            Ok(workspaces) => {
                if workspaces.is_empty() {
                    return KjResult::Ok("(no workspaces)".to_string());
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
                KjResult::Ok(lines.join("\n"))
            }
            Err(e) => KjResult::Err(format!("kj workspace list: {e}")),
        }
    }

    fn workspace_show(&self, argv: &[String]) -> KjResult {
        let label = match argv.get(1) {
            Some(l) => l.as_str(),
            None => return KjResult::Err("kj workspace show: requires a label".to_string()),
        };

        let db = self.kernel_db().lock().unwrap();
        match db.get_workspace_by_label(self.kernel_id(), label) {
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
                                let mount = p
                                    .mount_point
                                    .as_deref()
                                    .map(|m| format!(" → {m}"))
                                    .unwrap_or_default();
                                lines.push(format!("  {}{}", p.path, mount));
                            }
                        }
                    }
                    Err(e) => {
                        lines.push(format!("(paths error: {e})"));
                    }
                }

                KjResult::Ok(lines.join("\n"))
            }
            Ok(None) => KjResult::Err(format!("kj workspace show: '{}' not found", label)),
            Err(e) => KjResult::Err(format!("kj workspace show: {e}")),
        }
    }

    /// `kj workspace create <label> [--desc text] [--path p ...]`
    fn workspace_create(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let label = match argv.get(1) {
            Some(l) => l.as_str(),
            None => return KjResult::Err("kj workspace create: requires a label".to_string()),
        };

        let desc = extract_named_arg(argv, &["--desc", "--description"]);
        let paths = extract_all_named_args(argv, &["--path"]);

        let db = self.kernel_db().lock().unwrap();
        let kernel_id = self.kernel_id();
        let ws_id = WorkspaceId::new();

        let row = WorkspaceRow {
            workspace_id: ws_id,
            kernel_id,
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
        for path in &paths {
            let path_row = WorkspacePathRow {
                workspace_id: ws_id,
                path: path.clone(),
                mount_point: None,
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
        KjResult::Ok(format!("created workspace '{}'{}", label, path_msg))
    }

    /// `kj workspace add <label> <path> [--mount m] [--read-only]`
    fn workspace_add(&self, argv: &[String]) -> KjResult {
        let label = match argv.get(1) {
            Some(l) => l.as_str(),
            None => return KjResult::Err("kj workspace add: requires a workspace label".to_string()),
        };
        let path = match argv.get(2) {
            Some(p) => p.as_str(),
            None => return KjResult::Err("kj workspace add: requires a path".to_string()),
        };

        let mount_point = extract_named_arg(argv, &["--mount", "-m"]);
        // --read-only is captured but not stored yet (workspace_paths doesn't have a column)
        let _read_only = has_flag(argv, &["--read-only"]);

        let db = self.kernel_db().lock().unwrap();
        let kernel_id = self.kernel_id();

        let ws = match db.get_workspace_by_label(kernel_id, label) {
            Ok(Some(w)) => w,
            Ok(None) => return KjResult::Err(format!("kj workspace add: workspace '{}' not found", label)),
            Err(e) => return KjResult::Err(format!("kj workspace add: {e}")),
        };

        let path_row = WorkspacePathRow {
            workspace_id: ws.workspace_id,
            path: path.to_string(),
            mount_point,
            created_at: kaijutsu_types::now_millis() as i64,
        };
        match db.insert_workspace_path(&path_row) {
            Ok(()) => KjResult::Ok(format!("added path '{}' to workspace '{}'", path, label)),
            Err(e) => KjResult::Err(format!("kj workspace add: {e}")),
        }
    }

    /// `kj workspace bind <label> [ctx]`
    fn workspace_bind(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let label = match argv.get(1) {
            Some(l) => l.as_str(),
            None => return KjResult::Err("kj workspace bind: requires a workspace label".to_string()),
        };

        let db = self.kernel_db().lock().unwrap();
        let kernel_id = self.kernel_id();

        let ctx_arg = argv.get(2).map(|s| s.as_str());
        let target_id = match super::refs::resolve_context_arg(ctx_arg, caller, &db, kernel_id) {
            Ok(id) => id,
            Err(e) => return KjResult::Err(format!("kj workspace bind: {e}")),
        };

        let ws = match db.get_workspace_by_label(kernel_id, label) {
            Ok(Some(w)) => w,
            Ok(None) => return KjResult::Err(format!("kj workspace bind: workspace '{}' not found", label)),
            Err(e) => return KjResult::Err(format!("kj workspace bind: {e}")),
        };

        match db.update_workspace(target_id, Some(ws.workspace_id)) {
            Ok(()) => KjResult::Ok(format!("bound workspace '{}' to context {}", label, target_id.short())),
            Err(e) => KjResult::Err(format!("kj workspace bind: {e}")),
        }
    }

    /// `kj workspace remove <label>` — archive a workspace (latched).
    fn workspace_remove(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let label = match argv.get(1) {
            Some(l) => l.as_str(),
            None => return KjResult::Err("kj workspace remove: requires a label".to_string()),
        };

        let db = self.kernel_db().lock().unwrap();
        let kernel_id = self.kernel_id();

        let ws = match db.get_workspace_by_label(kernel_id, label) {
            Ok(Some(w)) => w,
            Ok(None) => return KjResult::Err(format!("kj workspace remove: '{}' not found", label)),
            Err(e) => return KjResult::Err(format!("kj workspace remove: {e}")),
        };

        if !caller.confirmed {
            let usage_count = db.contexts_using_workspace(kernel_id, ws.workspace_id).unwrap_or(0);
            return KjResult::Latch {
                command: "kj workspace remove".to_string(),
                target: label.to_string(),
                message: format!("{} context(s) using this workspace", usage_count),
            };
        }

        match db.archive_workspace(ws.workspace_id) {
            Ok(true) => KjResult::Ok(format!("archived workspace '{}'", label)),
            Ok(false) => KjResult::Err(format!("kj workspace remove: '{}' already archived", label)),
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
        assert_eq!(result.message(), "(no workspaces)");
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
        let ctx = register_context(&d, Some("ctx"), None, principal).await;
        let c = caller_with_context(ctx);

        let result = d
            .dispatch(&[s("workspace"), s("create"), s("myws"), s("--desc"), s("Test workspace")], &c)
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());
        assert!(result.message().contains("myws"));

        let result = d.dispatch(&[s("workspace"), s("list")], &c).await;
        assert!(result.is_ok());
        assert!(result.message().contains("myws"), "msg: {}", result.message());
    }

    #[tokio::test]
    async fn workspace_create_with_paths() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("ctx"), None, principal).await;
        let c = caller_with_context(ctx);

        let result = d
            .dispatch(&[s("workspace"), s("create"), s("ws2"), s("--path"), s("/src"), s("--path"), s("/docs")], &c)
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());
        assert!(result.message().contains("2 paths"), "msg: {}", result.message());

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
        let ctx = register_context(&d, Some("ctx"), None, principal).await;
        let c = caller_with_context(ctx);

        d.dispatch(&[s("workspace"), s("create"), s("ws3")], &c).await;

        let result = d
            .dispatch(&[s("workspace"), s("add"), s("ws3"), s("/extra"), s("--mount"), s("/mnt/extra")], &c)
            .await;
        assert!(result.is_ok(), "add failed: {}", result.message());
    }

    #[tokio::test]
    async fn workspace_bind_to_context() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("ctx"), None, principal).await;
        let c = caller_with_context(ctx);

        d.dispatch(&[s("workspace"), s("create"), s("ws4")], &c).await;

        let result = d
            .dispatch(&[s("workspace"), s("bind"), s("ws4")], &c)
            .await;
        assert!(result.is_ok(), "bind failed: {}", result.message());
        assert!(result.message().contains("bound"), "msg: {}", result.message());
    }

    #[tokio::test]
    async fn workspace_remove_requires_latch() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("ctx"), None, principal).await;
        let c = caller_with_context(ctx);

        d.dispatch(&[s("workspace"), s("create"), s("ws5")], &c).await;

        let result = d.dispatch(&[s("workspace"), s("remove"), s("ws5")], &c).await;
        assert!(result.is_latch(), "expected latch, got: {:?}", result);
    }

    #[tokio::test]
    async fn workspace_remove_confirmed() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("ctx"), None, principal).await;

        let c = caller_with_context(ctx);
        d.dispatch(&[s("workspace"), s("create"), s("ws6")], &c).await;

        let c = confirmed_caller(ctx);
        let result = d.dispatch(&[s("workspace"), s("remove"), s("ws6")], &c).await;
        assert!(result.is_ok(), "remove failed: {}", result.message());
        assert!(result.message().contains("archived"), "msg: {}", result.message());
    }
}
