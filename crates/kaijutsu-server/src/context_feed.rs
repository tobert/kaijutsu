//! The context change feed — one ordered stream of domain facts per context.
//!
//! Canonical design: `docs/change-feed.md`. This module is the server half.
//!
//! # What makes it different from the `BlockEvents` bridge
//!
//! `BlockEvents` forwards thirteen separate methods. It used to carry
//! **serialized text-engine operations** on `onBlockTextOpsBatch`, which meant
//! a client had to link a text engine to learn what happened; that method was
//! deleted in the 2026-08-15 wire flag day. This feed carries decisions
//! instead of encodings: the kernel classified an append or a replace while
//! it held the mutation lock, and this module forwards that classification
//! without ever looking at operation bytes.
//!
//! Three properties follow, and each is load-bearing:
//!
//! - **Batching is native.** A delivery is a list, so a burst of streamed
//!   tokens is one call. The old surface needed a bespoke `onBlockTextOpsBatch`
//!   to say the same thing, and it could only batch *one block's* text ops.
//! - **A delivery is transactional.** A tool's final output text and its `Done`
//!   status ride the same call, closing the race where a client renders a
//!   finished tool with no output.
//! - **One clock.** The context's `version` replaces both the per-context op
//!   counter and the per-subscription delivery counter.
//!
//! # Ordering, and why this module sorts
//!
//! The kernel assigns a version under the document guard but publishes after
//! releasing it, so two writers racing on one context can publish out of
//! version order. Amy's ruling (2026-08-15) is that the bridge repairs it: hold
//! a short window, sort the window by version, deliver. That is honest about
//! its limit — a window cannot distinguish "late" from "never" — so the client
//! also treats a version that goes backwards for a block as a signal to refetch
//! rather than as text to apply.
//!
//! # What must never ride this feed
//!
//! Render cues and beat sync. A batching window buys fewer messages with
//! latency, and `docs/midi.md` ("The one timebase") forbids that trade for
//! anything on the musical timebase: missed beats are missed, never delayed and
//! never replayed. They keep the directive path they already have.

use std::time::Duration;

use kaijutsu_kernel::flows::{BlockFlow, FlowRecv, Subscription};
use kaijutsu_types::{ContextId, KernelId};
use tokio_util::sync::CancellationToken;

use crate::kaijutsu_capnp::{context_event, context_observer};

/// How long a delivery stays open once its first event arrives.
///
/// Two jobs at once: it coalesces a token burst into one call, and it gives a
/// racing publisher time to land so the sort below can order them. Small enough
/// that a lone edit still feels immediate.
///
/// `pub(crate)` so `rpc.rs`'s `subscribe_ledger_events` can reuse the exact
/// same latency budget rather than defining a second constant that could
/// drift from this one — the capnp doc comment on `subscribeLedgerEvents`
/// names this constant by path.
pub(crate) const FEED_BATCH_WINDOW: Duration = Duration::from_millis(4);

/// Hard cap on one delivery, so a firehose cannot grow an unbounded message.
const FEED_BATCH_MAX: usize = 512;

/// How long the observer has to accept one delivery.
const FEED_CALLBACK_TIMEOUT: Duration = Duration::from_secs(30);

