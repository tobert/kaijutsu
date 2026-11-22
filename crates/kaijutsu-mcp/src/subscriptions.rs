//! Resource subscription hub, per-URI coalescer, and bounded per-context
//! event/turn ring buffers for MCP `2026-07-28` `subscriptions/listen`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kaijutsu_client::{ServerEvent, TurnCompletedStopReason};
use kaijutsu_types::ContextId;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// Default debounce window for coalescing rapid updates to the same URI.
pub const DEFAULT_COALESCE_WINDOW: Duration = Duration::from_millis(250);

/// Default maximum entries retained per context in each ring buffer.
pub const DEFAULT_RING_CAPACITY: usize = 256;

/// Capacity of the internal flushed-URI broadcast channel.
const BROADCAST_CAPACITY: usize = 256;

/// Subscribable `kaijutsu://` resource URI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SubscriptionUri {
    Doc(ContextId),
    Drift(ContextId),
    Turns(ContextId),
    Events(ContextId),
}

impl SubscriptionUri {
    /// Parse a `kaijutsu://` resource URI into a subscribable target.
    pub fn parse(uri: &str) -> Option<Self> {
        if let Some(id_str) = uri.strip_prefix("kaijutsu://docs/") {
            if id_str.contains('/') || id_str.is_empty() {
                return None;
            }
            return ContextId::parse(id_str).ok().map(Self::Doc);
        }
        if let Some(rest) = uri.strip_prefix("kaijutsu://contexts/") {
            let mut parts = rest.split('/');
            let id_str = parts.next()?;
            let suffix = parts.next()?;
            if parts.next().is_some() {
                return None;
            }
            let ctx_id = ContextId::parse(id_str).ok()?;
            return match suffix {
                "drift" => Some(Self::Drift(ctx_id)),
                "turns" => Some(Self::Turns(ctx_id)),
                "events" => Some(Self::Events(ctx_id)),
                _ => None,
            };
        }
        None
    }

    /// Format this target as its canonical `kaijutsu://` URI string.
    pub fn to_uri(&self) -> String {
        match self {
            Self::Doc(id) => format!("kaijutsu://docs/{}", id.to_hex()),
            Self::Drift(id) => format!("kaijutsu://contexts/{}/drift", id.to_hex()),
            Self::Turns(id) => format!("kaijutsu://contexts/{}/turns", id.to_hex()),
            Self::Events(id) => format!("kaijutsu://contexts/{}/events", id.to_hex()),
        }
    }

    /// The context this URI addresses.
    pub fn context_id(&self) -> ContextId {
        match self {
            Self::Doc(id) | Self::Drift(id) | Self::Turns(id) | Self::Events(id) => *id,
        }
    }
}

/// One recorded turn lifecycle event for `kaijutsu://contexts/{id}/turns`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnEventRecord {
    pub seq: u64,
    pub kind: String,
    pub turn_id: String,
    pub principal_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_block_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One recorded context event for `kaijutsu://contexts/{id}/events`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextEventRecord {
    pub seq: u64,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

/// Serializable drift entry returned by `kaijutsu://contexts/{id}/drift`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DriftRecord {
    pub id: u64,
    pub source_ctx: String,
    pub target_ctx: String,
    pub content: String,
    pub source_model: String,
    pub drift_kind: String,
    pub created_at: u64,
}

#[derive(Default)]
struct HubState {
    events: HashMap<ContextId, VecDeque<ContextEventRecord>>,
    turns: HashMap<ContextId, VecDeque<TurnEventRecord>>,
    local_drifts: HashMap<ContextId, Vec<DriftRecord>>,
    pending: HashSet<SubscriptionUri>,
    active_subscribers: HashMap<SubscriptionUri, usize>,
}

struct HubInner {
    coalesce_window: Duration,
    ring_capacity: usize,
    next_seq: AtomicU64,
    state: Mutex<HubState>,
    tx: broadcast::Sender<String>,
}

/// Cloneable hub that coalesces per-URI resource updates, buffers recent
/// context and turn events, and tracks active `subscriptions/listen` streams.
#[derive(Clone)]
pub struct SubscriptionHub {
    inner: Arc<HubInner>,
}

impl Default for SubscriptionHub {
    fn default() -> Self {
        Self::new()
    }
}

impl SubscriptionHub {
    /// Create a hub with the default 250ms coalesce window and 256-entry ring buffers.
    pub fn new() -> Self {
        Self::with_config(DEFAULT_COALESCE_WINDOW, DEFAULT_RING_CAPACITY)
    }

