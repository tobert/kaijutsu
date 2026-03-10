//! Drift subcommands: push, flush, queue, cancel.

use kaijutsu_crdt::DriftKind;
use kaijutsu_types::EdgeKind;

use super::format::format_drift_queue;
use super::refs;
use super::{KjCaller, KjDispatcher, KjResult};

impl KjDispatcher {
    pub(crate) async fn dispatch_drift(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return KjResult::Err(self.drift_help());
        }

        match argv[0].as_str() {
            "push" => self.drift_push(argv, caller).await,
            "pull" => self.drift_pull(argv, caller).await,
            "merge" => self.drift_merge(argv, caller).await,
            "flush" => self.drift_flush(caller).await,
            "queue" | "q" => self.drift_queue().await,
            "cancel" => self.drift_cancel(argv).await,
            "history" => self.drift_history(argv, caller),
            "help" | "--help" | "-h" => KjResult::Ok(self.drift_help()),
            other => KjResult::Err(format!(
                "kj drift: unknown subcommand '{}'\n\n{}",
                other,
                self.drift_help()
            )),
        }
    }

    fn drift_help(&self) -> String {
        "\
kj drift — cross-context communication

USAGE:
    kj drift <subcommand> [args...]

SUBCOMMANDS:
    push <dst> [content]    Stage content for target context
    pull <src> [prompt]     Pull + distill from source context via LLM
    merge [ctx]             Summarize this fork back into parent via LLM
    flush                   Deliver all staged drifts
    queue                   Show staging queue
    cancel <id>             Remove a staged drift by ID
    history [ctx]           Show drift history for a context"
            .to_string()
    }

    async fn drift_push(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        // kj drift push <dst> [content...]
        let dst_query = match argv.get(1) {
            Some(q) => q.as_str(),
            None => {
                return KjResult::Err(
                    "kj drift push: requires a destination context reference".to_string(),
                )
            }
        };

        // Resolve destination
        let target_id = {
            let router = self.drift_router().read().await;
            match router.resolve_context(dst_query) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj drift push: {e}")),
            }
        };

        // Content is everything after the destination
        let content = if argv.len() > 2 {
            argv[2..].join(" ")
        } else {
            return KjResult::Err("kj drift push: requires content".to_string());
        };

        // Get source model for provenance
        let source_model = {
            let router = self.drift_router().read().await;
            router.get(caller.context_id).and_then(|h| h.model.clone())
        };

        // Stage the drift
        let staged_id = {
            let mut router = self.drift_router().write().await;
            match router.stage(
                caller.context_id,
                target_id,
                content,
                source_model,
                DriftKind::Push,
            ) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj drift push: {e}")),
            }
        };

        KjResult::Ok(format!("staged drift #{} → {}", staged_id, dst_query))
    }

    async fn drift_pull(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        // kj drift pull <src> [prompt...]
        let src_query = match argv.get(1) {
            Some(q) => q.as_str(),
            None => return KjResult::Err("kj drift pull: requires a source context reference".to_string()),
        };

        // Resolve source context
        let source_id = {
            let db = self.kernel_db().lock().unwrap();
            match refs::resolve_context_arg(Some(src_query), caller, &db, self.kernel_id()) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj drift pull: {e}")),
            }
        };

        if source_id == caller.context_id {
            return KjResult::Err("kj drift pull: cannot pull from self".to_string());
        }

        // Directed prompt is everything after the source ref
        let directed_prompt = if argv.len() > 2 {
            Some(argv[2..].join(" "))
        } else {
            None
        };

        // Summarize source via LLM
        let summary = match self.summarize(source_id, directed_prompt.as_deref()).await {
            Ok(s) => s,
            Err(e) => return KjResult::Err(format!("kj drift pull: {e}")),
        };

        // Insert drift block in caller's context
        let source_model = {
            let router = self.drift_router().read().await;
            router.get(source_id).and_then(|h| h.model.clone())
        };
        let after = self.block_store().last_block_id(caller.context_id);
        if let Err(e) = self.block_store().insert_drift_block(
            caller.context_id,
            None,
            after.as_ref(),
            &summary,
            source_id,
            source_model,
            DriftKind::Pull,
        ) {
            return KjResult::Err(format!("kj drift pull: failed to insert drift block: {e}"));
        }

        // Record drift edge
        {
            let db = self.kernel_db().lock().unwrap();
            let edge = crate::kernel_db::ContextEdgeRow {
                edge_id: uuid::Uuid::now_v7(),
                source_id,
                target_id: caller.context_id,
                kind: EdgeKind::Drift,
                metadata: Some("pull".to_string()),
                created_at: kaijutsu_types::now_millis() as i64,
            };
            if let Err(e) = db.insert_edge(&edge) {
                tracing::warn!("failed to insert pull drift edge: {e}");
            }
        }

        // Preview: first ~200 chars
        let preview = if summary.len() > 200 {
            let mut end = 200;
            while end > 0 && !summary.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}...", &summary[..end])
        } else {
            summary
        };

        KjResult::Ok(format!("pulled from {}:\n{}", src_query, preview))
    }

    async fn drift_merge(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        // kj drift merge [ctx]
        // Default target = caller's forked_from parent
        let target_id = if let Some(target_query) = argv.get(1) {
            let db = self.kernel_db().lock().unwrap();
            match refs::resolve_context_arg(Some(target_query.as_str()), caller, &db, self.kernel_id()) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj drift merge: {e}")),
            }
        } else {
            // Default: forked_from parent
            let db = self.kernel_db().lock().unwrap();
            let row = match db.get_context(caller.context_id) {
                Ok(Some(r)) => r,
                Ok(None) => return KjResult::Err("kj drift merge: current context not found in db".to_string()),
                Err(e) => return KjResult::Err(format!("kj drift merge: {e}")),
            };
            match row.forked_from {
                Some(parent) => parent,
                None => return KjResult::Err("kj drift merge: not a fork (no parent context); use 'kj drift merge <ctx>' to specify a target".to_string()),
            }
        };

        if target_id == caller.context_id {
            return KjResult::Err("kj drift merge: cannot merge into self".to_string());
        }

        // Summarize caller's context
        let summary = match self.summarize(caller.context_id, None).await {
            Ok(s) => s,
            Err(e) => return KjResult::Err(format!("kj drift merge: {e}")),
        };

        // Insert drift block into the TARGET (parent) context
        let source_model = {
            let router = self.drift_router().read().await;
            router.get(caller.context_id).and_then(|h| h.model.clone())
        };
        let after = self.block_store().last_block_id(target_id);
        if let Err(e) = self.block_store().insert_drift_block(
            target_id,
            None,
            after.as_ref(),
            &summary,
            caller.context_id,
            source_model,
            DriftKind::Merge,
        ) {
            return KjResult::Err(format!("kj drift merge: failed to insert drift block: {e}"));
        }

        // Record drift edge
        {
            let db = self.kernel_db().lock().unwrap();
            let edge = crate::kernel_db::ContextEdgeRow {
                edge_id: uuid::Uuid::now_v7(),
                source_id: caller.context_id,
                target_id,
                kind: EdgeKind::Drift,
                metadata: Some("merge".to_string()),
                created_at: kaijutsu_types::now_millis() as i64,
            };
            if let Err(e) = db.insert_edge(&edge) {
                tracing::warn!("failed to insert merge drift edge: {e}");
            }
        }

        // Preview: first ~200 chars
        let target_label = {
            let db = self.kernel_db().lock().unwrap();
            db.get_context(target_id)
                .ok()
                .flatten()
                .and_then(|r| r.label)
                .unwrap_or_else(|| target_id.short())
        };

        let preview = if summary.len() > 200 {
            let mut end = 200;
            while end > 0 && !summary.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}...", &summary[..end])
        } else {
            summary
        };

        KjResult::Ok(format!("merged into '{}':\n{}", target_label, preview))
    }

    async fn drift_flush(&self, caller: &KjCaller) -> KjResult {
        let staged = {
            let mut router = self.drift_router().write().await;
            router.drain(Some(caller.context_id))
        };

        if staged.is_empty() {
            return KjResult::Ok("nothing to flush".to_string());
        }

        let count = staged.len();
        let mut injected = 0;
        let mut failed = Vec::new();

        for drift in staged {
            let after = self.block_store().last_block_id(drift.target_ctx);
            match self.block_store().insert_drift_block(
                drift.target_ctx,
                None,
                after.as_ref(),
                drift.content.clone(),
                drift.source_ctx,
                drift.source_model.clone(),
                drift.drift_kind,
            ) {
                Ok(_) => injected += 1,
                Err(e) => {
                    tracing::warn!(
                        "drift flush failed: {} → {}: {e}",
                        drift.source_ctx.short(),
                        drift.target_ctx.short()
                    );
                    failed.push(drift);
                }
            }
        }

        // Requeue failures
        if !failed.is_empty() {
            let fail_count = failed.len();
            let mut router = self.drift_router().write().await;
            router.requeue(failed);
            KjResult::Ok(format!(
                "flushed {injected}/{count} drifts ({fail_count} requeued)"
            ))
        } else {
            KjResult::Ok(format!("flushed {injected} drift(s)"))
        }
    }

    async fn drift_queue(&self) -> KjResult {
        let router = self.drift_router().read().await;
        let queue = router.queue();
        KjResult::Ok(format_drift_queue(queue))
    }

    /// `kj drift history [ctx]` — show drift history (edges) for a context.
    fn drift_history(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let db = self.kernel_db().lock().unwrap();
        let kernel_id = self.kernel_id();

        let target_arg = argv.get(1).map(|s| s.as_str());
        let target_id = match super::refs::resolve_context_arg(target_arg, caller, &db, kernel_id) {
            Ok(id) => id,
            Err(e) => return KjResult::Err(format!("kj drift history: {e}")),
        };

        let outgoing = db.drift_provenance(target_id).unwrap_or_default();
        let incoming = db.edges_to(target_id, Some(kaijutsu_types::EdgeKind::Drift)).unwrap_or_default();

        KjResult::Ok(super::format::format_drift_history(&outgoing, &incoming, &db))
    }

    async fn drift_cancel(&self, argv: &[String]) -> KjResult {
        let id_str = match argv.get(1) {
            Some(s) => s,
            None => return KjResult::Err("kj drift cancel: requires a staged drift ID".to_string()),
        };

        let id: u64 = match id_str.parse() {
            Ok(n) => n,
            Err(_) => {
                return KjResult::Err(format!(
                    "kj drift cancel: '{}' is not a valid drift ID",
                    id_str
                ))
            }
        };

        let mut router = self.drift_router().write().await;
        if router.cancel(id) {
            KjResult::Ok(format!("cancelled drift #{}", id))
        } else {
            KjResult::Err(format!("kj drift cancel: drift #{} not found in queue", id))
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
    async fn drift_push_and_queue() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let src = register_context(&d, Some("src"), None, principal).await;
        let _dst = register_context(&d, Some("dst"), None, principal).await;

        let c = caller_with_context(src);
        let result = d
            .dispatch(&[s("drift"), s("push"), s("dst"), s("hello from src")], &c)
            .await;
        assert!(result.is_ok(), "push failed: {}", result.message());
        assert!(result.message().contains("staged drift #1"));

        // Check queue
        let result = d.dispatch(&[s("drift"), s("queue")], &c).await;
        assert!(result.is_ok());
        let msg = result.message();
        assert!(msg.contains("hello from src"), "queue: {msg}");
    }

    #[tokio::test]
    async fn drift_cancel() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let src = register_context(&d, Some("a"), None, principal).await;
        let _dst = register_context(&d, Some("b"), None, principal).await;

        let c = caller_with_context(src);
        d.dispatch(&[s("drift"), s("push"), s("b"), s("content")], &c)
            .await;

        let result = d.dispatch(&[s("drift"), s("cancel"), s("1")], &c).await;
        assert!(result.is_ok(), "cancel: {}", result.message());
        assert!(result.message().contains("cancelled"));

        // Queue should be empty
        let result = d.dispatch(&[s("drift"), s("queue")], &c).await;
        assert_eq!(result.message(), "(queue empty)");
    }

    #[tokio::test]
    async fn drift_flush_empty() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("lonely"), None, principal).await;

        let c = caller_with_context(ctx);
        let result = d.dispatch(&[s("drift"), s("flush")], &c).await;
        assert!(result.is_ok());
        assert!(result.message().contains("nothing to flush"));
    }

    #[tokio::test]
    async fn drift_flush_delivers() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let src = register_context(&d, Some("sender"), None, principal).await;
        let dst = register_context(&d, Some("receiver"), None, principal).await;

        // Create target document so flush can insert
        d.block_store()
            .create_document(dst, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(src);
        d.dispatch(
            &[s("drift"), s("push"), s("receiver"), s("important finding")],
            &c,
        )
        .await;

        let result = d.dispatch(&[s("drift"), s("flush")], &c).await;
        assert!(result.is_ok(), "flush: {}", result.message());
        assert!(result.message().contains("flushed 1 drift"));
    }

    #[tokio::test]
    async fn drift_history_empty() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("ctx"), None, principal).await;

        let c = caller_with_context(ctx);
        let result = d.dispatch(&[s("drift"), s("history")], &c).await;
        assert!(result.is_ok());
        assert!(result.message().contains("no drift history"), "msg: {}", result.message());
    }

    #[tokio::test]
    async fn drift_pull_missing_source() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d.dispatch(&[s("drift"), s("pull"), s("nonexistent")], &c).await;
        assert!(!result.is_ok());
        // Should fail on context resolution, not "not yet implemented"
        assert!(!result.message().contains("not yet implemented"), "msg: {}", result.message());
    }

    #[tokio::test]
    async fn drift_pull_requires_source_arg() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d.dispatch(&[s("drift"), s("pull")], &c).await;
        assert!(!result.is_ok());
        assert!(result.message().contains("requires a source"), "msg: {}", result.message());
    }

    #[tokio::test]
    async fn drift_pull_no_blocks_error() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let src = register_context(&d, Some("src-ctx"), None, principal).await;
        let dst = register_context(&d, Some("dst-ctx"), None, principal).await;

        // Create empty documents
        d.block_store()
            .create_document(src, crate::DocumentKind::Conversation, None)
            .unwrap();
        d.block_store()
            .create_document(dst, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(dst);
        let result = d.dispatch(&[s("drift"), s("pull"), s("src-ctx")], &c).await;
        assert!(!result.is_ok());
        assert!(result.message().contains("no blocks"), "msg: {}", result.message());
    }

    #[tokio::test]
    async fn drift_pull_cannot_pull_from_self() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("self-ctx"), None, principal).await;

        let c = caller_with_context(ctx);
        let result = d.dispatch(&[s("drift"), s("pull"), s("self-ctx")], &c).await;
        assert!(!result.is_ok());
        assert!(result.message().contains("cannot pull from self"), "msg: {}", result.message());
    }

    #[tokio::test]
    async fn drift_merge_no_parent() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("orphan"), None, principal).await;

        let c = caller_with_context(ctx);
        let result = d.dispatch(&[s("drift"), s("merge")], &c).await;
        assert!(!result.is_ok());
        assert!(result.message().contains("not a fork"), "msg: {}", result.message());
    }

    #[tokio::test]
    async fn drift_merge_no_blocks_error() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal).await;
        let child = register_context(&d, Some("child"), Some(parent), principal).await;

        // Create empty documents
        d.block_store()
            .create_document(parent, crate::DocumentKind::Conversation, None)
            .unwrap();
        d.block_store()
            .create_document(child, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(child);
        let result = d.dispatch(&[s("drift"), s("merge")], &c).await;
        assert!(!result.is_ok());
        assert!(result.message().contains("no blocks"), "msg: {}", result.message());
    }

    #[tokio::test]
    async fn drift_push_missing_content() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("x"), None, principal).await;
        let _dst = register_context(&d, Some("y"), None, principal).await;

        let c = caller_with_context(ctx);
        let result = d.dispatch(&[s("drift"), s("push"), s("y")], &c).await;
        assert!(!result.is_ok());
        assert!(result.message().contains("requires content"));
    }
}
