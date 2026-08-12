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
use kaijutsu_types::{ContentType, ContextId, EdgeKind};

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
pub(crate) struct DriftArgs {
    #[command(subcommand)]
    command: DriftCommand,
}

#[derive(Subcommand, Debug)]
enum DriftCommand {
    /// Send content to a target context, delivered immediately. With
    /// --summarize, LLM-distill the caller's whole context instead of
    /// sending literal content. With --stage, queue it for a later
    /// `flush` instead of delivering now.
    Push {
        /// Destination context reference
        dst: String,
        /// LLM-distill the caller's context instead of using literal content
        #[arg(long, short = 's')]
        summarize: bool,
        /// Queue for a later `kj drift flush` instead of delivering now
        #[arg(long)]
        stage: bool,
        /// Content to send (joined with spaces). Omit when using --summarize.
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
                stage,
                content,
            } => {
                self.drift_push(&dst, summarize, stage, &content, caller)
                    .await
            }
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

    /// Insert a drift block into `target_ctx`, record the `context_edges`
    /// row, and run the target's `drift` rc lifecycle.
    ///
    /// This is the shared shape that `push` (immediate), `pull`, `merge`
    /// and `flush` each used to inline — see the "orchestration bloat"
    /// entry in `docs/issues.md`. Only the immediate-`push` path is routed
    /// through it so far; the others are unchanged and can migrate when
    /// they are next touched.
    ///
    /// A failed *insert* is returned to the caller so it can decide what to
    /// do with the content (immediate `push` stages it rather than losing
    /// it). A failed edge write or rc script is logged and does not fail
    /// the delivery — the block is already durable, and refusing to
    /// acknowledge a delivered block would be the worse lie.
    async fn deliver_drift(
        &self,
        source_ctx: ContextId,
        target_ctx: ContextId,
        content: &str,
        source_model: Option<String>,
        drift_kind: DriftKind,
        edge_metadata: String,
        caller: &KjCaller,
    ) -> Result<(), String> {
        let after = self.block_store().last_block_id(target_ctx);
        self.block_store()
            .insert_drift_block(
                target_ctx,
                None,
                after.as_ref(),
                content,
                source_ctx,
                source_model.clone(),
                drift_kind,
            )
            .map_err(|e| e.to_string())?;

        {
            let db = self.kernel_db().lock();
            let edge = crate::kernel_db::ContextEdgeRow {
                edge_id: uuid::Uuid::now_v7(),
                source_id: source_ctx,
                target_id: target_ctx,
                kind: EdgeKind::Drift,
                metadata: Some(edge_metadata),
                created_at: kaijutsu_types::now_millis() as i64,
            };
            if let Err(e) = db.insert_edge(&edge) {
                tracing::warn!(
                    "drift: failed to insert edge {} → {}: {e}",
                    source_ctx.short(),
                    target_ctx.short()
                );
            }
        }

        if let Err(e) = self
            .run_rc_lifecycle(
                super::lifecycle::VERB_DRIFT,
                target_ctx,
                None,
                None,
                Some(super::lifecycle::DriftInfo {
                    kind: drift_kind,
                    source_ctx,
                    target_ctx,
                    source_model,
                }),
                caller,
            )
            .await
        {
            tracing::warn!(
                "rc drift lifecycle {} → {}: {e}",
                source_ctx.short(),
                target_ctx.short()
            );
        }

        Ok(())
    }