    /// Create a hub with a custom coalesce window and default ring capacity.
    pub fn with_coalesce_window(coalesce_window: Duration) -> Self {
        Self::with_config(coalesce_window, DEFAULT_RING_CAPACITY)
    }

    /// Create a hub with explicit coalesce window and per-context ring capacity.
    pub fn with_config(coalesce_window: Duration, ring_capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            inner: Arc::new(HubInner {
                coalesce_window,
                ring_capacity: ring_capacity.max(1),
                next_seq: AtomicU64::new(1),
                state: Mutex::new(HubState::default()),
                tx,
            }),
        }
    }

    /// Subscribe to flushed resource URI notifications.
    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.inner.tx.subscribe()
    }

    /// Register an active subscriber for `uris`. Counts decrement automatically
    /// when the returned [`SubscriptionGuard`] is dropped.
    pub fn register_subscriber(&self, uris: Vec<SubscriptionUri>) -> SubscriptionGuard {
        let mut unique = Vec::new();
        let mut seen = HashSet::new();
        for uri in uris {
            if seen.insert(uri) {
                unique.push(uri);
            }
        }
        {
            let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            for &uri in &unique {
                *state.active_subscribers.entry(uri).or_insert(0) += 1;
            }
        }
        SubscriptionGuard {
            inner: Arc::clone(&self.inner),
            uris: unique,
        }
    }

    /// Whether at least one active subscription covers `kaijutsu://contexts/{ctx_id}/drift`.
    pub fn has_drift_subscriber(&self, ctx_id: ContextId) -> bool {
        let state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .active_subscribers
            .get(&SubscriptionUri::Drift(ctx_id))
            .copied()
            .unwrap_or(0)
            > 0
    }

    /// Mark `uri` dirty in the coalescer. Multiple calls within `coalesce_window`
    /// fold into one broadcast emission when the window elapses.
    pub fn mark_dirty(&self, uri: SubscriptionUri) {
        let should_schedule = {
            let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            state.pending.insert(uri)
        };
        if !should_schedule {
            return;
        }
        let window = self.inner.coalesce_window;
        let inner_weak = Arc::downgrade(&self.inner);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                tokio::time::sleep(window).await;
                if let Some(inner) = inner_weak.upgrade() {
                    let was_pending = {
                        let mut state = inner.state.lock().unwrap_or_else(|e| e.into_inner());
                        state.pending.remove(&uri)
                    };
                    if was_pending {
                        let _ = inner.tx.send(uri.to_uri());
                    }
                }
            });
        } else {
            // Outside an async runtime (e.g. synchronous unit tests), flush immediately.
            let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.pending.remove(&uri) {
                let _ = self.inner.tx.send(uri.to_uri());
            }
        }
    }

    /// Record a context event and mark `kaijutsu://contexts/{ctx_id}/events` dirty.
    pub fn record_event(
        &self,
        ctx_id: ContextId,
        kind: impl Into<String>,
        block_id: Option<String>,
        detail: Option<serde_json::Value>,
    ) {
        let seq = self.inner.next_seq.fetch_add(1, Ordering::Relaxed);
        let record = ContextEventRecord {
            seq,
            kind: kind.into(),
            block_id,
            detail,
        };
        {
            let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            push_bounded(
                state.events.entry(ctx_id).or_default(),
                record,
                self.inner.ring_capacity,
            );
        }
        self.mark_dirty(SubscriptionUri::Events(ctx_id));
    }

    /// Record a turn lifecycle event into both the turns and events ring buffers,
    /// marking both `/turns` and `/events` dirty.
    #[allow(clippy::too_many_arguments)]
    pub fn record_turn_event(
        &self,
        ctx_id: ContextId,
        kind: impl Into<String>,
        turn_id: impl Into<String>,
        principal_id: impl Into<String>,
        stop_reason: Option<String>,
        output_block_id: Option<String>,
        error: Option<String>,
    ) {
        let kind = kind.into();
        let turn_id = turn_id.into();
        let principal_id = principal_id.into();
        let seq = self.inner.next_seq.fetch_add(1, Ordering::Relaxed);

        let turn_record = TurnEventRecord {
            seq,
            kind: kind.clone(),
            turn_id: turn_id.clone(),
            principal_id: principal_id.clone(),
            stop_reason: stop_reason.clone(),
            output_block_id: output_block_id.clone(),
            error: error.clone(),
        };
        let event_record = ContextEventRecord {
            seq,
            kind,
            block_id: output_block_id,
            detail: Some(serde_json::json!({
                "turn_id": turn_id,
                "principal_id": principal_id,
                "stop_reason": stop_reason,
                "error": error,
            })),
        };
        {
            let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            push_bounded(
                state.turns.entry(ctx_id).or_default(),
                turn_record,
                self.inner.ring_capacity,
            );
            push_bounded(
                state.events.entry(ctx_id).or_default(),
                event_record,
                self.inner.ring_capacity,
            );
        }
        self.mark_dirty(SubscriptionUri::Turns(ctx_id));
        self.mark_dirty(SubscriptionUri::Events(ctx_id));
    }

    /// Classify a kernel [`ServerEvent`] into the appropriate ring buffers and
    /// mark the affected URIs dirty in the coalescer.
    pub fn record_server_event(&self, event: &ServerEvent) {
        match event {
            ServerEvent::BlockInserted { context_id, block } => {
                self.record_event(
                    *context_id,
                    "block_inserted",
                    Some(block.id.to_key()),
                    Some(serde_json::json!({
                        "role": block.role.as_str(),
                        "kind": block.kind.as_str(),
                        "status": block.status.as_str(),
                    })),
                );
                self.mark_dirty(SubscriptionUri::Doc(*context_id));
                if block.kind == kaijutsu_types::BlockKind::Drift {
                    self.push_local_drift(
                        *context_id,
                        DriftRecord {
                            id: block.id.seq,
                            source_ctx: block
                                .source_context
                                .map(|c| c.to_hex())
                                .unwrap_or_default(),
                            target_ctx: context_id.to_hex(),
                            content: block.content.clone(),
                            source_model: block.source_model.clone().unwrap_or_default(),
                            drift_kind: block
                                .drift_kind
                                .map(drift_kind_wire)
                                .unwrap_or("push")
                                .to_string(),
                            created_at: block.created_at,
                        },
                    );
                }
            }
            ServerEvent::BlockStatusChanged {
                context_id,
                block_id,
                status,
            } => {
                self.record_event(
                    *context_id,
                    "block_status_changed",
                    Some(block_id.to_key()),
                    Some(serde_json::json!({ "status": status.as_str() })),
                );
                self.mark_dirty(SubscriptionUri::Doc(*context_id));
            }
            ServerEvent::BlockDeleted {
                context_id,
                block_id,
            } => {
                self.record_event(*context_id, "block_deleted", Some(block_id.to_key()), None);
                self.mark_dirty(SubscriptionUri::Doc(*context_id));
            }
            ServerEvent::BlockOutputChanged {
                context_id,
                block_id,
                ..
            } => {
                self.record_event(
                    *context_id,
                    "block_output_changed",
                    Some(block_id.to_key()),
                    None,
                );
                self.mark_dirty(SubscriptionUri::Doc(*context_id));
            }
            ServerEvent::BlockMetadataChanged {
                context_id,
                block_id,
                ..
            } => {
                self.record_event(
                    *context_id,
                    "block_metadata_changed",
                    Some(block_id.to_key()),
                    None,
                );
                self.mark_dirty(SubscriptionUri::Doc(*context_id));
            }
            ServerEvent::BlockMoved {
                context_id,
                block_id,
                after_id,
            } => {
                self.record_event(
                    *context_id,
                    "block_moved",
                    Some(block_id.to_key()),
                    Some(serde_json::json!({
                        "after_id": after_id.map(|id| id.to_key()),
                    })),
                );
                self.mark_dirty(SubscriptionUri::Doc(*context_id));
            }
            ServerEvent::TurnStarted {
                turn_id,
                context_id,
                principal_id,
            } => {
                self.record_turn_event(
                    *context_id,
                    "turn_started",
                    turn_id.to_string(),
                    principal_id.to_hex(),
                    None,
                    None,
                    None,
                );
            }
            ServerEvent::TurnCompleted {
                turn_id,
                context_id,
                principal_id,
                output_block_id,
                stop_reason,
                ..
            } => {
                self.record_turn_event(
                    *context_id,
                    "turn_completed",
                    turn_id.to_string(),
                    principal_id.to_hex(),
                    Some(stop_reason_wire(*stop_reason).to_string()),
                    output_block_id.map(|id| id.to_key()),
                    None,
                );
            }
            ServerEvent::TurnFailed {
                turn_id,
                context_id,
                principal_id,
                error,
                ..
            } => {
                self.record_turn_event(
                    *context_id,
                    "turn_failed",
                    turn_id.to_string(),
                    principal_id.to_hex(),
                    None,
                    None,
                    Some(error.clone()),
                );
            }
            _ => {}
        }
    }

    /// Mark `kaijutsu://contexts/{ctx_id}/drift` dirty in the coalescer.
    pub fn notify_drift(&self, ctx_id: ContextId) {
        self.mark_dirty(SubscriptionUri::Drift(ctx_id));
    }

    /// Queue a local drift record (used in Local mode and tests) and notify subscribers.
    pub fn push_local_drift(&self, ctx_id: ContextId, drift: DriftRecord) {
        {
            let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            state.local_drifts.entry(ctx_id).or_default().push(drift);
        }
        self.notify_drift(ctx_id);
    }

    /// Drain locally queued drifts for `ctx_id`.
    pub fn drain_local_drifts(&self, ctx_id: ContextId) -> Vec<DriftRecord> {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        state.local_drifts.remove(&ctx_id).unwrap_or_default()
    }

    /// Snapshot recent context events for `ctx_id` in chronological order.
    pub fn recent_events(&self, ctx_id: ContextId) -> Vec<ContextEventRecord> {
        let state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .events
            .get(&ctx_id)
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Snapshot recent turn lifecycle events for `ctx_id` in chronological order.
    pub fn recent_turns(&self, ctx_id: ContextId) -> Vec<TurnEventRecord> {
        let state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .turns
            .get(&ctx_id)
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default()
    }
}

