//! Workspace subcommands: list, show (read-only).

use super::{KjCaller, KjDispatcher, KjResult};

impl KjDispatcher {
    pub(crate) fn dispatch_workspace(&self, argv: &[String], _caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return KjResult::Err(self.workspace_help());
        }

        match argv[0].as_str() {
            "list" | "ls" => self.workspace_list(),
            "show" => self.workspace_show(argv),
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
    list            List active workspaces
    show <label>    Show workspace details and paths"
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
}

#[cfg(test)]
mod tests {
    use crate::kj::test_helpers::*;

    fn s(v: &str) -> String {
        v.to_string()
    }

    #[tokio::test]
    async fn workspace_list_empty() {
        let d = test_dispatcher();
        let c = test_caller();
        let result = d.dispatch(&[s("workspace"), s("list")], &c).await;
        assert!(result.is_ok());
        assert_eq!(result.message(), "(no workspaces)");
    }

    #[tokio::test]
    async fn workspace_show_not_found() {
        let d = test_dispatcher();
        let c = test_caller();
        let result = d
            .dispatch(&[s("workspace"), s("show"), s("nonexistent")], &c)
            .await;
        assert!(!result.is_ok());
        assert!(result.message().contains("not found"));
    }

    #[tokio::test]
    async fn workspace_alias() {
        let d = test_dispatcher();
        let c = test_caller();
        let result = d.dispatch(&[s("ws"), s("list")], &c).await;
        assert!(result.is_ok());
    }
}