    async fn drift_push(
        &self,
        dst_query: &str,
        summarize: bool,
        stage: bool,
        content: &[String],
        caller: &KjCaller,
    ) -> KjResult {
        // The caller's own context is resolved first: you must be somewhere
        // to push from, and `.`/`.parent` below are relative to it. This also
        // keeps the friendly "no active context joined" error ahead of any
        // destination-lookup failure.
        let context_id = match caller.require_context() {
            Ok(id) => id,
            Err(e) => return e,
        };

        // Resolve destination through the *shared* reference grammar —
        // `.`, `.parent` chains, full UUIDs, labels, hex prefixes — the same
        // one `pull`/`merge`/`history` use. Push previously resolved through
        // the in-memory router alone, so `merge .parent` worked and
        // `push .parent` did not (docs/drift-ux.md, gap #3).
        //
        // The router remains a fallback for anything registered in this
        // process that the DB cannot resolve. On a double failure we report
        // the DB's error: it resolves against the larger candidate set, so
        // its message (the ambiguity candidate list in particular) is the
        // more useful of the two.
        let target_id = {
            let db_result = {
                let db = self.kernel_db().lock();
                refs::resolve_context_arg(Some(dst_query), caller, &db)
            };
            match db_result {
                Ok(id) => id,
                Err(db_err) => {
                    let router = self.drift_router().read();
                    match router.resolve_context(dst_query) {
                        Ok(id) => id,
                        Err(_) => return KjResult::Err(format!("kj drift push: {db_err}")),
                    }
                }
            }
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

        // Deliver now unless the caller explicitly asked to batch. `push`
        // staging by default meant a send could report success and deliver
        // nothing until someone remembered `flush` — the silent-fallback
        // shape CLAUDE.md rejects. Staging survives as `--stage`, which is
        // what makes `cancel` and batching possible. See `docs/drift-ux.md`.
        if !stage {
            let stage_after_failure = |e: String| {
                let mut router = self.drift_router().write();
                match router.stage(
                    context_id,
                    target_id,
                    content.clone(),
                    source_model.clone(),
                    drift_kind,
                ) {
                    // Loud, not silent: the push failed, but the content is
                    // parked in the queue that already knows how to retry
                    // and dead-letter rather than lose it.
                    Ok(id) => KjResult::Err(format!(
                        "kj drift push: delivery to {dst_query} failed: {e}; staged as \
                         #{id} — run 'kj drift flush' to retry"
                    )),
                    Err(stage_err) => KjResult::Err(format!(
                        "kj drift push: delivery to {dst_query} failed: {e}; staging also \
                         failed: {stage_err} — CONTENT LOST"
                    )),
                }
            };

            return match self
                .deliver_drift(
                    context_id,
                    target_id,
                    &content,
                    source_model.clone(),
                    drift_kind,
                    drift_kind.to_string(),
                    caller,
                )
                .await
            {
                Ok(()) => KjResult::ok(format!("drifted → {}", dst_query)),
                Err(e) => stage_after_failure(e),
            };
        }

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

        // The staged queue is drained per-CALLER; the dead-letter queue below is
        // kernel-global. Returning early on an empty caller queue made the
        // global half unreachable in exactly the case that matters — every
        // drift dead-lettered and nothing left staged — so dead letters sat
        // forever and lost+found was never written. Only stop when BOTH are
        // empty. (Found live, 2026-08-04.)
        let has_dead = !self.drift_router().read().dead_letters().is_empty();
        if staged.is_empty() && !has_dead {
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
                    // Delivered — clear the in-flight bookkeeping `drain`
                    // set so this item stops showing up in `queue()` and a
                    // stray `cancel` on its id becomes a normal "not found"
                    // instead of silently doing nothing. Lock is taken and
                    // dropped here, never held across the `.await` below.
                    self.drift_router().write().complete(drift.id);

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

        // Drain dead letters (items that exceeded MAX_DRIFT_RETRIES, plus any
        // staged drift orphaned by `unregister`) into lost+found. Secure the
        // sink BEFORE draining: if lost+found can't be created, the queue must
        // keep its items rather than have them drained into a context that does
        // not exist. Re-read the flag — the loop above can dead-letter more.
        let dead_letter_count = self.drift_router().read().dead_letters().len();
        let has_dead = dead_letter_count > 0;
        let mut dead_written = 0usize;
        let mut dead_retained = 0usize;
        if has_dead {
            let lf_id = match self.ensure_lost_found_context() {
                Ok(id) => id,
                Err(e) => {
                    // Nothing between the count above and here can have
                    // touched dead_letter — ensure_lost_found_context only
                    // creates/claims the sink context, it never drains the
                    // queue — so the earlier count is still accurate; no
                    // need to re-acquire the read lock just to re-read it.
                    let retained = dead_letter_count;
                    tracing::error!("lost+found unavailable, {retained} dead letters retained: {e}");
                    return KjResult::Err(format!(
                        "kj drift flush: flushed {injected}/{count} drifts, but {retained} dead \
                         letter(s) could not be written — lost+found unavailable: {e} \
                         (they stay queued; run flush again)"
                    ));
                }
            };
            let dead = {
                let mut router = self.drift_router().write();
                router.drain_dead_letter()
            };
            let dead_count = dead.len();
            let mut unwritten = Vec::new();
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
                    item.source_model.clone(),
                    item.drift_kind,
                ) {
                    tracing::error!("failed to write dead letter to lost+found, retaining: {e}");
                    unwritten.push(item);
                }
            }
            dead_retained = unwritten.len();
            dead_written = dead_count - dead_retained;
            if !unwritten.is_empty() {
                self.drift_router().write().restore_dead_letters(unwritten);
            }
            tracing::warn!(
                count = dead_written,
                retained = dead_retained,
                context = %lf_id.short(),
                "wrote dead letter drifts to lost+found"
            );
        }

        // Dead letters used to be reported only to the log. Say it in the
        // result too — the whole point of the sink is that a human finds them.
        let mut dead_note = String::new();
        if dead_written > 0 {
            dead_note.push_str(&format!(
                "; wrote {dead_written} dead letter(s) to lost+found"
            ));
        }
        if dead_retained > 0 {
            dead_note.push_str(&format!(
                "; {dead_retained} dead letter(s) retained for the next flush"
            ));
        }
        if fail_count > 0 {
            KjResult::ok(format!(
                "flushed {injected}/{count} drifts ({fail_count} requeued){dead_note}"
            ))
        } else {
            KjResult::ok(format!("flushed {injected} drift(s){dead_note}"))
        }
    }

