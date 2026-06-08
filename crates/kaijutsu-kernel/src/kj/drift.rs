//! Drift subcommands: push, flush, queue, cancel.
//!
//! Migrated to clap_derive following the `block`/`cas` template. One
//! `DriftArgs` struct + `DriftCommand` enum at the top; `dispatch_drift`
//! parses argv via `try_parse_from`, routes DisplayHelp to ok-ephemeral,
//! real errors to `KjResult::Err`, and empty argv to `clap_help_for`. The
//! per-verb async handler bodies are unchanged — only argv extraction moved
//! into the derive. Capability gating (push/pull/merge/flush/cancel on the
//! `Drift` cap) is preserved against the matched variant.

use clap::{Parser, Subcommand};
use kaijutsu_crdt::DriftKind;
use kaijutsu_types::{ContentType, EdgeKind};

use super::format::format_drift_queue;
use super::refs;
use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug)]
#[command(
    name = "drift",
    about = "Cross-context communication (push/pull/merge/flush)",
    disable_help_subcommand = true,
    no_binary_name = true
)]
struct DriftArgs {
    #[command(subcommand)]
    command: DriftCommand,
}

#[derive(Subcommand, Debug)]
enum DriftCommand {
    /// Stage content for a target context. With --summarize, LLM-distill
    /// the caller's whole context instead of sending literal content.
    Push {
        /// Destination context reference
        dst: String,
        /// LLM-distill the caller's context instead of using literal content
        #[arg(long, short = 's')]
        summarize: bool,
        /// Content to stage (joined with spaces). Omit when using --summarize.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        content: Vec<String>,
    },
    /// Pull + LLM-distill from a source context into the caller's context.
    Pull {
        /// Source context reference
        src: String,
        /// Optional directed prompt (joined with spaces)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        prompt: Vec<String>,
    },
    /// Summarize this fork back into the parent context (or a given ctx).
    Merge {
        /// Target context (defaults to forked_from parent)
        ctx: Option<String>,
    },
    /// Deliver all staged drifts.
    Flush,
    /// Show the staging queue (yields queue u64 ids).
    #[command(alias = "q")]
    Queue,
    /// Remove a staged drift before flush (pre-flush only).
    Cancel {
        /// Staged drift queue id (u64)
        queue_id: String,
    },
    /// Show drift edges for a context (yields edge UUIDs).
    History {
        /// Target context (defaults to caller's context)
        ctx: Option<String>,
    },
    /// Manage post-flush drift edges.
    Edge {
        #[command(subcommand)]
        op: EdgeCommand,
    },
}

#[derive(Subcommand, Debug)]
enum EdgeCommand {
    /// Remove a post-flush drift edge by its UUID (see `kj drift history`).
    #[command(alias = "remove")]
    Rm {
        /// Drift edge UUID
        uuid: String,
    },
}

