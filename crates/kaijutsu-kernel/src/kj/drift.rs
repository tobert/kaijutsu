//! Drift subcommands: push, flush, queue, cancel.

use kaijutsu_crdt::DriftKind;
use kaijutsu_types::{ContentType, EdgeKind};

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
            "edge" => self.drift_edge(&argv[1..]),
            "help" | "--help" | "-h" => KjResult::ok_ephemeral(self.drift_help(), ContentType::Markdown),
            other => KjResult::Err(format!(
                "kj drift: unknown subcommand '{}'\n\n{}",
                other,
                self.drift_help()
            )),
        }
    }

    fn drift_help(&self) -> String {
        include_str!("../../docs/help/kj-drift.md").to_string()
    }

    async fn drift_push(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        use super::parse::has_flag;

        // kj drift push <dst> [--summarize|-s] [content...]
        let summarize = has_flag(argv, &["--summarize", "-s"]);

        let dst_query = match argv.get(1) {
            Some(q) if !q.starts_with('-') => q.as_str(),
            _ => {
                return KjResult::Err(
                    "kj drift push: requires a destination context reference".to_string(),
                );
            }
        };

        // Resolve destination
        let target_id = {
            let router = self.drift_router().read();
            match router.resolve_context(dst_query) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj drift push: {e}")),
            }
        };

        let context_id = match caller.require_context() {
            Ok(id) => id,
            Err(e) => return e,
        };

        // Determine content and drift kind
        let (content, drift_kind) = if summarize {
            // LLM-distill the caller's context
            match self.summarize(context_id, None).await {
                Ok(s) => (s, DriftKind::Distill),
                Err(e) => return KjResult::Err(format!("kj drift push --summarize: {e}")),
            }
        } else {
            // Content is everything after the destination, excluding flags
            let content_args: Vec<&str> = argv[2..]
                .iter()
                .filter(|a| *a != "--summarize" && *a != "-s")
                .map(|s| s.as_str())
                .collect();
            if content_args.is_empty() {
                return KjResult::Err(
                    "kj drift push: requires content (or use --summarize)".to_string(),
                );
            }
            (content_args.join(" "), DriftKind::Push)
        };

        // Get source model for provenance
        let source_model = {
            let router = self.drift_router().read();
            router.get(context_id).and_then(|h| h.model.clone())
        };

        // Stage the drift
        let staged_id = {
            let mut router = self.drift_router().write();
            match router.stage(
                context_id,
                target_id,
                content,
                source_model,
                drift_kind,
            ) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj drift push: {e}")),
            }
        };

        KjResult::ok(format!("staged drift #{} → {}", staged_id, dst_query))
    }

    async fn drift_pull(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        // kj drift pull <src> [prompt...]
        let src_query = match argv.get(1) {
            Some(q) => q.as_str(),
            None => {
                return KjResult::Err(
                    "kj drift pull: requires a source context reference".to_string(),
                );
            }
        };

        // Resolve source context
        let source_id = {
            let db = self.kernel_db().lock();
            match refs::resolve_context_arg(Some(src_query), caller, &db, self.kernel_id()) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj drift pull: {e}")),
            }
        };

        let context_id = match caller.require_context() {
            Ok(id) => id,
            Err(e) => return e,
        };

        if source_id == context_id {
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
            let router = self.drift_router().read();
            router.get(source_id).and_then(|h| h.model.clone())
        };
        let after = self.block_store().last_block_id(context_id);

        if let Err(e) = self.block_store().insert_drift_block(
            context_id,
            None,
            after.as_ref(),
            &summary,
            source_id,
            source_model.clone(),
            DriftKind::Pull,
        ) {
            return KjResult::Err(format!("kj drift pull: failed to insert drift block: {e}"));
        }

        // Record drift edge
        {
            let db = self.kernel_db().lock();
            let edge = crate::kernel_db::ContextEdgeRow {
                edge_id: uuid::Uuid::now_v7(),
                source_id,
                target_id: context_id,
                kind: EdgeKind::Drift,
                metadata: Some("pull".to_string()),
                created_at: kaijutsu_types::now_millis() as i64,
            };
            if let Err(e) = db.insert_edge(&edge) {
                tracing::warn!("failed to insert pull drift edge: {e}");
            }
        }

        if let Err(e) = self
            .run_rc_lifecycle(
                super::lifecycle::VERB_DRIFT,
                context_id,
                None,
                None,
                Some(super::lifecycle::DriftInfo {
                    kind: DriftKind::Pull,
                    source_ctx: source_id,
                    target_ctx: context_id,
                    source_model,
                }),
                caller,
            )
            .await
        {
            tracing::warn!("rc drift lifecycle (pull): {e}");
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

        KjResult::ok(format!("pulled from {}:\n{}", src_query, preview))
    }

    async fn drift_merge(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let context_id = match caller.require_context() {
            Ok(id) => id,
            Err(e) => return e,
        };

        // kj drift merge [ctx]
        // Default target = caller's forked_from parent
        let target_id = if let Some(target_query) = argv.get(1) {
            let db = self.kernel_db().lock();
            match refs::resolve_context_arg(
                Some(target_query.as_str()),
                caller,
                &db,
                self.kernel_id(),
            ) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj drift merge: {e}")),
            }
        } else {
            // Default: forked_from parent
            let db = self.kernel_db().lock();
            let row = match db.get_context(context_id) {
                Ok(Some(r)) => r,
                Ok(None) => {
                    return KjResult::Err(
                        "kj drift merge: current context not found in db".to_string(),
                    );
                }
                Err(e) => return KjResult::Err(format!("kj drift merge: {e}")),
            };
            match row.forked_from {
                Some(parent) => parent,
                None => return KjResult::Err("kj drift merge: not a fork (no parent context); use 'kj drift merge <ctx>' to specify a target".to_string()),
            }
        };

        if target_id == context_id {
            return KjResult::Err("kj drift merge: cannot merge into self".to_string());
        }

        // Summarize caller's context
        let summary = match self.summarize(context_id, None).await {
            Ok(s) => s,
            Err(e) => return KjResult::Err(format!("kj drift merge: {e}")),
        };

        // Insert drift block into the TARGET (parent) context
        let source_model = {
            let router = self.drift_router().read();
            router.get(context_id).and_then(|h| h.model.clone())
        };
        let after = self.block_store().last_block_id(target_id);
        if let Err(e) = self.block_store().insert_drift_block(
            target_id,
            None,
            after.as_ref(),
            &summary,
            context_id,
            source_model.clone(),
            DriftKind::Merge,
        ) {
            return KjResult::Err(format!("kj drift merge: failed to insert drift block: {e}"));
        }

        // Record drift edge
        {
            let db = self.kernel_db().lock();
            let edge = crate::kernel_db::ContextEdgeRow {
                edge_id: uuid::Uuid::now_v7(),
                source_id: context_id,
                target_id,
                kind: EdgeKind::Drift,
                metadata: Some("merge".to_string()),
                created_at: kaijutsu_types::now_millis() as i64,
            };
            if let Err(e) = db.insert_edge(&edge) {
                tracing::warn!("failed to insert merge drift edge: {e}");
            }
        }

        if let Err(e) = self
            .run_rc_lifecycle(
                super::lifecycle::VERB_DRIFT,
                target_id,
                None,
                None,
                Some(super::lifecycle::DriftInfo {
                    kind: DriftKind::Merge,
                    source_ctx: context_id,
                    target_ctx: target_id,
                    source_model,
                }),
                caller,
            )
            .await
        {
            tracing::warn!("rc drift lifecycle (merge): {e}");
        }

        // Preview: first ~200 chars
        let target_label = {
            let db = self.kernel_db().lock();
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

        KjResult::ok(format!("merged into '{}':\n{}", target_label, preview))
    }

    async fn drift_flush(&self, caller: &KjCaller) -> KjResult {
        let staged = {
            let mut router = self.drift_router().write();
            router.drain(caller.context_id)
        };

        if staged.is_empty() {
            return KjResult::ok("nothing to flush".to_string());
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
                Ok(_) => {
                    injected += 1;
                    if let Err(e) = self
                        .run_rc_lifecycle(
                            super::lifecycle::VERB_DRIFT,
                            drift.target_ctx,
                            None,
                            None,
                            Some(super::lifecycle::DriftInfo {
                                kind: drift.drift_kind,
                                source_ctx: drift.source_ctx,
                                target_ctx: drift.target_ctx,
                                source_model: drift.source_model.clone(),
                            }),
                            caller,
                        )
                        .await
                    {
                        tracing::warn!(
                            "rc drift lifecycle (flush) {} → {}: {e}",
                            drift.source_ctx.short(),
                            drift.target_ctx.short()
                        );
                    }
                }
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

        // Requeue failures and drain dead letters
        let fail_count = failed.len();
        if !failed.is_empty() {
            let mut router = self.drift_router().write();
            router.requeue(failed);
        }

        // Drain dead letters (items that exceeded MAX_DRIFT_RETRIES) into lost+found
        let dead = {
            let mut router = self.drift_router().write();
            router.drain_dead_letter()
        };
        if !dead.is_empty() {
            let (lf_id, _is_new) = {
                let mut router = self.drift_router().write();
                router.ensure_lost_found()
            };
            // create_document is idempotent (DashMap entry-based)
            let _ =
                self.block_store()
                    .create_document(lf_id, crate::DocumentKind::Conversation, None);
            let dead_count = dead.len();
            for item in dead {
                let after = self.block_store().last_block_id(lf_id);
                let content = format!(
                    "[DEAD LETTER] {} → {} (retries: {}, kind: {:?})\n\n{}",
                    item.source_ctx.short(),
                    item.target_ctx.short(),
                    item.retry_count,
                    item.drift_kind,
                    &item.content,
                );
                if let Err(e) = self.block_store().insert_drift_block(
                    lf_id,
                    None,
                    after.as_ref(),
                    content,
                    item.source_ctx,
                    item.source_model,
                    item.drift_kind,
                ) {
                    tracing::error!("failed to write dead letter to lost+found: {e}");
                }
            }
            tracing::warn!(
                count = dead_count,
                context = %lf_id.short(),
                "wrote dead letter drifts to lost+found"
            );
        }

        if fail_count > 0 {
            KjResult::ok(format!(
                "flushed {injected}/{count} drifts ({fail_count} requeued)"
            ))
        } else {
            KjResult::ok(format!("flushed {injected} drift(s)"))
        }
    }

    async fn drift_queue(&self) -> KjResult {
        let router = self.drift_router().read();
        let queue = router.queue();
        let ids = serde_json::Value::Array(
            queue
                .iter()
                .map(|item| serde_json::Value::String(item.id.to_string()))
                .collect(),
        );
        KjResult::ok_with_data(format_drift_queue(queue), ids)
    }

    /// `kj drift history [ctx]` — show drift history (edges) for a context.
    fn drift_history(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let db = self.kernel_db().lock();
        let kernel_id = self.kernel_id();

        let target_arg = argv.get(1).map(|s| s.as_str());
        let target_id = match super::refs::resolve_context_arg(target_arg, caller, &db, kernel_id) {
            Ok(id) => id,
            Err(e) => return KjResult::Err(format!("kj drift history: {e}")),
        };

        let outgoing = db.drift_provenance(target_id).unwrap_or_default();
        let incoming = db
            .edges_to(target_id, Some(kaijutsu_types::EdgeKind::Drift))
            .unwrap_or_default();

        let text = super::format::format_drift_history(&outgoing, &incoming, &db);

        // Iteration handle: full edge_id (UUID) for each drift edge,
        // outgoing first then incoming. Edge IDs are already full strings.
        let ids: Vec<serde_json::Value> = outgoing
            .iter()
            .chain(incoming.iter())
            .map(|e| serde_json::Value::String(e.edge_id.to_string()))
            .collect();

        KjResult::ok_with_data(text, serde_json::Value::Array(ids))
    }

    /// `kj drift edge rm <uuid>` — delete a post-flush drift edge by its
    /// UUID. The UUIDs come from `kj drift history`'s `.data` array, so
    /// `for e in $(kj drift history); do kj drift edge rm $e; done`
    /// round-trips. Distinct from `kj drift cancel` which takes the
    /// in-memory queue id (u64, pre-flush).
    fn drift_edge(&self, argv: &[String]) -> KjResult {
        let sub = argv.first().map(|s| s.as_str()).unwrap_or("");
        match sub {
            "rm" | "remove" => {
                let id_str = match argv.get(1) {
                    Some(s) => s.as_str(),
                    None => {
                        return KjResult::Err(
                            "kj drift edge rm: requires a drift edge UUID (see `kj drift history`)"
                                .to_string(),
                        );
                    }
                };
                let edge_id = match uuid::Uuid::parse_str(id_str) {
                    Ok(u) => u,
                    Err(_) => {
                        return KjResult::Err(format!(
                            "kj drift edge rm: '{id_str}' is not a valid UUID"
                        ));
                    }
                };
                let db = self.kernel_db().lock();
                match db.delete_drift_edge(edge_id) {
                    Ok(true) => KjResult::ok(format!("removed drift edge {edge_id}")),
                    Ok(false) => KjResult::Err(format!(
                        "kj drift edge rm: no drift edge {edge_id} (already removed, \
                         or wrong kind — only drift edges are eligible)"
                    )),
                    Err(e) => KjResult::Err(format!("kj drift edge rm: {e}")),
                }
            }
            // Defer to the canonical `kj drift --help` (rendered from
            // `docs/help/kj-drift.md`) so there's one source of truth.
            "" | "help" | "--help" | "-h" => {
                KjResult::ok_ephemeral(self.drift_help(), ContentType::Markdown)
            }
            other => KjResult::Err(format!(
                "kj drift edge: unknown subcommand '{other}' (try `rm <uuid>`)"
            )),
        }
    }

    async fn drift_cancel(&self, argv: &[String]) -> KjResult {
        let id_str = match argv.get(1) {
            Some(s) => s,
            None => {
                return KjResult::Err("kj drift cancel: requires a staged drift ID".to_string());
            }
        };

        let id: u64 = match id_str.parse() {
            Ok(n) => n,
            Err(_) => {
                return KjResult::Err(format!(
                    "kj drift cancel: '{}' is not a valid drift ID",
                    id_str
                ));
            }
        };

        let mut router = self.drift_router().write();
        if router.cancel(id) {
            KjResult::ok(format!("cancelled drift #{}", id))
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
        let src = register_context(&d, Some("src"), None, principal);
        let _dst = register_context(&d, Some("dst"), None, principal);

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
        let src = register_context(&d, Some("a"), None, principal);
        let _dst = register_context(&d, Some("b"), None, principal);

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
        let ctx = register_context(&d, Some("lonely"), None, principal);

        let c = caller_with_context(ctx);
        let result = d.dispatch(&[s("drift"), s("flush")], &c).await;
        assert!(result.is_ok());
        assert!(result.message().contains("nothing to flush"));
    }

    #[tokio::test]
    async fn drift_flush_delivers() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let src = register_context(&d, Some("sender"), None, principal);
        let dst = register_context(&d, Some("receiver"), None, principal);

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
        let ctx = register_context(&d, Some("ctx"), None, principal);

        let c = caller_with_context(ctx);
        let result = d.dispatch(&[s("drift"), s("history")], &c).await;
        assert!(result.is_ok());
        assert!(
            result.message().contains("no drift history"),
            "msg: {}",
            result.message()
        );
    }

    /// `kj drift edge rm <uuid>` deletes a single drift edge and pairs
    /// with the UUIDs that `kj drift history` emits in `.data`. End-to-end:
    /// insert a drift edge → history surfaces its UUID → edge rm removes
    /// it → history is empty again.
    ///
    /// Note: `drift_flush` injects a Drift *block* into the target context
    /// but does NOT currently write a row to `context_edges`. That's a
    /// pre-existing gap (tracked in `docs/issues.md`); this test seeds the
    /// edge directly to exercise the rm path independently. Once flush is
    /// fixed to also write the edge, the seed becomes optional.
    #[tokio::test]
    async fn drift_edge_rm_round_trip() {
        use crate::kernel_db::ContextEdgeRow;
        use crate::kj::KjResult;
        use kaijutsu_types::EdgeKind;

        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let src = register_context(&d, Some("src"), None, principal);
        let dst = register_context(&d, Some("dst"), None, principal);

        let edge_id = uuid::Uuid::now_v7();
        {
            let db = d.kernel_db().lock();
            db.insert_edge(&ContextEdgeRow {
                edge_id,
                source_id: src,
                target_id: dst,
                kind: EdgeKind::Drift,
                metadata: None,
                created_at: kaijutsu_types::now_millis() as i64,
            })
            .expect("insert drift edge");
        }

        let c = caller_with_context(src);

        // History on the source surfaces the edge_id we just inserted.
        let history = d.dispatch(&[s("drift"), s("history")], &c).await;
        match history {
            KjResult::Ok { data: Some(v), .. } => {
                let ids: Vec<&str> = v
                    .as_array()
                    .expect("array")
                    .iter()
                    .filter_map(|x| x.as_str())
                    .collect();
                assert_eq!(ids.len(), 1, "expected one edge in .data: {ids:?}");
                assert_eq!(ids[0], edge_id.to_string());
            }
            other => panic!("expected Ok with data, got {other:?}"),
        };

        // The handle round-trips into `edge rm`.
        let rm = d
            .dispatch(
                &[s("drift"), s("edge"), s("rm"), edge_id.to_string()],
                &c,
            )
            .await;
        assert!(rm.is_ok(), "edge rm: {}", rm.message());
        assert!(
            rm.message().contains(&edge_id.to_string()),
            "msg should echo the uuid: {}",
            rm.message()
        );

        // History is now empty.
        let again = d.dispatch(&[s("drift"), s("history")], &c).await;
        assert!(again.message().contains("no drift history"));
    }

    /// Removing a non-existent edge UUID returns a friendly error (not
    /// a panic) and doesn't claim success.
    #[tokio::test]
    async fn drift_edge_rm_unknown_uuid_errors() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d
            .dispatch(
                &[
                    s("drift"),
                    s("edge"),
                    s("rm"),
                    s("00000000-0000-0000-0000-000000000000"),
                ],
                &c,
            )
            .await;
        assert!(!result.is_ok(), "must fail on unknown edge");
        assert!(
            result.message().contains("no drift edge"),
            "msg: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn drift_edge_rm_malformed_uuid_errors() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d
            .dispatch(&[s("drift"), s("edge"), s("rm"), s("not-a-uuid")], &c)
            .await;
        assert!(!result.is_ok());
        assert!(
            result.message().contains("valid UUID"),
            "msg: {}",
            result.message()
        );
    }

    /// Structural edges are NOT eligible — `edge rm` only touches drift
    /// edges so it can't break the context DAG. This test inserts a
    /// structural edge directly and confirms `edge rm <its-uuid>`
    /// returns a friendly "not found" rather than silently deleting it.
    #[tokio::test]
    async fn drift_edge_rm_refuses_structural() {
        use crate::kernel_db::ContextEdgeRow;
        use kaijutsu_types::EdgeKind;

        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);
        let child = register_context(&d, Some("child"), Some(parent), principal);

        let structural_id = uuid::Uuid::now_v7();
        {
            let db = d.kernel_db().lock();
            db.insert_edge(&ContextEdgeRow {
                edge_id: structural_id,
                source_id: parent,
                target_id: child,
                kind: EdgeKind::Structural,
                metadata: None,
                created_at: kaijutsu_types::now_millis() as i64,
            })
            .expect("insert structural edge");
        }

        let c = caller_with_context(parent);
        let result = d
            .dispatch(
                &[s("drift"), s("edge"), s("rm"), structural_id.to_string()],
                &c,
            )
            .await;
        assert!(!result.is_ok(), "must refuse structural: {}", result.message());

        // And the structural edge is still there.
        let edges = d
            .kernel_db()
            .lock()
            .edges_from(parent, Some(EdgeKind::Structural))
            .unwrap();
        assert_eq!(edges.len(), 1, "structural edge must survive");
    }

    #[tokio::test]
    async fn drift_pull_missing_source() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d
            .dispatch(&[s("drift"), s("pull"), s("nonexistent")], &c)
            .await;
        assert!(!result.is_ok());
        // Should fail on context resolution, not "not yet implemented"
        assert!(
            !result.message().contains("not yet implemented"),
            "msg: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn drift_pull_requires_source_arg() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d.dispatch(&[s("drift"), s("pull")], &c).await;
        assert!(!result.is_ok());
        assert!(
            result.message().contains("requires a source"),
            "msg: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn drift_pull_no_blocks_error() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let src = register_context(&d, Some("src-ctx"), None, principal);
        let dst = register_context(&d, Some("dst-ctx"), None, principal);

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
        assert!(
            result.message().contains("no blocks"),
            "msg: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn drift_pull_cannot_pull_from_self() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("self-ctx"), None, principal);

        let c = caller_with_context(ctx);
        let result = d
            .dispatch(&[s("drift"), s("pull"), s("self-ctx")], &c)
            .await;
        assert!(!result.is_ok());
        assert!(
            result.message().contains("cannot pull from self"),
            "msg: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn drift_merge_no_parent() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("orphan"), None, principal);

        let c = caller_with_context(ctx);
        let result = d.dispatch(&[s("drift"), s("merge")], &c).await;
        assert!(!result.is_ok());
        assert!(
            result.message().contains("not a fork"),
            "msg: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn drift_merge_no_blocks_error() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);
        let child = register_context(&d, Some("child"), Some(parent), principal);

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
        assert!(
            result.message().contains("no blocks"),
            "msg: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn drift_push_missing_content() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("x"), None, principal);
        let _dst = register_context(&d, Some("y"), None, principal);

        let c = caller_with_context(ctx);
        let result = d.dispatch(&[s("drift"), s("push"), s("y")], &c).await;
        assert!(!result.is_ok());
        assert!(
            result.message().contains("requires content"),
            "msg: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn drift_push_missing_content_suggests_summarize() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("x"), None, principal);
        let _dst = register_context(&d, Some("y"), None, principal);

        let c = caller_with_context(ctx);
        let result = d.dispatch(&[s("drift"), s("push"), s("y")], &c).await;
        assert!(!result.is_ok());
        assert!(
            result.message().contains("--summarize"),
            "msg: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn drift_push_summarize_empty_context_error() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("src"), None, principal);
        let _dst = register_context(&d, Some("dst"), None, principal);

        // Create empty document for source
        d.block_store()
            .create_document(ctx, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(ctx);
        let result = d
            .dispatch(&[s("drift"), s("push"), s("dst"), s("--summarize")], &c)
            .await;
        assert!(!result.is_ok());
        assert!(
            result.message().contains("no blocks"),
            "msg: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn drift_flush_delivers_to_existing_document() {
        // Verify the basic flush path works and requeues on missing document
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let src = register_context(&d, Some("sender"), None, principal);
        let dst_no_doc = register_context(&d, Some("nodoc"), None, principal);

        let c = caller_with_context(src);

        // Push to a context WITHOUT a document — insertion will fail
        d.dispatch(&[s("drift"), s("push"), s("nodoc"), s("will fail")], &c)
            .await;

        let result = d.dispatch(&[s("drift"), s("flush")], &c).await;
        assert!(result.is_ok(), "flush: {}", result.message());
        assert!(
            result.message().contains("requeued"),
            "msg: {}",
            result.message()
        );

        // The item should be back in the queue
        let result = d.dispatch(&[s("drift"), s("queue")], &c).await;
        assert!(
            result.message().contains("will fail"),
            "queue: {}",
            result.message()
        );

        // Now create the document and flush successfully
        d.block_store()
            .create_document(dst_no_doc, crate::DocumentKind::Conversation, None)
            .unwrap();
        let result = d.dispatch(&[s("drift"), s("flush")], &c).await;
        assert!(result.is_ok(), "flush2: {}", result.message());
        assert!(
            result.message().contains("flushed 1 drift"),
            "msg: {}",
            result.message()
        );
    }
}