    /// The lost+found context id, creating the context if this kernel has none.
    ///
    /// Order matters and is the point of this helper: the `documents` row (via
    /// the block store) and the `contexts` row land BEFORE the router claims
    /// the handle, so the "a registered handle implies a KernelDb row"
    /// invariant holds even when the DB write fails. A failure rolls the
    /// document back and returns `Err` — the caller keeps its dead letters
    /// queued rather than writing them into a context that isn't there.
    fn ensure_lost_found_context(&self) -> Result<ContextId, String> {
        if let Some(id) = self.drift_router().read().lost_found_id() {
            return Ok(id);
        }

        let id = ContextId::new();
        let system = kaijutsu_types::PrincipalId::system();

        // 1. The document — this is what writes the `documents` row.
        self.block_store()
            .create_document(id, crate::DocumentKind::Conversation, None)
            .map_err(|e| format!("failed to create lost+found document: {e}"))?;

        // 2. The context row, so cold start rehydrates and re-adopts it and the
        //    dead letters survive a restart.
        let persist = {
            let db = self.kernel_db().lock();
            let now = kaijutsu_types::now_millis() as i64;
            db.get_or_create_default_workspace(system)
                .map_err(|e| format!("failed to resolve workspace for lost+found: {e}"))
                .and_then(|ws| {
                    let row = crate::kernel_db::ContextRow {
                        context_id: id,
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
                        workspace_id: Some(ws),
                        preset_id: None,
                        concluded_at: None,
                        last_activity_at: None,
                        promoted_at: None,
                        demoted_at: None,
                        paused_at: None,
                        cast_id: None,
                        origin_host: None,
                    };
                    db.insert_context_with_document(&row, ws)
                        .map_err(|e| format!("failed to persist lost+found context row: {e}"))
                })
        };
        if let Err(e) = persist {
            // Roll the document back so a retry starts clean instead of
            // leaving an orphan `documents` row behind per attempt.
            if let Err(cleanup) = self.block_store().delete_document(id) {
                tracing::error!(
                    context = %id.short(),
                    "lost+found rollback failed, orphan document row left behind: {cleanup}"
                );
            }
            return Err(e);
        }

        // 3. Only now is a handle legitimate.
        self.drift_router()
            .write()
            .claim_lost_found(id)
            .map_err(|e| format!("failed to claim lost+found: {e}"))
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
            .dispatch(
                &[
                    s("drift"),
                    s("push"),
                    s("--stage"),
                    s("dst"),
                    s("hello from src"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "push failed: {}", result.message());
        assert!(result.message().contains("staged drift #1"));

        // Check queue
        let result = d.dispatch(&[s("drift"), s("queue")], &c).await;
        assert!(result.is_ok());
        let msg = result.message();
        assert!(msg.contains("hello from src"), "queue: {msg}");
    }

    /// `push` delivers immediately — no `flush` in between. This is the
    /// slice-1 contract from `docs/drift-ux.md`: the verb named `push` must
    /// push. Staging as the default meant a send could report success and
    /// deliver nothing, which in the music case is a dropped note.
    #[tokio::test]
    async fn drift_push_delivers_immediately() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let src = register_context(&d, Some("src"), None, principal);
        let dst = register_context(&d, Some("dst"), None, principal);
        d.block_store()
            .create_document(dst, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(src);
        let result = d
            .dispatch(&[s("drift"), s("push"), s("dst"), s("hello from src")], &c)
            .await;
        assert!(result.is_ok(), "push failed: {}", result.message());

        // Landed in the target document without a flush.
        let blocks = d.block_store().block_snapshots(dst).expect("snapshots");
        let drifted: Vec<_> = blocks
            .iter()
            .filter(|b| b.kind == kaijutsu_types::BlockKind::Drift)
            .collect();
        assert_eq!(
            drifted.len(),
            1,
            "expected exactly one drift block, saw {} blocks total",
            blocks.len()
        );
        assert!(
            drifted[0].content.contains("hello from src"),
            "content: {}",
            drifted[0].content
        );

        // Nothing left staged.
        let queue = d.dispatch(&[s("drift"), s("queue")], &c).await;
        assert_eq!(queue.message(), "(queue empty)");
    }

    /// `--stage` keeps the old batching behaviour: nothing reaches the
    /// target until `flush`. Staging is still worth having (it is what
    /// makes `cancel` possible and lets a sender batch); it just is not the
    /// default any more.
    #[tokio::test]
    async fn drift_push_stage_defers_delivery() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let src = register_context(&d, Some("src"), None, principal);
        let dst = register_context(&d, Some("dst"), None, principal);
        d.block_store()
            .create_document(dst, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(src);
        let result = d
            .dispatch(
                &[s("drift"), s("push"), s("--stage"), s("dst"), s("later")],
                &c,
            )
            .await;
        assert!(result.is_ok(), "push --stage failed: {}", result.message());
        assert!(
            result.message().contains("staged drift #1"),
            "msg: {}",
            result.message()
        );

        // Nothing delivered yet.
        let blocks = d.block_store().block_snapshots(dst).expect("snapshots");
        assert!(
            !blocks
                .iter()
                .any(|b| b.kind == kaijutsu_types::BlockKind::Drift),
            "staged drift must not be delivered before flush"
        );

        // It is sitting in the queue, and flush delivers it.
        let queue = d.dispatch(&[s("drift"), s("queue")], &c).await;
        assert!(queue.message().contains("later"), "queue: {}", queue.message());
        let flush = d.dispatch(&[s("drift"), s("flush")], &c).await;
        assert!(flush.is_ok(), "flush: {}", flush.message());
        let blocks = d.block_store().block_snapshots(dst).expect("snapshots");
        assert!(blocks
            .iter()
            .any(|b| b.kind == kaijutsu_types::BlockKind::Drift));
    }

    /// An immediate push whose delivery fails must not lose the content and
    /// must not pretend it worked. It falls back to the staging queue — the
    /// machinery that already knows how to retry and dead-letter — and says
    /// so loudly via `KjResult::Err`. Loud fallback, not silent.
    #[tokio::test]
    async fn drift_push_delivery_failure_stages_loudly() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let src = register_context(&d, Some("sender"), None, principal);
        // No document on the target — insertion cannot succeed.
        let _dst = register_context(&d, Some("nodoc"), None, principal);

        let c = caller_with_context(src);
        let result = d
            .dispatch(&[s("drift"), s("push"), s("nodoc"), s("precious")], &c)
            .await;
        assert!(
            !result.is_ok(),
            "a failed delivery must not report success: {}",
            result.message()
        );
        assert!(
            result.message().contains("staged"),
            "error must say the content was staged: {}",
            result.message()
        );

        // Content preserved, recoverable by flush.
        let queue = d.dispatch(&[s("drift"), s("queue")], &c).await;
        assert!(
            queue.message().contains("precious"),
            "content must survive a failed push: {}",
            queue.message()
        );
    }

    /// `push` must accept the same address grammar as `pull`/`merge`/
    /// `history` — including the structural `.parent` chain. Before this,
    /// `merge .parent` worked and `push .parent` did not, because push
    /// resolved through the in-memory router while everything else went
    /// through `refs::resolve_context_arg`. Structural refs also dodge label
    /// collisions entirely, which matters while the `cc-*` namespace is
    /// crowded (see `docs/drift-ux.md` gap #5).
    #[tokio::test]
    async fn drift_push_accepts_parent_ref() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);
        let child = register_context(&d, Some("child"), Some(parent), principal);
        d.block_store()
            .create_document(parent, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(child);
        let result = d
            .dispatch(&[s("drift"), s("push"), s(".parent"), s("upward")], &c)
            .await;
        assert!(result.is_ok(), "push .parent failed: {}", result.message());

        let blocks = d.block_store().block_snapshots(parent).expect("snapshots");
        assert!(
            blocks
                .iter()
                .any(|b| b.kind == kaijutsu_types::BlockKind::Drift
                    && b.content.contains("upward")),
            "drift should have landed in the parent context"
        );
    }