/// Run one context's feed until the connection closes or the subscriber is
/// terminated for falling behind.
///
/// `sub` is a FlowBus subscription; the bus is lossless-or-terminated, so this
/// loop never silently drops an event — it either delivers it or the
/// subscription ends and the client is told to resubscribe and refetch.
pub(crate) async fn run_context_feed(
    observer: context_observer::Client,
    context_id: ContextId,
    mut sub: Subscription<BlockFlow>,
    kernel_id: KernelId,
    conn_cancel: CancellationToken,
    disconnect: CancellationToken,
) {
    let mut delivered_version = 0u64;

    loop {
        // 1. Block until something arrives (or the connection goes away).
        let first = tokio::select! {
            _ = conn_cancel.cancelled() => break,
            ev = sub.recv_event() => ev,
        };
        let mut batch: Vec<BlockFlow> = Vec::new();
        match first {
            None => break,
            Some(FlowRecv::Message(m)) => batch.push(m.payload),
            Some(FlowRecv::Terminated(info)) => {
                terminate(&observer, delivered_version, kernel_id, &info).await;
                disconnect.cancel();
                break;
            }
        }

        // 2. Hold the window open: coalesce, and let a racing publisher land.
        let deadline = tokio::time::Instant::now() + FEED_BATCH_WINDOW;
        while batch.len() < FEED_BATCH_MAX {
            let next = tokio::select! {
                _ = conn_cancel.cancelled() => break,
                ev = tokio::time::timeout_at(deadline, sub.recv_event()) => ev,
            };
            match next {
                // Window expired, or the bus closed — ship what we have.
                Err(_) | Ok(None) => break,
                Ok(Some(FlowRecv::Message(m))) => batch.push(m.payload),
                Ok(Some(FlowRecv::Terminated(info))) => {
                    // Deliver what we already hold first: those events are
                    // accepted facts, and the client's recovery is cheaper if
                    // its snapshot is as recent as possible.
                    if let Ok(Some(v)) =
                        deliver(&observer, context_id, &mut batch, kernel_id, delivered_version)
                            .await
                    {
                        delivered_version = v;
                    }
                    terminate(&observer, delivered_version, kernel_id, &info).await;
                    disconnect.cancel();
                    return;
                }
            }
        }

        // 3. Deliver. A batch that holds nothing this feed carries (a beat, a
        //    cue, another context's change) is not an empty delivery — it is no
        //    delivery at all.
        match deliver(&observer, context_id, &mut batch, kernel_id, delivered_version).await {
            Ok(Some(v)) => delivered_version = v,
            Ok(None) => continue,
            Err(fault) => {
                terminate_fault(&observer, delivered_version, kernel_id, fault).await;
                disconnect.cancel();
                break;
            }
        }
    }
}

/// Build and send one delivery. Returns the version the observer was brought
/// to, or `None` when nothing in the batch belonged on this feed or the
/// observer refused the call.
async fn deliver(
    observer: &context_observer::Client,
    context_id: ContextId,
    batch: &mut Vec<BlockFlow>,
    kernel_id: KernelId,
    last_delivered: u64,
) -> Result<Option<u64>, FeedFault> {
    // This feed is one context's. Another context's changes share the bus, not
    // the version counter, so they are not ours to deliver.
    batch.retain(|flow| flow.context_id() == context_id && carries(flow));
    if batch.is_empty() {
        return Ok(None);
    }

    // Version order within the delivery (rules 11, 13, 14). Stable, so two
    // events the kernel accepted at the same version keep the order it
    // published them in.
    batch.sort_by_key(|flow| flow.version().unwrap_or(0));
    let version = batch.last().and_then(|flow| flow.version()).unwrap_or(0);

    // The window repairs an inversion by sorting; an inversion that spans two
    // windows it cannot repair, because the earlier event is already gone. Say
    // so instead of shipping it: an event older than one already delivered
    // would either be applied out of order (corrupting text) or skipped
    // (losing a change), and neither is something a client can discover on its
    // own. `Err` here ends the feed, and the client rehydrates from a
    // snapshot — the same recovery a slow subscriber gets, for the same
    // reason.
    let oldest = batch.first().and_then(|flow| flow.version()).unwrap_or(0);
    if last_delivered > 0 && oldest <= last_delivered {
        tracing::error!(
            kernel = %kernel_id,
            %context_id,
            oldest,
            last_delivered,
            "context feed saw an event older than one already delivered — the \
             batching window could not repair it; ending the feed so the client \
             refetches rather than applying it out of order"
        );
        return Err(FeedFault::UnrepairableInversion);
    }

    let mut req = observer.on_context_changed_request();
    {
        let mut params = req.get();
        params.set_context_id(context_id.as_bytes());
        params.set_version(version);
        let mut list = params.init_events(batch.len() as u32);
        for (i, flow) in batch.iter().enumerate() {
            let mut event = list.reborrow().get(i as u32);
            // Per event, not just per delivery: a batch can straddle a
            // client's snapshot, and only the event's own version can say
            // which side of it the event falls on.
            event.set_version(flow.version().unwrap_or(0));
            write_event(event, flow);
        }
    }
    batch.clear();

    match tokio::time::timeout(FEED_CALLBACK_TIMEOUT, req.send().promise).await {
        Ok(Ok(_)) => Ok(Some(version)),
        Ok(Err(e)) => {
            tracing::debug!(kernel = %kernel_id, error = %e, "context feed delivery refused");
            Ok(None)
        }
        Err(_) => {
            tracing::warn!(
                kernel = %kernel_id,
                "context feed delivery timed out after {FEED_CALLBACK_TIMEOUT:?}"
            );
            Ok(None)
        }
    }
}