/// RAII guard returned by [`SubscriptionHub::register_subscriber`].
/// Decrements active subscriber counts when dropped.
pub struct SubscriptionGuard {
    inner: Arc<HubInner>,
    uris: Vec<SubscriptionUri>,
}

impl Drop for SubscriptionGuard {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        for uri in &self.uris {
            if let Some(count) = state.active_subscribers.get_mut(uri) {
                if *count > 1 {
                    *count -= 1;
                } else {
                    state.active_subscribers.remove(uri);
                }
            }
        }
    }
}

fn push_bounded<T>(queue: &mut VecDeque<T>, item: T, cap: usize) {
    if queue.len() >= cap {
        queue.pop_front();
    }
    queue.push_back(item);
}

fn stop_reason_wire(reason: TurnCompletedStopReason) -> &'static str {
    match reason {
        TurnCompletedStopReason::EndTurn => "end_turn",
        TurnCompletedStopReason::CancelledSoft => "cancelled_soft",
        TurnCompletedStopReason::CancelledImmediate => "cancelled_immediate",
        TurnCompletedStopReason::MaxTokens => "max_tokens",
        TurnCompletedStopReason::MaxIterations => "max_iterations",
    }
}

fn drift_kind_wire(kind: kaijutsu_types::DriftKind) -> &'static str {
    match kind {
        kaijutsu_types::DriftKind::Push => "push",
        kaijutsu_types::DriftKind::Pull => "pull",
        kaijutsu_types::DriftKind::Merge => "merge",
        kaijutsu_types::DriftKind::Distill => "distill",
        kaijutsu_types::DriftKind::Notification => "notification",
        kaijutsu_types::DriftKind::Fork => "fork",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_parse_and_roundtrip() {
        let id = ContextId::new();
        let cases = [
            SubscriptionUri::Doc(id),
            SubscriptionUri::Drift(id),
            SubscriptionUri::Turns(id),
            SubscriptionUri::Events(id),
        ];
        for expected in cases {
            let s = expected.to_uri();
            assert_eq!(SubscriptionUri::parse(&s), Some(expected), "failed for {s}");
            assert_eq!(expected.context_id(), id);
        }

        // Reject invalid or unsubscribable URIs.
        assert_eq!(SubscriptionUri::parse("kaijutsu://docs"), None);
        assert_eq!(SubscriptionUri::parse("kaijutsu://docs/not-a-ctx"), None);
        assert_eq!(
            SubscriptionUri::parse(&format!("kaijutsu://docs/{}/extra", id.to_hex())),
            None
        );
        assert_eq!(
            SubscriptionUri::parse(&format!("kaijutsu://contexts/{}/unknown", id.to_hex())),
            None
        );
        assert_eq!(
            SubscriptionUri::parse(&format!("kaijutsu://contexts/{}/events/extra", id.to_hex())),
            None
        );
        assert_eq!(SubscriptionUri::parse("https://example.com/foo"), None);
    }

    #[tokio::test]
    async fn coalescer_folds_rapid_events_into_single_notification() {
        let hub = SubscriptionHub::with_coalesce_window(Duration::from_millis(20));
        let mut rx = hub.subscribe();
        let ctx = ContextId::new();

        for i in 0..10 {
            hub.record_event(ctx, "block_inserted", Some(format!("b-{i}")), None);
        }

        let uri = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("should flush within timeout")
            .expect("broadcast recv");
        assert_eq!(uri, SubscriptionUri::Events(ctx).to_uri());

        // No second notification should arrive for the same burst.
        let second = tokio::time::timeout(Duration::from_millis(60), rx.recv()).await;
        assert!(second.is_err(), "expected 1 coalesced notification, got {second:?}");
        assert_eq!(hub.recent_events(ctx).len(), 10);
    }

    #[test]
    fn ring_buffer_caps_at_configured_capacity() {
        let hub = SubscriptionHub::with_config(Duration::from_millis(20), 4);
        let ctx = ContextId::new();

        for i in 0..10 {
            hub.record_event(ctx, "block_inserted", Some(format!("b-{i}")), None);
        }
        let events = hub.recent_events(ctx);
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].block_id.as_deref(), Some("b-6"));
        assert_eq!(events[3].block_id.as_deref(), Some("b-9"));
        assert!(events.windows(2).all(|w| w[0].seq < w[1].seq));

        for i in 0..6 {
            hub.record_turn_event(
                ctx,
                "turn_started",
                format!("t-{i}"),
                "p-1",
                None,
                None,
                None,
            );
        }
        let turns = hub.recent_turns(ctx);
        assert_eq!(turns.len(), 4);
        assert_eq!(turns[0].turn_id, "t-2");
        assert_eq!(turns[3].turn_id, "t-5");
    }

    #[test]
    fn drift_subscriber_guard_tracks_refcount_raii() {
        let hub = SubscriptionHub::new();
        let ctx = ContextId::new();
        let other = ContextId::new();

        assert!(!hub.has_drift_subscriber(ctx));

        let g1 = hub.register_subscriber(vec![
            SubscriptionUri::Drift(ctx),
            SubscriptionUri::Drift(ctx), // duplicate in one call counts once
        ]);
        assert!(hub.has_drift_subscriber(ctx));
        assert!(!hub.has_drift_subscriber(other));

        let g2 = hub.register_subscriber(vec![SubscriptionUri::Drift(ctx)]);
        assert!(hub.has_drift_subscriber(ctx));

        drop(g1);
        assert!(
            hub.has_drift_subscriber(ctx),
            "second subscriber still holds the drift subscription"
        );

        drop(g2);
        assert!(
            !hub.has_drift_subscriber(ctx),
            "dropping last subscriber must clear has_drift_subscriber"
        );
    }

    #[tokio::test]
    async fn drift_block_inserted_marks_drift_uri_dirty() {
        let hub = SubscriptionHub::with_coalesce_window(Duration::from_millis(15));
        let mut rx = hub.subscribe();
        let ctx = ContextId::new();
        let src_ctx = ContextId::new();
        let principal = kaijutsu_types::PrincipalId::new();

        let mut block = kaijutsu_types::BlockSnapshot::text(
            kaijutsu_types::BlockId::new(ctx, principal, 7),
            None,
            kaijutsu_types::Role::System,
            "live drift content",
        );
        block.kind = kaijutsu_types::BlockKind::Drift;
        block.source_context = Some(src_ctx);

        hub.record_server_event(&ServerEvent::BlockInserted {
            context_id: ctx,
            block: Box::new(block),
        });

        let mut seen = HashSet::new();
        for _ in 0..3 {
            let uri = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("should flush within timeout")
                .expect("broadcast recv");
            seen.insert(uri);
        }

        let drift_uri = SubscriptionUri::Drift(ctx).to_uri();
        assert!(
            seen.contains(&drift_uri),
            "expected {drift_uri} to be marked dirty for BlockKind::Drift, saw {seen:?}"
        );
        let drained = hub.drain_local_drifts(ctx);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].content, "live drift content");
        assert_eq!(drained[0].source_ctx, src_ctx.to_hex());
    }
}