    /// `.` resolves to the caller's own context on `push` too. Pushing to
    /// yourself is odd but legal, and the point here is that the grammar is
    /// shared — `.` must not be read as a label.
    #[tokio::test]
    async fn drift_push_accepts_current_ref() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let me = register_context(&d, Some("me"), None, principal);
        d.block_store()
            .create_document(me, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(me);
        let result = d
            .dispatch(&[s("drift"), s("push"), s("."), s("note to self")], &c)
            .await;
        assert!(result.is_ok(), "push . failed: {}", result.message());
        let blocks = d.block_store().block_snapshots(me).expect("snapshots");
        assert!(blocks
            .iter()
            .any(|b| b.kind == kaijutsu_types::BlockKind::Drift));
    }

    /// An ambiguous label prefix must still report every candidate rather
    /// than silently picking one. This is the failure mode the crowded
    /// `cc-*` namespace produces in the field, so it is worth pinning.
    #[tokio::test]
    async fn drift_push_ambiguous_prefix_names_candidates() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let src = register_context(&d, Some("src"), None, principal);
        let _a = register_context(&d, Some("cc-proj-aaaa"), None, principal);
        let _b = register_context(&d, Some("cc-proj-bbbb"), None, principal);

        let c = caller_with_context(src);
        let result = d
            .dispatch(&[s("drift"), s("push"), s("cc-proj"), s("hi")], &c)
            .await;
        assert!(!result.is_ok(), "ambiguous prefix must not resolve");
        let msg = result.message();
        assert!(msg.contains("ambiguous"), "msg: {msg}");
        assert!(
            msg.contains("cc-proj-aaaa") && msg.contains("cc-proj-bbbb"),
            "error must name the candidates: {msg}"
        );
    }

    #[tokio::test]
    async fn drift_cancel() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let src = register_context(&d, Some("a"), None, principal);
        let _dst = register_context(&d, Some("b"), None, principal);

        let c = caller_with_context(src);
        d.dispatch(&[s("drift"), s("push"), s("--stage"), s("b"), s("content")], &c)
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
            &[
                s("drift"),
                s("push"),
                s("--stage"),
                s("receiver"),
                s("important finding"),
            ],
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
        // the sink survives restart. This is the mixed case: one deliverable
        // item AND one dead letter in the same flush. (The dead-letters-only
        // case is `drift_flush_drains_dead_letters_with_nothing_staged`.)
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
        d.dispatch(&[s("drift"), s("push"), s("--stage"), s("dst"), s("ok")], &c)
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
    async fn drift_flush_drains_dead_letters_with_nothing_staged() {
        // FOUND LIVE 2026-08-04. The staged queue drains per-CALLER; the
        // dead-letter queue is kernel-global. The empty-staging early return
        // skipped the global half entirely, so in the case that matters most
        // — every drift dead-lettered, nothing left staged — `kj drift flush`
        // answered "nothing to flush" and the dead letters sat there forever,
        // with lost+found never written.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let src = register_context(&d, Some("src"), None, principal);
        let dst = register_context(&d, Some("dst"), None, principal);
        d.block_store()
            .create_document(dst, crate::DocumentKind::Conversation, None)
            .unwrap();

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
            assert!(router.queue().is_empty(), "nothing may remain staged");
        }

        let c = caller_with_context(src);
        let result = d.dispatch(&[s("drift"), s("flush")], &c).await;
        assert!(result.is_ok(), "flush: {}", result.message());
        assert!(
            result.message().contains("dead letter"),
            "the result must report the dead letters, not just the log: {}",
            result.message()
        );
        assert!(
            d.drift_router().read().dead_letters().is_empty(),
            "dead letters must be drained even with an empty staging queue"
        );

        // …into a lost+found that exists in both the router and the DB.
        let lf_id = d
            .drift_router()
            .read()
            .lost_found_id()
            .expect("lost+found created during flush");
        assert!(
            d.kernel_db().lock().get_context(lf_id).unwrap().is_some(),
            "lost+found handle implies a KernelDb row"
        );
        assert!(
            d.block_store().last_block_id(lf_id).is_some(),
            "the dead letter must actually be written into lost+found"
        );
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
            .dispatch(
                &[s("drift"), s("push"), s("--stage"), s("dst"), s("payload")],
                &c,
            )
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
            .dispatch(
                &[s("drift"), s("push"), s("--stage"), s("dst"), s("finding")],
                &c,
            )
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

    /// End-to-end coverage for `summarize`'s pull path — the sibling
    /// error-path tests (`drift_pull_missing_source`, `_no_blocks_error`,
    /// `_cannot_pull_from_self`) never exercise a successful distillation, so
    /// nothing here proved `summarize` actually runs and its result lands as
    /// a Drift block. A MockClient stands in for the LLM (the real
    /// `summarize` call goes through the same `prompt_with_system` path
    /// `fork --compact` uses — see `fork_compact_distill_uses_calling_context_provider`).
    #[tokio::test]
    async fn drift_pull_summarizes_and_inserts_drift_block() {
        use crate::llm::{MockClient, Provider};
        use std::sync::Arc;

        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("source"), None, principal);
        let dest = register_context(&d, Some("dest"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        d.block_store()
            .create_document(dest, crate::DocumentKind::Conversation, None)
            .unwrap();
        d.block_store()
            .insert_block(
                source,
                None,
                None,
                kaijutsu_crdt::Role::User,
                kaijutsu_crdt::BlockKind::Text,
                "material to distill",
                kaijutsu_crdt::Status::Done,
                kaijutsu_crdt::ContentType::Plain,
            )
            .unwrap();

        {
            let mut reg = d.kernel().llm().write().await;
            reg.register(
                "mock",
                Arc::new(Provider::Mock(MockClient::new("PULL-SUMMARY"))),
            );
            reg.set_default("mock");
        }
        {
            let mut drift = d.drift_router().write();
            let _ = drift.configure_llm(source, "mock", "mock-model");
        }

        let c = caller_with_context(dest);
        let result = d
            .dispatch(&[s("drift"), s("pull"), s("source")], &c)
            .await;
        assert!(result.is_ok(), "drift pull failed: {}", result.message());

        let blocks = d.block_store().block_snapshots(dest).unwrap();
        let drift_block = blocks
            .iter()
            .find(|b| b.kind == kaijutsu_crdt::BlockKind::Drift);
        assert!(
            drift_block.is_some(),
            "drift pull must insert a Drift block into the caller's context: {blocks:?}"
        );
        assert!(
            drift_block.unwrap().content.contains("PULL-SUMMARY"),
            "drift block should carry the LLM's distilled summary: {:?}",
            drift_block.unwrap().content
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

    /// End-to-end coverage for `summarize`'s merge path (mirrors
    /// `drift_pull_summarizes_and_inserts_drift_block` above) — the
    /// sibling error-path tests (`drift_merge_no_parent`,
    /// `_no_blocks_error`) never exercise a successful distillation.
    #[tokio::test]
    async fn drift_merge_summarizes_and_inserts_drift_block() {
        use crate::llm::{MockClient, Provider};
        use std::sync::Arc;

        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);
        let child = register_context(&d, Some("child"), Some(parent), principal);
        d.block_store()
            .create_document(parent, crate::DocumentKind::Conversation, None)
            .unwrap();
        d.block_store()
            .create_document(child, crate::DocumentKind::Conversation, None)
            .unwrap();
        d.block_store()
            .insert_block(
                child,
                None,
                None,
                kaijutsu_crdt::Role::User,
                kaijutsu_crdt::BlockKind::Text,
                "child material to distill",
                kaijutsu_crdt::Status::Done,
                kaijutsu_crdt::ContentType::Plain,
            )
            .unwrap();

        {
            let mut reg = d.kernel().llm().write().await;
            reg.register(
                "mock",
                Arc::new(Provider::Mock(MockClient::new("MERGE-SUMMARY"))),
            );
            reg.set_default("mock");
        }
        {
            let mut drift = d.drift_router().write();
            let _ = drift.configure_llm(child, "mock", "mock-model");
        }

        let c = caller_with_context(child);
        let result = d.dispatch(&[s("drift"), s("merge")], &c).await;
        assert!(result.is_ok(), "drift merge failed: {}", result.message());

        let blocks = d.block_store().block_snapshots(parent).unwrap();
        let drift_block = blocks
            .iter()
            .find(|b| b.kind == kaijutsu_crdt::BlockKind::Drift);
        assert!(
            drift_block.is_some(),
            "drift merge must insert a Drift block into the parent context: {blocks:?}"
        );
        assert!(
            drift_block.unwrap().content.contains("MERGE-SUMMARY"),
            "drift block should carry the LLM's distilled summary: {:?}",
            drift_block.unwrap().content
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

        // Stage to a context WITHOUT a document — insertion will fail at flush
        d.dispatch(
            &[
                s("drift"),
                s("push"),
                s("--stage"),
                s("nodoc"),
                s("will fail"),
            ],
            &c,
        )
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