/// A fault the feed cannot deliver through — it ends the feed instead.
#[derive(Debug, Clone, Copy)]
enum FeedFault {
    /// An event arrived older than one already delivered, across two batching
    /// windows. Sorting cannot repair what has already been sent.
    UnrepairableInversion,
}

/// End the feed because the server cannot deliver a correct stream.
///
/// Distinct from the slow-subscriber path below: nothing is wrong with the
/// client here. It is told so, and recovers exactly the same way — refetch a
/// snapshot and start again.
async fn terminate_fault(
    observer: &context_observer::Client,
    delivered_version: u64,
    kernel_id: KernelId,
    fault: FeedFault,
) {
    const TERMINATE_TIMEOUT: Duration = Duration::from_secs(1);
    tracing::error!(
        kernel = %kernel_id,
        ?fault,
        delivered_version,
        "context feed ending on a server-side fault; the client will refetch"
    );
    let mut req = observer.on_terminated_request();
    {
        let mut p = req.get();
        p.set_reason(crate::kaijutsu_capnp::SubscriptionEndReason::InternalFault);
        p.set_delivered_version(delivered_version);
    }
    if tokio::time::timeout(TERMINATE_TIMEOUT, req.send().promise)
        .await
        .is_err()
    {
        tracing::debug!("context feed subscriber did not accept its fault notice");
    }
}

/// Tell the observer its feed is over, best effort and briefly.
///
/// A client that never implements this still recovers through the ordinary
/// reconnect path; it just does not learn why.
async fn terminate(
    observer: &context_observer::Client,
    delivered_version: u64,
    kernel_id: KernelId,
    info: &kaijutsu_kernel::flows::FlowTermination,
) {
    const TERMINATE_TIMEOUT: Duration = Duration::from_secs(1);
    tracing::error!(
        kernel = %kernel_id,
        topic = info.topic,
        delivered = info.delivered,
        capacity = info.capacity,
        delivered_version,
        "context feed subscriber fell behind — ending the feed and dropping the \
         connection so it resubscribes and refetches (no lossy delivery)"
    );
    let mut req = observer.on_terminated_request();
    {
        let mut p = req.get();
        p.set_reason(crate::kaijutsu_capnp::SubscriptionEndReason::SlowSubscriber);
        p.set_delivered_version(delivered_version);
    }
    if tokio::time::timeout(TERMINATE_TIMEOUT, req.send().promise)
        .await
        .is_err()
    {
        tracing::debug!("context feed subscriber did not accept its termination notice");
    }
}

/// Does this event belong on the change feed?
///
/// `TextOps` never does — raw operation bytes are the thing being retired, and
/// a feed that carried them would defeat its own purpose. `SyncReset` never
/// does either: oplog compaction is server maintenance, and a client holding
/// materialized text is not affected by it. `ContextSwitched` is a shell
/// concern rather than a block change (open question 1 in the design), and the
/// two timing directives are forbidden from a batched path by the timebase
/// doctrine.
fn carries(flow: &BlockFlow) -> bool {
    matches!(
        flow,
        BlockFlow::Inserted { .. }
            | BlockFlow::Deleted { .. }
            | BlockFlow::Moved { .. }
            | BlockFlow::TextAppended { .. }
            | BlockFlow::TextReplaced { .. }
            | BlockFlow::StatusChanged { .. }
            | BlockFlow::CollapsedChanged { .. }
            | BlockFlow::ExcludedChanged { .. }
            | BlockFlow::MetadataChanged { .. }
            | BlockFlow::OutputChanged { .. }
    )
}

