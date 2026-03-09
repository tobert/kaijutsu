//! Drift subcommands: push, flush, queue, cancel.

use kaijutsu_crdt::DriftKind;

use super::format::format_drift_queue;
use super::{KjCaller, KjDispatcher, KjResult};

impl KjDispatcher {
    pub(crate) async fn dispatch_drift(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return KjResult::Err(self.drift_help());
        }

        match argv[0].as_str() {
            "push" => self.drift_push(argv, caller).await,
            "flush" => self.drift_flush(caller).await,
            "queue" | "q" => self.drift_queue().await,
            "cancel" => self.drift_cancel(argv).await,
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
    flush                   Deliver all staged drifts
    queue                   Show staging queue
    cancel <id>             Remove a staged drift by ID"
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
        let d = test_dispatcher();
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
        let d = test_dispatcher();
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
        let d = test_dispatcher();
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("lonely"), None, principal).await;

        let c = caller_with_context(ctx);
        let result = d.dispatch(&[s("drift"), s("flush")], &c).await;
        assert!(result.is_ok());
        assert!(result.message().contains("nothing to flush"));
    }

    #[tokio::test]
    async fn drift_flush_delivers() {
        let d = test_dispatcher();
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
    async fn drift_push_missing_content() {
        let d = test_dispatcher();
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("x"), None, principal).await;
        let _dst = register_context(&d, Some("y"), None, principal).await;

        let c = caller_with_context(ctx);
        let result = d.dispatch(&[s("drift"), s("push"), s("y")], &c).await;
        assert!(!result.is_ok());
        assert!(result.message().contains("requires content"));
    }
}
