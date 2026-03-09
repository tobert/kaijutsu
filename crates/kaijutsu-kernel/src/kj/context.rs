//! Context subcommands: list, info, switch, create.

use kaijutsu_types::{ConsentMode, ContextId, EdgeKind};

use crate::kernel_db::{ContextEdgeRow, ContextRow};

use super::format::{format_context_info, format_context_table, format_context_tree};
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
            "help" | "--help" | "-h" => KjResult::Ok(self.context_help()),
            other => KjResult::Err(format!(
                "kj context: unknown subcommand '{}'\n\n{}",
                other,
                self.context_help()
            )),
        }
    }

    fn context_help(&self) -> String {
        "\
kj context — context management

USAGE:
    kj context <subcommand> [args...]

SUBCOMMANDS:
    list [--tree]           List contexts (flat or tree view)
    info [<ctx>]            Show context details (default: current)
    switch <ctx>            Switch to a different context
    create <label> [--parent <ctx>]  Create a new context"
            .to_string()
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
        KjResult::Ok(format_context_info(
            &row,
            children_count,
            drift_from + drift_to,
            is_current,
        ))
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
}

#[cfg(test)]
mod tests {
    use crate::kernel_db::ContextEdgeRow;
    use crate::kj::test_helpers::*;
    use crate::kj::KjResult;
    use kaijutsu_types::{EdgeKind, PrincipalId};

    fn s(v: &str) -> String {
        v.to_string()
    }

    #[tokio::test]
    async fn context_list_empty() {
        let d = test_dispatcher();
        let c = test_caller();
        let result = d.dispatch(&[s("context"), s("list")], &c).await;
        assert!(result.is_ok());
        assert_eq!(result.message(), "(no contexts)");
    }

    #[tokio::test]
    async fn context_list_shows_contexts() {
        let d = test_dispatcher();
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
        let d = test_dispatcher();
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
        let d = test_dispatcher();
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
        let d = test_dispatcher();
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
        let d = test_dispatcher();
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
        let d = test_dispatcher();
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
        let d = test_dispatcher();
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
        let d = test_dispatcher();
        let c = test_caller();
        let result = d.dispatch(&[s("context"), s("help")], &c).await;
        assert!(result.is_ok());
        assert!(result.message().contains("SUBCOMMANDS"));
    }
}