impl KjDispatcher {
    pub(crate) async fn dispatch_drift(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<DriftArgs>();
        }
        let parsed = match DriftArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj drift: {e}"));
            }
        };

        // The cross-context write surface (push/pull/merge/flush/cancel) is
        // gated on `drift`; the read-only views (queue/history/edge) are not.
        if matches!(
            parsed.command,
            DriftCommand::Push { .. }
                | DriftCommand::Pull { .. }
                | DriftCommand::Merge { .. }
                | DriftCommand::Flush
                | DriftCommand::Cancel { .. }
        ) && let Err(denied) = self.require_cap(caller, crate::mcp::Capability::Drift, "drift")
        {
            return denied;
        }

        match parsed.command {
            DriftCommand::Push {
                dst,
                summarize,
                content,
            } => self.drift_push(&dst, summarize, &content, caller).await,
            DriftCommand::Pull { src, prompt } => self.drift_pull(&src, &prompt, caller).await,
            DriftCommand::Merge { ctx } => self.drift_merge(ctx.as_deref(), caller).await,
            DriftCommand::Flush => self.drift_flush(caller).await,
            DriftCommand::Queue => self.drift_queue().await,
            DriftCommand::Cancel { queue_id } => self.drift_cancel(&queue_id).await,
            DriftCommand::History { ctx } => self.drift_history(ctx.as_deref(), caller),
            DriftCommand::Edge { op } => match op {
                EdgeCommand::Rm { uuid } => self.drift_edge_rm(&uuid),
            },
        }
    }

    async fn drift_push(
        &self,
        dst_query: &str,
        summarize: bool,
        content: &[String],
        caller: &KjCaller,
    ) -> KjResult {
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
            if content.is_empty() {
                return KjResult::Err(
                    "kj drift push: requires content (or use --summarize)".to_string(),
                );
            }
            (content.join(" "), DriftKind::Push)
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

    async fn drift_pull(&self, src_query: &str, prompt: &[String], caller: &KjCaller) -> KjResult {
        // Resolve source context
        let source_id = {
            let db = self.kernel_db().lock();
            match refs::resolve_context_arg(Some(src_query), caller, &db) {
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
        let directed_prompt = if prompt.is_empty() {
            None
        } else {
            Some(prompt.join(" "))
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

    async fn drift_merge(&self, target_arg: Option<&str>, caller: &KjCaller) -> KjResult {
        let context_id = match caller.require_context() {
            Ok(id) => id,
            Err(e) => return e,
        };

        // kj drift merge [ctx]
        // Default target = caller's forked_from parent
        let target_id = if let Some(target_query) = target_arg {
            let db = self.kernel_db().lock();
            match refs::resolve_context_arg(Some(target_query), caller, &db) {
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

                    // Record the drift edge in context_edges so `kj drift
                    // history` can find it. `drift_kind` and `source_model`
                    // are recoverable from the inserted block itself, but we
                    // stash the pre-flush staged id in metadata so post-flush
                    // queries can trace back to the staging event (the same
                    // namespace `kj drift cancel` uses).
                    {
                        let db = self.kernel_db().lock();
                        let edge = crate::kernel_db::ContextEdgeRow {
                            edge_id: uuid::Uuid::now_v7(),
                            source_id: drift.source_ctx,
                            target_id: drift.target_ctx,
                            kind: EdgeKind::Drift,
                            metadata: Some(format!("{}#{}", drift.drift_kind, drift.id)),
                            created_at: kaijutsu_types::now_millis() as i64,
                        };
                        if let Err(e) = db.insert_edge(&edge) {
                            tracing::warn!(
                                "drift flush: failed to insert edge {} → {}: {e}",
                                drift.source_ctx.short(),
                                drift.target_ctx.short()
                            );
                        }
                    }

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
            let (lf_id, is_new) = {
                let mut router = self.drift_router().write();
                router.ensure_lost_found()
            };
            // Persist lost+found as a real context the first time it's created,
            // so the "registered handle implies a DB row" invariant holds and
            // dead letters survive a kernel restart (cold-start rehydrates it
            // from this row and re-adopts it). The router itself has no DB
            // handle; this caller does.
            if is_new {
                let db = self.kernel_db().lock();
                let system = kaijutsu_types::PrincipalId::system();
                match db.get_or_create_default_workspace(system) {
                    Ok(ws) => {
                        let now = kaijutsu_types::now_millis() as i64;
                        let row = crate::kernel_db::ContextRow {
                            context_id: lf_id,
                            label: Some("lost+found".to_string()),
                            provider: None,
                            model: None,
                            system_prompt: None,
                            consent_mode: kaijutsu_types::ConsentMode::Collaborative,
                            context_state: kaijutsu_types::ContextState::Live,
                            context_type: "default".to_string(),
                            created_at: now,
                            created_by: system,
                            forked_from: None,
                            fork_kind: None,
                            archived_at: None,
                            workspace_id: None,
                            preset_id: None,
                        };
                        if let Err(e) = db.insert_context_with_document(&row, ws) {
                            tracing::error!("failed to persist lost+found context row: {e}");
                        }
                    }
                    Err(e) => {
                        tracing::error!("failed to resolve workspace for lost+found: {e}")
                    }
                }
            }
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
    fn drift_history(&self, target_arg: Option<&str>, caller: &KjCaller) -> KjResult {
        let db = self.kernel_db().lock();

        let target_id = match super::refs::resolve_context_arg(target_arg, caller, &db) {
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
    fn drift_edge_rm(&self, id_str: &str) -> KjResult {
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

    async fn drift_cancel(&self, id_str: &str) -> KjResult {
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
    async fn drift_flush_persists_lost_found_context_row() {
        // A dead letter drained into lost+found must persist a real context
        // row, so the "registered handle implies a DB row" invariant holds and
        // the sink survives restart. The flush early-returns when the caller's
        // staging is empty, so we need both a deliverable item AND a dead
        // letter present at flush time.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let src = register_context(&d, Some("src"), None, principal);
        let dst = register_context(&d, Some("dst"), None, principal);
        d.block_store()
            .create_document(dst, crate::DocumentKind::Conversation, None)
            .unwrap();

        // Force a dead letter: stage a victim, then cycle drain→requeue past
        // the retry ceiling so it lands in the dead-letter queue.
        {
            let mut router = d.drift_router().write();
            router
                .stage(src, dst, "victim".into(), None, kaijutsu_crdt::DriftKind::Push)
                .unwrap();
            for _ in 0..8 {
                let drained = router.drain(None);
                router.requeue(drained);
            }
            assert_eq!(router.dead_letters().len(), 1, "victim should be dead-lettered");
        }

        // Stage a deliverable item so flush proceeds past the empty-staging guard.
        let c = caller_with_context(src);
        d.dispatch(&[s("drift"), s("push"), s("dst"), s("ok")], &c)
            .await;
        let result = d.dispatch(&[s("drift"), s("flush")], &c).await;
        assert!(result.is_ok(), "flush: {}", result.message());

        // lost+found now exists with a persisted context row.
        let lf_id = d
            .drift_router()
            .read()
            .lost_found_id()
            .expect("lost+found created during flush");
        let row = d
            .kernel_db()
            .lock()
            .get_context(lf_id)
            .unwrap()
            .expect("lost+found has a persisted context row");
        assert_eq!(row.label.as_deref(), Some("lost+found"));
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
    /// with the UUIDs that `kj drift history` emits in `.data`. Full
    /// round-trip: push a drift → flush writes both the Drift block and
    /// the `EdgeKind::Drift` row → history surfaces the UUID → edge rm
    /// removes it → history is empty again. This is the user-facing path,
    /// not a hand-seeded edge.
    #[tokio::test]
    async fn drift_edge_rm_round_trip() {
        use crate::kj::KjResult;

        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let src = register_context(&d, Some("src"), None, principal);
        let dst = register_context(&d, Some("dst"), None, principal);

        // Target document must exist for the flush to land.
        d.block_store()
            .create_document(dst, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(src);

        // Push + flush is what writes the edge.
        let push = d
            .dispatch(&[s("drift"), s("push"), s("dst"), s("payload")], &c)
            .await;
        assert!(push.is_ok(), "push: {}", push.message());
        let flush = d.dispatch(&[s("drift"), s("flush")], &c).await;
        assert!(flush.is_ok(), "flush: {}", flush.message());

        // History on the source surfaces the edge written by flush.
        let history = d.dispatch(&[s("drift"), s("history")], &c).await;
        let edge_id_str = match history {
            KjResult::Ok { data: Some(v), .. } => {
                let ids: Vec<String> = v
                    .as_array()
                    .expect("array")
                    .iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect();
                assert_eq!(ids.len(), 1, "expected one edge in .data: {ids:?}");
                ids.into_iter().next().unwrap()
            }
            other => panic!("expected Ok with data, got {other:?}"),
        };

        // The handle round-trips into `edge rm`.
        let rm = d
            .dispatch(&[s("drift"), s("edge"), s("rm"), edge_id_str.clone()], &c)
            .await;
        assert!(rm.is_ok(), "edge rm: {}", rm.message());
        assert!(
            rm.message().contains(&edge_id_str),
            "msg should echo the uuid: {}",
            rm.message()
        );

        // History is now empty.
        let again = d.dispatch(&[s("drift"), s("history")], &c).await;
        assert!(again.message().contains("no drift history"));
    }

    /// `drift_flush` must write a `context_edges` row alongside the
    /// injected Drift block — otherwise `kj drift history`
    /// (`drift_provenance` + `edges_to`) sees nothing. This test pins the
    /// flush write site: push one drift, flush, and verify exactly one
    /// Drift edge exists with the expected source/target/kind and
    /// metadata stamped with the staged id.
    #[tokio::test]
    async fn drift_flush_writes_edge() {
        use kaijutsu_types::EdgeKind;

        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let src = register_context(&d, Some("src"), None, principal);
        let dst = register_context(&d, Some("dst"), None, principal);
        d.block_store()
            .create_document(dst, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(src);
        let push = d
            .dispatch(&[s("drift"), s("push"), s("dst"), s("finding")], &c)
            .await;
        assert!(push.is_ok(), "push: {}", push.message());
        // First staged drift in a fresh router lands at id=1; we assert
        // metadata against that below.
        assert!(push.message().contains("#1"), "push id: {}", push.message());

        let flush = d.dispatch(&[s("drift"), s("flush")], &c).await;
        assert!(flush.is_ok(), "flush: {}", flush.message());

        let edges = {
            let db = d.kernel_db().lock();
            db.drift_provenance(src).expect("drift_provenance")
        };
        assert_eq!(edges.len(), 1, "expected one drift edge after flush: {edges:?}");
        let edge = &edges[0];
        assert_eq!(edge.source_id, src);
        assert_eq!(edge.target_id, dst);
        assert_eq!(edge.kind, EdgeKind::Drift);
        assert_eq!(
            edge.metadata.as_deref(),
            Some("push#1"),
            "metadata stamps kind + staged id: {:?}",
            edge.metadata
        );
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
            result.message().contains("required")
                || result.message().contains("<SRC>"),
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
