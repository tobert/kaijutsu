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
//! - **A mutation stays complete.** All events of one accepted mutation ride
//!   the same call. Separate mutations can land in separate deliveries.
//! - **One clock.** The context's `version` replaces both the per-context op
//!   counter and the per-subscription delivery counter.
//!
//! # Ordering
//!
//! The block store holds the document guard through durable acceptance and
//! publication. This bridge preserves that order and ends the feed if a
//! delivery would move its version backward. FlowBus marks complete publish
//! groups, so neither the batching window nor the size limit splits a mutation.
//!
//! # What must never ride this feed
//!
//! Render cues and beat sync. A batching window buys fewer messages with
//! latency, and `docs/midi.md` ("The one timebase") forbids that trade for
//! anything on the musical timebase: missed beats are missed, never delayed and
//! never replayed. They keep the directive path they already have.

use std::time::Duration;

use kaijutsu_kernel::flows::{BlockFlow, FlowMessage, FlowRecv, FlowTopics, Subscription, TopicClass};
use kaijutsu_types::{ContextId, KernelId};
use tokio_util::sync::CancellationToken;

use crate::kaijutsu_capnp::{context_event, context_observer};

/// How long a delivery stays open once its first event arrives.
///
/// Coalesces a token burst into one call. Small enough that a lone edit
/// still feels immediate.
///
/// `pub(crate)` so `rpc.rs`'s `subscribe_ledger_events` can reuse the exact
/// same latency budget rather than defining a second constant that could
/// drift from this one — the capnp doc comment on `subscribeLedgerEvents`
/// names this constant by path.
pub(crate) const FEED_BATCH_WINDOW: Duration = Duration::from_millis(4);