/// Write one kernel event into its wire union arm.
///
/// Every arm here is a decision the kernel already made. Nothing in this
/// function inspects text, compares lengths, or decodes operations — if it ever
/// needs to, the classification has been put in the wrong place again.
fn write_event(builder: context_event::Builder<'_>, flow: &BlockFlow) {
    match flow {
        BlockFlow::Inserted {
            block, after_id, ..
        } => {
            let mut b = builder.init_block_inserted();
            b.set_has_after_id(after_id.is_some());
            if let Some(after) = after_id {
                crate::rpc::set_block_id_builder(&mut b.reborrow().init_after_id(), after);
            }
            crate::rpc::set_block_snapshot(&mut b.reborrow().init_block(), block);
        }
        BlockFlow::Deleted { block_id, .. } => {
            let mut b = builder.init_block_deleted();
            crate::rpc::set_block_id_builder(&mut b, block_id);
        }
        BlockFlow::Moved {
            block_id, after_id, ..
        } => {
            let mut b = builder.init_block_moved();
            b.set_has_after_id(after_id.is_some());
            crate::rpc::set_block_id_builder(&mut b.reborrow().init_block_id(), block_id);
            if let Some(after) = after_id {
                crate::rpc::set_block_id_builder(&mut b.reborrow().init_after_id(), after);
            }
        }
        BlockFlow::TextAppended {
            block_id, suffix, ..
        } => {
            let mut b = builder.init_text_appended();
            b.set_suffix(suffix);
            crate::rpc::set_block_id_builder(&mut b.reborrow().init_block_id(), block_id);
        }
        BlockFlow::TextReplaced {
            block_id, content, ..
        } => {
            let mut b = builder.init_text_replaced();
            b.set_content(content);
            crate::rpc::set_block_id_builder(&mut b.reborrow().init_block_id(), block_id);
        }
        BlockFlow::StatusChanged {
            block_id, status, ..
        } => {
            let mut b = builder.init_status_changed();
            b.set_status(crate::rpc::status_to_capnp(*status));
            crate::rpc::set_block_id_builder(&mut b.reborrow().init_block_id(), block_id);
        }
        BlockFlow::CollapsedChanged {
            block_id, collapsed, ..
        } => {
            let mut b = builder.init_collapsed_changed();
            b.set_value(*collapsed);
            crate::rpc::set_block_id_builder(&mut b.reborrow().init_block_id(), block_id);
        }
        BlockFlow::ExcludedChanged {
            block_id, excluded, ..
        } => {
            let mut b = builder.init_excluded_changed();
            b.set_value(*excluded);
            crate::rpc::set_block_id_builder(&mut b.reborrow().init_block_id(), block_id);
        }
        BlockFlow::MetadataChanged {
            block_id, metadata, ..
        } => {
            let mut b = builder.init_metadata_changed();
            crate::rpc::set_block_id_builder(&mut b.reborrow().init_block_id(), block_id);
            crate::rpc::build_block_metadata(b.reborrow().init_metadata(), metadata);
        }
        BlockFlow::OutputChanged {
            block_id, output, ..
        } => {
            let mut b = builder.init_output_changed();
            crate::rpc::set_block_id_builder(&mut b.reborrow().init_block_id(), block_id);
            if let Some(data) = output {
                crate::rpc::build_output_data(b.reborrow().init_output(), data);
            }
        }
        // Unreachable by contract: `carries` filtered these out before the
        // batch was built. Asserted in debug so a future arm added to
        // `carries` without an arm here is found in tests; in release the
        // event is dropped rather than mislabeled as another kind of change.
        other => debug_assert!(
            false,
            "context feed tried to write an event it does not carry: {other:?}"
        ),
    }
}