/// Target cap on one delivery. Finish a started group before closing it.
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
        let mut complete = 0;
        match first {
            None => break,
            Some(FlowRecv::Message(m)) => append_message(&mut batch, &mut complete, m),
            Some(FlowRecv::Terminated(info)) => {
                terminate(&observer, delivered_version, kernel_id, &info).await;
                disconnect.cancel();
                break;
            }
        }

        // 2. Hold the window open to coalesce a burst.
        let deadline = tokio::time::Instant::now() + FEED_BATCH_WINDOW;
        while batch.len() < FEED_BATCH_MAX || complete < batch.len() {
            let next = tokio::select! {
                _ = conn_cancel.cancelled() => return,
                ev = async {
                    if complete < batch.len() {
                        Ok(sub.recv_event().await)
                    } else {
                        tokio::time::timeout_at(deadline, sub.recv_event()).await
                    }
                } => ev,
            };
            match next {
                Err(_) => break,
                Ok(None) => {
                    if complete < batch.len() {
                        terminate_fault(&observer, delivered_version, kernel_id, FeedFault::IncompleteGroup).await;
                        disconnect.cancel();
                        return;
                    }
                    break;
                }
                Ok(Some(FlowRecv::Message(m))) => append_message(&mut batch, &mut complete, m),
                Ok(Some(FlowRecv::Terminated(info))) => {
                    // Only complete groups can advance the client's version.
                    batch.truncate(complete);
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

/// Timing events use their own lane and cannot end an ordered group.
fn append_message(batch: &mut Vec<BlockFlow>, complete: &mut usize, message: FlowMessage<BlockFlow>) {
    if BlockFlow::topic_class(message.topic) == TopicClass::Timing { return; }
    batch.push(message.payload);
    if message.group_end { *complete = batch.len(); }
}

/// Build and send one delivery. Returns the version the observer was brought
/// to, or `None` when nothing in the batch belonged on this feed. A refused
/// or timed-out callback ends the feed because acceptance is unknown.
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

    let version = batch.last().and_then(|flow| flow.version()).unwrap_or(0);
    let oldest = batch.first().and_then(|flow| flow.version()).unwrap_or(0);
    if oldest <= last_delivered || batch.windows(2).any(|pair| pair[0].version() > pair[1].version()) {
        tracing::error!(
            kernel = %kernel_id, %context_id, oldest, last_delivered,
            "context feed received events out of version order; ending the feed for recovery"
        );
        return Err(FeedFault::VersionOrder);
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
            Err(FeedFault::CallbackRefused)
        }
        Err(_) => {
            tracing::warn!(
                kernel = %kernel_id,
                "context feed delivery timed out after {FEED_CALLBACK_TIMEOUT:?}"
            );
            Err(FeedFault::CallbackTimeout)
        }
    }
}

/// A fault the feed cannot deliver through — it ends the feed instead.
#[derive(Debug, Clone, Copy)]
enum FeedFault {
    VersionOrder,
    IncompleteGroup,
    CallbackRefused,
    CallbackTimeout,
}

/// End the feed because the server cannot deliver a correct stream.
///
/// Stream order or callback acceptance is uncertain. The client must
/// resubscribe and refetch a snapshot; replaying an uncertain append could
/// apply it twice.
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
        "context feed ending on a delivery fault; the client will refetch"
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
            | BlockFlow::SpansChanged { .. }
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
        BlockFlow::SpansChanged {
            block_id,
            style_spans,
            provenance,
            ..
        } => {
            let mut b = builder.init_spans_changed();
            crate::rpc::set_block_id_builder(&mut b.reborrow().init_block_id(), block_id);
            if let Some(tag) = provenance {
                b.set_provenance_transform(&tag.transform);
                b.set_provenance_version(tag.version);
            }
            crate::rpc::set_style_spans(
                b.reborrow().init_style_spans(style_spans.len() as u32),
                style_spans,
            );
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

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{BlockId, ContextId, PrincipalId, ProvenanceTag, StyleAttrs, StyleColor,
        StyleSpan};

    struct FailingObserver {
        timeout: bool,
        notice_timeout: bool,
        deliveries: tokio::sync::mpsc::UnboundedSender<u64>,
        terminations: tokio::sync::mpsc::UnboundedSender<(crate::kaijutsu_capnp::SubscriptionEndReason, u64)>,
    }

    impl context_observer::Server for FailingObserver {
        fn on_context_changed(
            self: std::rc::Rc<Self>,
            params: context_observer::OnContextChangedParams,
            _results: context_observer::OnContextChangedResults,
        ) -> impl std::future::Future<Output = Result<(), capnp::Error>> + 'static {
            let version = params.get().unwrap().get_version();
            self.deliveries.send(version).unwrap();
            if version != 2 {
                capnp::capability::Promise::ok(())
            } else if self.timeout {
                capnp::capability::Promise::from_future(std::future::pending())
            } else {
                capnp::capability::Promise::err(capnp::Error::failed("delivery refused".into()))
            }
        }

        fn on_terminated(
            self: std::rc::Rc<Self>,
            params: context_observer::OnTerminatedParams,
            _results: context_observer::OnTerminatedResults,
        ) -> impl std::future::Future<Output = Result<(), capnp::Error>> + 'static {
            let p = params.get().unwrap();
            self.terminations.send((p.get_reason().unwrap(), p.get_delivered_version())).unwrap();
            if self.notice_timeout {
                capnp::capability::Promise::from_future(std::future::pending())
            } else {
                capnp::capability::Promise::err(capnp::Error::failed("notice refused".into()))
            }
        }
    }

    async fn callback_failure_ends_feed(timeout: bool, notice_timeout: bool) {
        use kaijutsu_kernel::flows::{FlowBus, OpSource};

        tokio::task::LocalSet::new().run_until(async {
            let ctx = ContextId::new();
            let id = BlockId::new(ctx, PrincipalId::new(), 1);
            let bus = FlowBus::new(8);
            let sub = bus.subscribe("block.>");
            let publish = |version| bus.publish(BlockFlow::TextAppended {
                context_id: ctx, block_id: id, suffix: "x".into(), version, source: OpSource::Local,
            });
            let (deliveries, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let (terminations, mut ended) = tokio::sync::mpsc::unbounded_channel();
            let observer = capnp_rpc::new_client(FailingObserver {
                timeout, notice_timeout, deliveries, terminations,
            });
            let disconnect = CancellationToken::new();
            let feed = tokio::task::spawn_local(run_context_feed(
                observer, ctx, sub, KernelId::new(), CancellationToken::new(), disconnect.clone(),
            ));
            publish(1);
            assert_eq!(rx.recv().await, Some(1));
            publish(2);
            assert_eq!(rx.recv().await, Some(2));
            publish(3);
            tokio::time::timeout(FEED_CALLBACK_TIMEOUT + Duration::from_secs(2), feed).await
                .expect("a failed callback must end the feed, even if its termination notice fails")
                .unwrap();
            assert!(disconnect.is_cancelled(), "disconnect forces snapshot recovery");
            assert_eq!(ended.recv().await, Some((
                crate::kaijutsu_capnp::SubscriptionEndReason::InternalFault, 1,
            )), "only acknowledged deliveries advance the recovery version");
            assert_eq!(rx.recv().await, None, "no later append may cross a failed delivery");
        }).await;
    }

    #[tokio::test(start_paused = true)]
    async fn refused_callback_ends_feed() {
        callback_failure_ends_feed(false, false).await;
    }

    #[tokio::test(start_paused = true)]
    async fn timed_out_callback_ends_feed() {
        callback_failure_ends_feed(true, true).await;
    }

    #[tokio::test(start_paused = true)]
    async fn draft_submission_stays_complete_at_the_delivery_limit() {
        use capnp::capability::FromClientHook;
        use kaijutsu_client::{ContextChange, ContextMirror, FeedEvent, context_feed_channel};
        use kaijutsu_kernel::{BlockStore, block_store::DocumentKind};
        use kaijutsu_kernel::flows::FlowBus;
        use std::sync::Arc;

        tokio::task::LocalSet::new().run_until(async {
            let ctx = ContextId::new();
            let me = PrincipalId::new();
            let bus = Arc::new(FlowBus::new(FEED_BATCH_MAX * 4));
            let store = BlockStore::with_flows(me, bus.clone());
            store.create_document(ctx, DocumentKind::Conversation, None).unwrap();
            let id = store.edit_draft(ctx, me, 0, "hello", 0).unwrap();
            let sub = bus.subscribe("block.>");
            let mut mirror = ContextMirror::new(ctx);
            mirror.apply_snapshot(store.block_snapshots(ctx).unwrap(), store.get(ctx).unwrap().version()).unwrap();
            for _ in 0..FEED_BATCH_MAX - 1 { store.append_text(ctx, &id, "x").unwrap(); }
            store.submit_draft(ctx, me, None).unwrap();
            let submitted_version = store.get(ctx).unwrap().version();
            store.append_text(ctx, &id, "after").unwrap();
            let target = store.get(ctx).unwrap().version();
            let (observer, mut rx) = context_feed_channel(8);
            let cancel = CancellationToken::new();
            let disconnect = CancellationToken::new();
            let feed = tokio::task::spawn_local(run_context_feed(
                observer.cast_to(), ctx, sub, KernelId::new(), cancel.clone(), disconnect.clone(),
            ));
            let mut last = 0;
            while last < target {
                let event = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await.unwrap().unwrap();
                let FeedEvent::Changed(delivery) = event else { panic!("unexpected feed event: {event:?}"); };
                assert!(delivery.events[0].version > last);
                assert!(delivery.events.windows(2).all(|pair| pair[0].version <= pair[1].version));
                let submission: Vec<_> = delivery.events.iter()
                    .filter(|event| event.version == submitted_version).collect();
                if !submission.is_empty() {
                    assert_eq!(submission.len(), 2, "a delivery must contain the whole submitted mutation");
                    assert!(matches!(submission[0].change, ContextChange::StatusChanged { .. }));
                    assert!(matches!(submission[1].change, ContextChange::MetadataChanged { .. }));
                }
                last = delivery.version;
                mirror.receive(delivery).expect("complete mutation applies to the client mirror");
            }
            let live = store.get_block_snapshot(ctx, &id).unwrap().unwrap();
            let seen = mirror.block(&id).unwrap();
            assert_eq!(seen.content, live.content);
            assert_eq!(seen.status, live.status);
            assert_eq!(seen.ephemeral, live.ephemeral);
            assert!(!disconnect.is_cancelled());
            cancel.cancel();
            feed.await.unwrap();
        }).await;
    }

    #[tokio::test(start_paused = true)]
    async fn feed_refuses_an_inversion_instead_of_sorting_it() {
        use capnp::capability::FromClientHook;
        use kaijutsu_client::{FeedEvent, context_feed_channel};
        use kaijutsu_client::subscriptions::SubscriptionEndReason;
        use kaijutsu_kernel::flows::{FlowBus, OpSource};

        tokio::task::LocalSet::new().run_until(async {
            let ctx = ContextId::new();
            let id = BlockId::new(ctx, PrincipalId::new(), 1);
            let bus = FlowBus::new(8);
            let sub = bus.subscribe("block.>");
            for version in [2, 1] {
                bus.publish(BlockFlow::TextAppended { context_id: ctx, block_id: id,
                    suffix: "x".into(), version, source: OpSource::Local });
            }
            let (observer, mut rx) = context_feed_channel(8);
            let disconnect = CancellationToken::new();
            run_context_feed(observer.cast_to(), ctx, sub, KernelId::new(),
                CancellationToken::new(), disconnect.clone()).await;
            assert!(disconnect.is_cancelled());
            assert!(matches!(rx.recv().await, Some(FeedEvent::Terminated {
                reason: SubscriptionEndReason::InternalFault, delivered_version: 0,
            })));
        }).await;
    }

    #[test]
    fn timing_messages_cannot_complete_an_ordered_group() {
        use kaijutsu_kernel::flows::OpSource;
        let ctx = ContextId::new();
        let id = BlockId::new(ctx, PrincipalId::new(), 1);
        let flow = BlockFlow::TextAppended { context_id: ctx, block_id: id,
            suffix: "x".into(), version: 1, source: OpSource::Local };
        let mut first = FlowMessage::new(flow.subject(), flow.clone());
        first.group_end = false;
        let mut batch = Vec::new();
        let mut complete = 0;
        append_message(&mut batch, &mut complete, first);
        let beat = BlockFlow::BeatSync { context_id: ctx,
            beat_ref: kaijutsu_audio::BeatRef::new(0.0, 2.0) };
        append_message(&mut batch, &mut complete, FlowMessage::new(beat.subject(), beat));
        assert_eq!(complete, 0, "a timing event cannot complete the partial ordered group");
        append_message(&mut batch, &mut complete, FlowMessage::new(flow.subject(), flow));
        assert_eq!(complete, 2);
        assert_eq!(batch.len(), 2, "timing events never enter the delivery");
    }

    /// The encoder half of the span wire mapping (docs/ansi-and-beyond.md).
    ///
    /// `Indexed` and `Rgb` share one `(kind, value)` pair, so a mapping that
    /// forgot the kind byte — or packed the components in the wrong order —
    /// would still look plausible for one of them. Read back through the
    /// generated reader rather than through the client's decoder, so this
    /// pins what the SERVER put on the wire.
    #[test]
    fn spans_changed_writes_the_wire_color_encoding() {
        let context_id = ContextId::new();
        let block_id = BlockId::new(context_id, PrincipalId::new(), 1);
        let flow = BlockFlow::SpansChanged {
            context_id,
            block_id,
            style_spans: vec![
                StyleSpan {
                    start: 0,
                    end: 4,
                    fg: Some(StyleColor::Indexed(9)),
                    bg: None,
                    attrs: StyleAttrs::BOLD | StyleAttrs::UNDERLINE,
                },
                StyleSpan {
                    start: 4,
                    end: 9,
                    fg: Some(StyleColor::Rgb(0x12, 0x34, 0x56)),
                    bg: Some(StyleColor::Indexed(0)),
                    attrs: StyleAttrs::default(),
                },
            ],
            provenance: Some(ProvenanceTag {
                transform: "ansi-strip".into(),
                version: 7,
            }),
            version: 42,
            source: kaijutsu_kernel::flows::OpSource::Local,
        };
        assert!(carries(&flow), "spans must ride the change feed");

        let mut message = capnp::message::Builder::new_default();
        write_event(message.init_root::<context_event::Builder>(), &flow);
        let reader = message
            .get_root_as_reader::<context_event::Reader>()
            .expect("root reads back");

        let context_event::SpansChanged(r) = reader.which().expect("known union arm") else {
            panic!("SpansChanged must land in its own union arm");
        };
        let r = r.expect("spansChanged payload");
        assert_eq!(
            r.get_provenance_transform().unwrap().to_str().unwrap(),
            "ansi-strip"
        );
        assert_eq!(r.get_provenance_version(), 7);

        let spans = r.get_style_spans().expect("span list");
        assert_eq!(spans.len(), 2);

        let indexed = spans.get(0);
        assert_eq!((indexed.get_start(), indexed.get_end()), (0, 4));
        assert_eq!((indexed.get_fg_kind(), indexed.get_fg_value()), (1, 9));
        assert_eq!((indexed.get_bg_kind(), indexed.get_bg_value()), (0, 0));
        assert_eq!(indexed.get_attrs(), (StyleAttrs::BOLD | StyleAttrs::UNDERLINE).0);

        let rgb = spans.get(1);
        assert_eq!(
            (rgb.get_fg_kind(), rgb.get_fg_value()),
            (2, 0x0012_3456),
            "truecolor packs as 0x00RRGGBB"
        );
        assert_eq!((rgb.get_bg_kind(), rgb.get_bg_value()), (1, 0));
        assert_eq!(rgb.get_attrs(), 0);
    }

    /// An untagged, unstyled reprojection is a real event: it says the block
    /// has no styling any more. It must ride the feed as an empty list and an
    /// empty transform, not be skipped.
    #[test]
    fn spans_changed_carries_an_empty_projection() {
        let context_id = ContextId::new();
        let block_id = BlockId::new(context_id, PrincipalId::new(), 1);
        let flow = BlockFlow::SpansChanged {
            context_id,
            block_id,
            style_spans: Vec::new(),
            provenance: None,
            version: 2,
            source: kaijutsu_kernel::flows::OpSource::Local,
        };

        let mut message = capnp::message::Builder::new_default();
        write_event(message.init_root::<context_event::Builder>(), &flow);
        let reader = message
            .get_root_as_reader::<context_event::Reader>()
            .expect("root reads back");

        let context_event::SpansChanged(r) = reader.which().expect("known union arm") else {
            panic!("SpansChanged must land in its own union arm");
        };
        let r = r.expect("spansChanged payload");
        assert_eq!(r.get_style_spans().unwrap().len(), 0);
        assert!(r.get_provenance_transform().unwrap().is_empty());
        assert_eq!(r.get_provenance_version(), 0);
    }
}
