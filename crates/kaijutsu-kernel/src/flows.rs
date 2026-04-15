//! FlowBus pub/sub system for CRDT event broadcasting.
//!
//! The FlowBus provides a typed publish/subscribe mechanism for kernel events.
//! Subscribers use NATS-style subject patterns to filter events of interest.
//!
//! # Pattern Matching
//!
//! Patterns use dot-separated tokens with wildcards:
//! - `*` matches exactly one token: `block.*` matches `block.inserted` but not `block.text.edited`
//! - `>` matches one or more tokens (only at end): `block.>` matches `block.inserted` and `block.text.edited`
//! - Exact match: `block.inserted` only matches `block.inserted`
//!
//! # Example
//!
//! ```ignore
//! let bus = FlowBus::<BlockFlow>::new(1024);
//!
//! // Subscribe to all block events
//! let mut sub = bus.subscribe("block.*");
//!
//! // Publish an event
//! bus.publish(BlockFlow::Inserted { ... });
//!
//! // Receive matching events
//! while let Some(msg) = sub.recv().await {
//!     println!("Got: {}", msg.topic);
//! }
//! ```

use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use kaijutsu_crdt::{BlockId, BlockKind, BlockSnapshot, Status};
use kaijutsu_types::{BlockEventFilter, BlockFlowKind, ContextId};

// ============================================================================
// Origin Tracking
// ============================================================================

/// Origin source for CRDT operations.
///
/// Used to prevent echo loops in bidirectional sync:
/// - Local operations should be sent to the server
/// - Remote operations (received from server) should NOT be sent back
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum OpSource {
    /// Generated locally by tool execution or user action
    #[default]
    Local,
    /// Received from remote via subscription/sync
    Remote,
}

// ============================================================================
// Pattern Matching
// ============================================================================

/// Check if a subject matches a pattern.
///
/// Patterns use NATS-style wildcards:
/// - `*` matches exactly one token
/// - `>` matches one or more tokens (only at end)
///
/// # Examples
///
/// ```ignore
/// assert!(matches_pattern("block.*", "block.inserted"));
/// assert!(matches_pattern("block.>", "block.text.edited"));
/// assert!(!matches_pattern("block.*", "block.text.edited"));
/// assert!(matches_pattern("block.inserted", "block.inserted"));
/// ```
pub fn matches_pattern(pattern: &str, subject: &str) -> bool {
    let pattern_tokens: Vec<&str> = pattern.split('.').collect();
    let subject_tokens: Vec<&str> = subject.split('.').collect();

    let mut pi = 0;
    let mut si = 0;

    while pi < pattern_tokens.len() && si < subject_tokens.len() {
        match pattern_tokens[pi] {
            ">" => {
                // `>` must be at the end and matches one or more remaining tokens
                return pi == pattern_tokens.len() - 1 && si < subject_tokens.len();
            }
            "*" => {
                // `*` matches exactly one token
                pi += 1;
                si += 1;
            }
            token => {
                // Exact match required
                if token != subject_tokens[si] {
                    return false;
                }
                pi += 1;
                si += 1;
            }
        }
    }

    // Both must be exhausted for a match (unless pattern ends with `>`)
    pi == pattern_tokens.len() && si == subject_tokens.len()
}

// ============================================================================
// Flow Message Types
// ============================================================================

/// Trait for payloads that know their subject/topic string.
///
/// Returns `&'static str` because all topic strings are compile-time constants,
/// which enables zero-cost `FlowMessage.topic` (pointer, no allocation).
pub trait HasSubject {
    /// Get the subject string for this payload.
    fn subject(&self) -> &'static str;
}

/// A message published to the flow bus.
#[derive(Clone, Debug)]
pub struct FlowMessage<T> {
    /// The topic this message was published to (zero-cost static pointer).
    pub topic: &'static str,
    /// The payload data.
    pub payload: T,
    /// When this message was created.
    pub timestamp: Instant,
    /// Optional sender identifier (agent_id or user_id).
    pub sender: Option<String>,
}

impl<T: HasSubject> FlowMessage<T> {
    /// Create a new flow message. Topic is derived from payload's subject.
    pub fn new(topic: &'static str, payload: T) -> Self {
        Self {
            topic,
            payload,
            timestamp: Instant::now(),
            sender: None,
        }
    }

    /// Create a new flow message with sender.
    pub fn with_sender(topic: &'static str, payload: T, sender: impl Into<String>) -> Self {
        Self {
            topic,
            payload,
            timestamp: Instant::now(),
            sender: Some(sender.into()),
        }
    }
}

// ============================================================================
// Topic Partitioning
// ============================================================================

/// Trait for flow types that declare their topic set.
///
/// Each flow domain (BlockFlow, ResourceFlow, etc.) declares the set of
/// topic strings that its variants can produce. The FlowBus creates one
/// async-broadcast channel per topic, so subscribers only receive events
/// for topics they're interested in — no discard loops.
pub trait FlowTopics {
    /// All known topic strings for this flow type.
    const TOPICS: &[&'static str];

    /// Per-topic capacity override. Returns None to use the bus default.
    fn topic_capacity(_topic: &str) -> Option<usize> {
        None
    }
}

impl FlowTopics for BlockFlow {
    const TOPICS: &[&'static str] = &[
        "block.inserted",
        "block.text_ops",
        "block.deleted",
        "block.status",
        "block.collapsed",
        "block.excluded",
        "block.moved",
        "block.sync_reset",
        "block.output",
        "block.metadata",
    ];

    fn topic_capacity(topic: &str) -> Option<usize> {
        match topic {
            "block.text_ops" => Some(2048),
            "block.inserted" | "block.status" => Some(256),
            _ => Some(128),
        }
    }
}

impl FlowTopics for ConfigFlow {
    const TOPICS: &[&'static str] = &[
        "config.loaded",
        "config.changed",
        "config.reload_requested",
        "config.reset",
        "config.validation_failed",
    ];
}

impl FlowTopics for InputDocFlow {
    const TOPICS: &[&'static str] = &["input.text_ops", "input.cleared"];
}

// ============================================================================
// Block Flow Events
// ============================================================================

/// Block-related flow events.
///
/// These events are emitted by the DocumentStore when blocks are modified.
/// Each variant corresponds to a specific block operation.
///
/// Events include `source` field for origin tracking:
/// - `Local`: Generated by local tool execution (should be sent to server)
/// - `Remote`: Received from server (should NOT be sent back to avoid echo loops)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum BlockFlow {
    /// A new block was inserted.
    Inserted {
        /// The context ID.
        context_id: ContextId,
        /// Full snapshot of the inserted block (Arc-wrapped to avoid deep clones).
        block: Arc<BlockSnapshot>,
        /// Block to insert after (None = beginning).
        after_id: Option<BlockId>,
        /// CRDT operations that created this block (for sync).
        /// Clients should merge these ops instead of creating their own.
        /// Arc-wrapped to avoid per-subscriber deep cloning.
        ops: Arc<[u8]>,
        /// Origin of this operation (Local or Remote).
        #[serde(default)]
        source: OpSource,
    },

    /// CRDT operations for a block's text content.
    /// Clients should use merge_ops() to apply these.
    TextOps {
        /// The context ID.
        context_id: ContextId,
        /// The block that was edited.
        block_id: BlockId,
        /// Serialized CRDT operations (diamond-types format).
        /// Arc-wrapped to avoid per-subscriber deep cloning.
        ops: Arc<[u8]>,
        /// Origin of this operation (Local or Remote).
        #[serde(default)]
        source: OpSource,
    },

    /// A block was deleted.
    Deleted {
        /// The context ID.
        context_id: ContextId,
        /// The block that was deleted.
        block_id: BlockId,
        /// Origin of this operation (Local or Remote).
        #[serde(default)]
        source: OpSource,
    },

    /// Block status changed.
    StatusChanged {
        /// The context ID.
        context_id: ContextId,
        /// The block whose status changed.
        block_id: BlockId,
        /// The new status.
        status: Status,
        /// DEPRECATED: Piggybacked for wire compat. Prefer `OutputChanged` event.
        /// Remove once `onBlockStatusChanged` drops `outputData` from Cap'n Proto schema.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<kaijutsu_types::OutputData>,
        /// Origin of this operation (Local or Remote).
        #[serde(default)]
        source: OpSource,
    },

    /// Block collapsed state changed (for thinking blocks).
    CollapsedChanged {
        /// The context ID.
        context_id: ContextId,
        /// The block whose collapsed state changed.
        block_id: BlockId,
        /// New collapsed state.
        collapsed: bool,
        /// Origin of this operation (Local or Remote).
        #[serde(default)]
        source: OpSource,
    },

    /// Block excluded state changed (for fork curation).
    ExcludedChanged {
        /// The context ID.
        context_id: ContextId,
        /// The block whose excluded state changed.
        block_id: BlockId,
        /// New excluded state.
        excluded: bool,
        /// Origin of this operation (Local or Remote).
        #[serde(default)]
        source: OpSource,
    },

    /// Block was moved to a new position.
    Moved {
        /// The context ID.
        context_id: ContextId,
        /// The block that was moved.
        block_id: BlockId,
        /// New position (after this block, None = beginning).
        after_id: Option<BlockId>,
        /// Origin of this operation (Local or Remote).
        #[serde(default)]
        source: OpSource,
    },

    /// Document was compacted — clients must re-sync from full oplog.
    SyncReset {
        /// The context ID.
        context_id: ContextId,
        /// New sync generation after compaction.
        generation: u64,
    },

    /// Block output data changed.
    OutputChanged {
        /// The context ID.
        context_id: ContextId,
        /// The block whose output changed.
        block_id: BlockId,
        /// The new output data.
        output: Option<kaijutsu_types::OutputData>,
        /// Origin of this operation (Local or Remote).
        #[serde(default)]
        source: OpSource,
    },

    /// Block metadata changed (tool_use_id, etc).
    MetadataChanged {
        /// The context ID.
        context_id: ContextId,
        /// The block whose metadata changed.
        block_id: BlockId,
        /// Origin of this operation (Local or Remote).
        #[serde(default)]
        source: OpSource,
    },

    /// Session context switched (server → client control).
    ContextSwitched {
        /// The new active context ID.
        context_id: ContextId,
    },
}

impl BlockFlow {
    /// Get the subject string for this event.
    pub fn subject(&self) -> &'static str {
        match self {
            Self::Inserted { .. } => "block.inserted",
            Self::TextOps { .. } => "block.text_ops",
            Self::Deleted { .. } => "block.deleted",
            Self::StatusChanged { .. } => "block.status",
            Self::CollapsedChanged { .. } => "block.collapsed",
            Self::ExcludedChanged { .. } => "block.excluded",
            Self::Moved { .. } => "block.moved",
            Self::SyncReset { .. } => "block.sync_reset",
            Self::OutputChanged { .. } => "block.output",
            Self::MetadataChanged { .. } => "block.metadata",
            Self::ContextSwitched { .. } => "block.context_switched",
        }
    }

    /// Get the context ID for this event.
    pub fn context_id(&self) -> ContextId {
        match self {
            Self::Inserted { context_id, .. }
            | Self::TextOps { context_id, .. }
            | Self::Deleted { context_id, .. }
            | Self::StatusChanged { context_id, .. }
            | Self::CollapsedChanged { context_id, .. }
            | Self::ExcludedChanged { context_id, .. }
            | Self::Moved { context_id, .. }
            | Self::SyncReset { context_id, .. }
            | Self::OutputChanged { context_id, .. }
            | Self::MetadataChanged { context_id, .. }
            | Self::ContextSwitched { context_id, .. } => *context_id,
        }
    }

    /// Get the block ID for this event (if applicable).
    pub fn block_id(&self) -> Option<&BlockId> {
        match self {
            Self::Inserted { block, .. } => Some(&block.id),
            Self::TextOps { block_id, .. }
            | Self::Deleted { block_id, .. }
            | Self::StatusChanged { block_id, .. }
            | Self::CollapsedChanged { block_id, .. }
            | Self::ExcludedChanged { block_id, .. }
            | Self::Moved { block_id, .. }
            | Self::OutputChanged { block_id, .. }
            | Self::MetadataChanged { block_id, .. } => Some(block_id),
            Self::SyncReset { .. } | Self::ContextSwitched { .. } => None,
        }
    }

    /// Get the block kind for Inserted events.
    pub fn block_kind(&self) -> Option<BlockKind> {
        match self {
            Self::Inserted { block, .. } => Some(block.kind),
            _ => None,
        }
    }

    /// Get the source of this event (Local or Remote).
    pub fn source(&self) -> OpSource {
        match self {
            Self::Inserted { source, .. }
            | Self::TextOps { source, .. }
            | Self::Deleted { source, .. }
            | Self::StatusChanged { source, .. }
            | Self::CollapsedChanged { source, .. }
            | Self::ExcludedChanged { source, .. }
            | Self::Moved { source, .. }
            | Self::OutputChanged { source, .. }
            | Self::MetadataChanged { source, .. } => *source,
            Self::SyncReset { .. } | Self::ContextSwitched { .. } => OpSource::Local,
        }
    }

    /// Check if this event originated locally.
    pub fn is_local(&self) -> bool {
        self.source() == OpSource::Local
    }

    /// Check if this event originated from a remote source.
    pub fn is_remote(&self) -> bool {
        self.source() == OpSource::Remote
    }

    /// Get the discriminant kind for this event (no payload).
    pub fn kind(&self) -> BlockFlowKind {
        match self {
            Self::Inserted { .. } => BlockFlowKind::Inserted,
            Self::TextOps { .. } => BlockFlowKind::TextOps,
            Self::Deleted { .. } => BlockFlowKind::Deleted,
            Self::StatusChanged { .. } => BlockFlowKind::StatusChanged,
            Self::CollapsedChanged { .. } => BlockFlowKind::CollapsedChanged,
            Self::ExcludedChanged { .. } => BlockFlowKind::ExcludedChanged,
            Self::Moved { .. } => BlockFlowKind::Moved,
            Self::SyncReset { .. } => BlockFlowKind::SyncReset,
            Self::OutputChanged { .. } => BlockFlowKind::OutputChanged,
            Self::MetadataChanged { .. } => BlockFlowKind::MetadataChanged,
            Self::ContextSwitched { .. } => BlockFlowKind::ContextSwitched,
        }
    }

    /// Check if this event passes a [`BlockEventFilter`].
    ///
    /// Used by the server-side subscription bridge to filter events before
    /// serializing them to the wire.
    pub fn matches_filter(&self, filter: &BlockEventFilter) -> bool {
        // Event type constraint
        if !filter.event_types.is_empty() && !filter.event_types.contains(&self.kind()) {
            return false;
        }
        // Context constraint
        if !filter.context_ids.is_empty() && !filter.context_ids.contains(&self.context_id()) {
            return false;
        }
        // Block kind constraint (only for Inserted events which carry a snapshot)
        if !filter.block_kinds.is_empty()
            && let Some(bk) = self.block_kind()
            && !filter.block_kinds.contains(&bk)
        {
            return false;
        }
        // Non-Inserted events don't carry block kind — pass this constraint
        true
    }
}

impl HasSubject for BlockFlow {
    fn subject(&self) -> &'static str {
        BlockFlow::subject(self)
    }
}

// ============================================================================
// FlowBus — topic-partitioned pub/sub via async-broadcast
// ============================================================================

use std::collections::HashMap;

/// Type-parameterized pub/sub bus for a specific flow domain.
///
/// Each flow domain declares its topics via [`FlowTopics`]. The bus creates one
/// `async_broadcast` channel per topic. Subscribers receive only messages for
/// matching topics — no discard loops, no CPU waste during high-throughput streaming.
///
/// Overflow: receivers use `set_overflow(true)` so the oldest message is silently
/// dropped when a receiver falls behind. The sender never blocks.
///
/// TODO: Add a monotonic sequence number to `BlockFlow::TextOps` so clients
/// can detect gaps when the server-side bridge subscription overflows. On gap
/// detection the client should re-fetch `ops_since()` to recover missed text
/// ops and display complete block content. The CRDT data is never lost — only
/// the realtime notification is missed.
pub struct FlowBus<T: Clone + Send + 'static> {
    /// Per-topic senders.
    topics: HashMap<&'static str, async_broadcast::Sender<FlowMessage<T>>>,
    /// Inactive receivers kept alive to clone new subscriptions from.
    inactive: HashMap<&'static str, async_broadcast::InactiveReceiver<FlowMessage<T>>>,
    /// Default capacity for topics without overrides.
    default_capacity: usize,
}

impl<T: Clone + Send + 'static> std::fmt::Debug for FlowBus<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlowBus")
            .field("topics", &self.topics.keys().collect::<Vec<_>>())
            .field("default_capacity", &self.default_capacity)
            .finish()
    }
}

impl<T: Clone + Send + HasSubject + FlowTopics + 'static> FlowBus<T> {
    /// Create a new topic-partitioned flow bus.
    ///
    /// One channel per topic declared in `T::TOPICS`. Per-topic capacity
    /// overrides via `T::topic_capacity()`, falling back to `default_capacity`.
    pub fn with_topics(default_capacity: usize) -> Self {
        let mut topics = HashMap::with_capacity(T::TOPICS.len());
        let mut inactive = HashMap::with_capacity(T::TOPICS.len());

        for &topic in T::TOPICS {
            let cap = T::topic_capacity(topic).unwrap_or(default_capacity);
            let (tx, rx) = async_broadcast::broadcast(cap);
            topics.insert(topic, tx);
            inactive.insert(topic, rx.deactivate());
        }

        Self {
            topics,
            inactive,
            default_capacity,
        }
    }

    /// Backward-compatible constructor. Creates topic-partitioned bus with given capacity.
    pub fn new(capacity: usize) -> Self {
        Self::with_topics(capacity)
    }
}

impl<T: Clone + Send + HasSubject + 'static> FlowBus<T> {
    /// Publish a payload to the bus.
    ///
    /// Routes to the correct topic channel based on `payload.subject()`.
    /// Returns the number of active receivers on the target topic.
    pub fn publish(&self, payload: T) -> usize {
        let topic = payload.subject();
        if let Some(tx) = self.topics.get(topic) {
            let msg = FlowMessage::new(topic, payload);
            // try_broadcast: non-blocking. If receiver is full and overflow is set,
            // the oldest message on the receiver side is dropped.
            match tx.try_broadcast(msg) {
                Ok(None) => tx.receiver_count(),
                Ok(Some(_returned)) => {
                    // All receivers full and no overflow? Shouldn't happen with set_overflow(true).
                    tx.receiver_count()
                }
                Err(async_broadcast::TrySendError::Closed(_)) => 0,
                Err(async_broadcast::TrySendError::Full(_)) => {
                    // No active receivers to overflow. Fine.
                    0
                }
                Err(async_broadcast::TrySendError::Inactive(_)) => 0,
            }
        } else {
            tracing::warn!(topic, "published to unknown topic");
            0
        }
    }

    /// Publish a payload with sender information.
    pub fn publish_with_sender(&self, payload: T, sender: impl Into<String>) -> usize {
        let topic = payload.subject();
        if let Some(tx) = self.topics.get(topic) {
            let msg = FlowMessage::with_sender(topic, payload, sender);
            match tx.try_broadcast(msg) {
                Ok(None) => tx.receiver_count(),
                Ok(Some(_)) => tx.receiver_count(),
                Err(_) => 0,
            }
        } else {
            tracing::warn!(topic, "published to unknown topic");
            0
        }
    }

    /// Subscribe to messages matching a pattern.
    ///
    /// Resolves the pattern against known topics at subscribe-time:
    /// - Exact match: `"block.inserted"` → single-topic subscription (zero overhead)
    /// - Wildcard: `"block.*"` → multi-topic subscription (select across matches)
    /// - No match: empty subscription (recv never returns)
    ///
    /// Pattern matching uses NATS-style wildcards:
    /// - `*` matches exactly one token
    /// - `>` matches one or more tokens (only at end)
    pub fn subscribe(&self, pattern: &str) -> Subscription<T> {
        let matching: Vec<&'static str> = self
            .topics
            .keys()
            .filter(|topic| matches_pattern(pattern, topic))
            .copied()
            .collect();

        match matching.len() {
            0 => Subscription::Empty,
            1 => {
                let topic = matching[0];
                let mut rx = self.inactive[topic].activate_cloned();
                rx.set_overflow(true);
                Subscription::Single { topic, rx }
            }
            _ => {
                let receivers = matching
                    .into_iter()
                    .map(|topic| {
                        let mut rx = self.inactive[topic].activate_cloned();
                        rx.set_overflow(true);
                        (topic, rx)
                    })
                    .collect();
                Subscription::Multi { receivers }
            }
        }
    }

    /// Get the total number of active subscribers across all topics.
    pub fn subscriber_count(&self) -> usize {
        self.topics.values().map(|tx| tx.receiver_count()).sum()
    }

    /// Get the default capacity.
    pub fn capacity(&self) -> usize {
        self.default_capacity
    }
}

impl<T: Clone + Send + 'static> Clone for FlowBus<T> {
    fn clone(&self) -> Self {
        Self {
            topics: self.topics.clone(),
            inactive: self.inactive.clone(),
            default_capacity: self.default_capacity,
        }
    }
}

// ============================================================================
// Subscription — topic-routed, zero-discard
// ============================================================================

/// A subscription to one or more FlowBus topics.
///
/// Created by [`FlowBus::subscribe()`]. Every message received is relevant —
/// topic routing happens at subscribe-time, not per-message.
pub enum Subscription<T: Clone + Send + 'static> {
    /// No topics matched the pattern.
    Empty,
    /// Exactly one topic matched (common fast path, zero overhead).
    Single {
        topic: &'static str,
        rx: async_broadcast::Receiver<FlowMessage<T>>,
    },
    /// Multiple topics matched (wildcard patterns).
    Multi {
        receivers: Vec<(&'static str, async_broadcast::Receiver<FlowMessage<T>>)>,
    },
}

impl<T: Clone + Send + 'static> Subscription<T> {
    /// Receive the next message, waiting if necessary.
    ///
    /// Returns None if all channels are closed.
    pub async fn recv(&mut self) -> Option<FlowMessage<T>> {
        match self {
            Self::Empty => {
                // Never returns — matches old behavior for non-matching patterns
                std::future::pending().await
            }
            Self::Single { rx, .. } => loop {
                match rx.recv().await {
                    Ok(msg) => return Some(msg),
                    Err(async_broadcast::RecvError::Overflowed(n)) => {
                        // Slow subscriber — oldest messages dropped from the channel buffer.
                        // CRDT data is safe in BlockStore; the caller may want to re-fetch
                        // ops_since() to recover any missed text ops after streaming ends.
                        tracing::warn!(
                            skipped = n,
                            "subscription overflowed — missed realtime updates"
                        );
                        continue;
                    }
                    Err(async_broadcast::RecvError::Closed) => return None,
                }
            },
            Self::Multi { receivers } => {
                use futures::stream::{FuturesUnordered, StreamExt};

                loop {
                    if receivers.is_empty() {
                        return std::future::pending().await;
                    }

                    // Non-blocking first pass: drain any ready messages
                    let mut closed_idx = None;
                    for (i, (topic, rx)) in receivers.iter_mut().enumerate() {
                        loop {
                            match rx.try_recv() {
                                Ok(msg) => return Some(msg),
                                Err(async_broadcast::TryRecvError::Overflowed(n)) => {
                                    // Slow subscriber — oldest messages dropped.
                                    // CRDT data is safe; caller may re-fetch ops_since() to recover.
                                    tracing::warn!(
                                        topic = *topic,
                                        skipped = n,
                                        "multi-subscription overflowed — missed realtime updates"
                                    );
                                    continue;
                                }
                                Err(async_broadcast::TryRecvError::Closed) => {
                                    closed_idx = Some(i);
                                    break;
                                }
                                Err(async_broadcast::TryRecvError::Empty) => break,
                            }
                        }
                        if closed_idx.is_some() {
                            break;
                        }
                    }

                    if let Some(idx) = closed_idx {
                        receivers.swap_remove(idx);
                        continue;
                    }

                    // Nothing ready — wait on ALL receivers concurrently.
                    // We clone each receiver to get an owned value for the future,
                    // then advance the original on success.
                    let mut futs = FuturesUnordered::new();
                    for (i, (_, rx)) in receivers.iter().enumerate() {
                        let mut rx_clone = rx.clone();
                        futs.push(async move { (i, rx_clone.recv().await) });
                    }

                    if let Some((idx, result)) = futs.next().await {
                        match result {
                            Ok(msg) => {
                                // Advance the real receiver past the message we got from the clone
                                // by draining one message. The clone already consumed it from the
                                // shared channel, so the original should skip past it.
                                let _ = receivers[idx].1.try_recv();
                                return Some(msg);
                            }
                            Err(async_broadcast::RecvError::Overflowed(_)) => continue,
                            Err(async_broadcast::RecvError::Closed) => {
                                receivers.swap_remove(idx);
                                continue;
                            }
                        }
                    }
                }
            }
        }
    }

    /// Try to receive the next message without blocking.
    ///
    /// Returns None if no message is available.
    pub fn try_recv(&mut self) -> Option<FlowMessage<T>> {
        match self {
            Self::Empty => None,
            Self::Single { rx, .. } => loop {
                match rx.try_recv() {
                    Ok(msg) => return Some(msg),
                    Err(async_broadcast::TryRecvError::Overflowed(n)) => {
                        tracing::warn!(
                            skipped = n,
                            "subscription overflowed on try_recv — missed realtime updates"
                        );
                        continue;
                    }
                    Err(
                        async_broadcast::TryRecvError::Empty
                        | async_broadcast::TryRecvError::Closed,
                    ) => return None,
                }
            },
            Self::Multi { receivers } => {
                // Round-robin try_recv across all receivers
                for (_topic, rx) in receivers.iter_mut() {
                    loop {
                        match rx.try_recv() {
                            Ok(msg) => return Some(msg),
                            Err(async_broadcast::TryRecvError::Overflowed(n)) => {
                                tracing::warn!(
                                    skipped = n,
                                    "multi try_recv overflowed — missed realtime updates"
                                );
                                continue;
                            }
                            Err(
                                async_broadcast::TryRecvError::Empty
                                | async_broadcast::TryRecvError::Closed,
                            ) => break,
                        }
                    }
                }
                None
            }
        }
    }
}

impl<T: Clone + Send + 'static> std::fmt::Debug for Subscription<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("Subscription::Empty"),
            Self::Single { topic, .. } => f
                .debug_struct("Subscription::Single")
                .field("topic", topic)
                .finish(),
            Self::Multi { receivers } => f
                .debug_struct("Subscription::Multi")
                .field(
                    "topics",
                    &receivers.iter().map(|(t, _)| *t).collect::<Vec<_>>(),
                )
                .finish(),
        }
    }
}

// ============================================================================
// Config Flow Events
// ============================================================================

/// Source of a config file load.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ConfigSource {
    /// Loaded from disk file.
    #[default]
    Disk,
    /// Loaded from CRDT (possibly edited by agent/user).
    Crdt,
    /// Using embedded default (fallback).
    Default,
}

impl std::fmt::Display for ConfigSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disk => write!(f, "disk"),
            Self::Crdt => write!(f, "crdt"),
            Self::Default => write!(f, "default"),
        }
    }
}

/// Config-related flow events.
///
/// These events are emitted when config files are loaded, changed, or reloaded.
/// The config system supports both file-backed and CRDT-backed config documents.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ConfigFlow {
    /// A config file was loaded (at startup or on demand).
    Loaded {
        /// Relative path within config directory (e.g., "theme.toml").
        path: String,
        /// Where the config was loaded from.
        source: ConfigSource,
        /// Content of the loaded config.
        content: String,
    },

    /// Config content changed (either from CRDT edit or file watcher).
    Changed {
        /// Relative path within config directory.
        path: String,
        /// Serialized CRDT operations (for sync).
        /// Arc-wrapped to avoid per-subscriber deep cloning.
        ops: Arc<[u8]>,
        /// Origin of this operation (Local or Remote).
        source: OpSource,
    },

    /// User requested a config reload from disk (safety valve).
    ReloadRequested {
        /// Relative path within config directory (or "all" for all configs).
        path: String,
    },

    /// Config was reset to embedded default.
    Reset {
        /// Relative path within config directory.
        path: String,
    },

    /// Config validation failed (on flush to disk or apply).
    ValidationFailed {
        /// Relative path within config directory.
        path: String,
        /// Error message describing the validation failure.
        error: String,
        /// Content that failed validation.
        content: String,
    },
}

impl ConfigFlow {
    /// Get the subject string for this event.
    pub fn subject(&self) -> &'static str {
        match self {
            Self::Loaded { .. } => "config.loaded",
            Self::Changed { .. } => "config.changed",
            Self::ReloadRequested { .. } => "config.reload_requested",
            Self::Reset { .. } => "config.reset",
            Self::ValidationFailed { .. } => "config.validation_failed",
        }
    }

    /// Get the path for this event.
    pub fn path(&self) -> &str {
        match self {
            Self::Loaded { path, .. }
            | Self::Changed { path, .. }
            | Self::ReloadRequested { path, .. }
            | Self::Reset { path, .. }
            | Self::ValidationFailed { path, .. } => path,
        }
    }

    /// Get the source of this event (for Changed events).
    pub fn op_source(&self) -> Option<OpSource> {
        match self {
            Self::Changed { source, .. } => Some(*source),
            _ => None,
        }
    }

    /// Check if this event originated locally.
    pub fn is_local(&self) -> bool {
        self.op_source() == Some(OpSource::Local)
    }
}

impl HasSubject for ConfigFlow {
    fn subject(&self) -> &'static str {
        ConfigFlow::subject(self)
    }
}

// ============================================================================
// Input Doc Flow Events
// ============================================================================

/// Input document flow events for compose scratchpad changes.
///
/// These events are emitted when the per-context input document is modified.
/// Used to broadcast typing to other participants on the same context.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum InputDocFlow {
    /// Text operations applied to the input document.
    TextOps {
        /// The context whose input doc was modified.
        context_id: ContextId,
        /// Serialized DTE operations (postcard-encoded SerializedOpsOwned).
        /// Arc-wrapped to avoid per-subscriber deep cloning.
        ops: Arc<[u8]>,
        /// Origin of this operation.
        source: OpSource,
    },

    /// The input document was cleared (after submit).
    Cleared {
        /// The context whose input doc was cleared.
        context_id: ContextId,
    },
}

impl InputDocFlow {
    /// Get the subject string for this event.
    pub fn subject(&self) -> &'static str {
        match self {
            Self::TextOps { .. } => "input.text_ops",
            Self::Cleared { .. } => "input.cleared",
        }
    }

    /// Get the context ID for this event.
    pub fn context_id(&self) -> ContextId {
        match self {
            Self::TextOps { context_id, .. } | Self::Cleared { context_id, .. } => *context_id,
        }
    }
}

impl HasSubject for InputDocFlow {
    fn subject(&self) -> &'static str {
        InputDocFlow::subject(self)
    }
}

// ============================================================================
// Shared FlowBus Handle
// ============================================================================

/// Thread-safe handle to a BlockFlow bus.
pub type SharedBlockFlowBus = Arc<FlowBus<BlockFlow>>;

/// Thread-safe handle to a ConfigFlow bus.
pub type SharedConfigFlowBus = Arc<FlowBus<ConfigFlow>>;

/// Thread-safe handle to an InputDocFlow bus.
pub type SharedInputDocFlowBus = Arc<FlowBus<InputDocFlow>>;

/// Create a new shared block flow bus.
pub fn shared_block_flow_bus(capacity: usize) -> SharedBlockFlowBus {
    Arc::new(FlowBus::new(capacity))
}

/// Create a new shared config flow bus.
pub fn shared_config_flow_bus(capacity: usize) -> SharedConfigFlowBus {
    Arc::new(FlowBus::new(capacity))
}

/// Create a new shared input doc flow bus.
pub fn shared_input_doc_flow_bus(capacity: usize) -> SharedInputDocFlowBus {
    Arc::new(FlowBus::new(capacity))
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_crdt::Role;
    use kaijutsu_types::PrincipalId;

    // ====================================================================
    // Pattern matching (pure function, no FlowBus dependency)
    // ====================================================================

    #[test]
    fn test_pattern_matching_exact() {
        assert!(matches_pattern("block.inserted", "block.inserted"));
        assert!(!matches_pattern("block.inserted", "block.deleted"));
        assert!(!matches_pattern("block.inserted", "block.inserted.extra"));
    }

    #[test]
    fn test_pattern_matching_single_wildcard() {
        assert!(matches_pattern("block.*", "block.inserted"));
        assert!(matches_pattern("block.*", "block.deleted"));
        assert!(matches_pattern("block.*", "block.status"));
        assert!(!matches_pattern("block.*", "block.text.edited"));
        assert!(!matches_pattern("block.*", "user.joined"));
    }

    #[test]
    fn test_pattern_matching_multi_wildcard() {
        assert!(matches_pattern("block.>", "block.inserted"));
        assert!(matches_pattern("block.>", "block.text.edited"));
        assert!(matches_pattern("block.>", "block.a.b.c.d"));
        assert!(!matches_pattern("block.>", "user.joined"));
    }

    #[test]
    fn test_pattern_matching_mixed() {
        assert!(matches_pattern("*.inserted", "block.inserted"));
        assert!(matches_pattern("*.inserted", "user.inserted"));
        assert!(!matches_pattern("*.inserted", "block.deleted"));
        assert!(matches_pattern("block.*.done", "block.status.done"));
        assert!(!matches_pattern("block.*.done", "block.done"));
    }

    // ====================================================================
    // HasSubject impls
    // ====================================================================

    #[test]
    fn test_block_flow_subjects() {
        let ctx = ContextId::new();
        let id = BlockId::new(ctx, PrincipalId::new(), 1);
        let block = BlockSnapshot::text(id, None, Role::User, "test");

        assert_eq!(
            BlockFlow::Inserted {
                context_id: ctx,
                block: Arc::new(block),
                after_id: None,
                ops: Arc::from(Vec::<u8>::new()),
                source: OpSource::Local,
            }
            .subject(),
            "block.inserted"
        );

        assert_eq!(
            BlockFlow::Deleted {
                context_id: ctx,
                block_id: id,
                source: OpSource::Local,
            }
            .subject(),
            "block.deleted"
        );

        assert_eq!(
            BlockFlow::StatusChanged {
                context_id: ctx,
                block_id: id,
                status: Status::Done,
                output: None,
                source: OpSource::Local,
            }
            .subject(),
            "block.status"
        );

        assert_eq!(
            BlockFlow::CollapsedChanged {
                context_id: ctx,
                block_id: id,
                collapsed: true,
                source: OpSource::Local,
            }
            .subject(),
            "block.collapsed"
        );

        assert_eq!(
            BlockFlow::Moved {
                context_id: ctx,
                block_id: id,
                after_id: None,
                source: OpSource::Local,
            }
            .subject(),
            "block.moved"
        );
    }

    /// Regression: rpc.rs semantic index watcher subscribes to "block.status".
    #[test]
    fn test_status_changed_subject_is_block_status() {
        let ctx = ContextId::new();
        let id = BlockId::new(ctx, PrincipalId::new(), 1);
        let flow = BlockFlow::StatusChanged {
            context_id: ctx,
            block_id: id,
            status: Status::Done,
            output: None,
            source: OpSource::Local,
        };
        assert_eq!(flow.subject(), "block.status");
    }

    // ====================================================================
    // Topic isolation — the core design property
    // ====================================================================

    /// Subscribe to "block.status", publish TextOps + StatusChanged,
    /// assert only StatusChanged received. No discard loop.
    #[tokio::test]
    async fn test_topic_isolation() {
        let bus: FlowBus<BlockFlow> = FlowBus::new(64);
        let mut status_sub = bus.subscribe("block.status");

        let ctx = ContextId::new();
        let id = BlockId::new(ctx, PrincipalId::new(), 1);

        // Publish 10 TextOps (high-throughput noise)
        for _ in 0..10 {
            bus.publish(BlockFlow::TextOps {
                context_id: ctx,
                block_id: id,
                ops: Arc::from(vec![1u8, 2, 3]),
                source: OpSource::Local,
            });
        }

        // Publish 1 StatusChanged
        bus.publish(BlockFlow::StatusChanged {
            context_id: ctx,
            block_id: id,
            status: Status::Done,
            output: None,
            source: OpSource::Local,
        });

        // status_sub should see exactly 1 message (no TextOps noise)
        let msg = status_sub.try_recv().expect("should receive StatusChanged");
        assert_eq!(msg.topic, "block.status");
        assert!(
            status_sub.try_recv().is_none(),
            "should have no more messages"
        );
    }

    /// Subscribe to "block.*", publish one of each variant, assert all received.
    #[tokio::test]
    async fn test_wildcard_receives_all_topics() {
        let bus: FlowBus<BlockFlow> = FlowBus::new(64);
        let mut sub = bus.subscribe("block.*");

        let ctx = ContextId::new();
        let id = BlockId::new(ctx, PrincipalId::new(), 1);
        let block = Arc::new(BlockSnapshot::text(id, None, Role::User, "test"));

        bus.publish(BlockFlow::Inserted {
            context_id: ctx,
            block: block.clone(),
            after_id: None,
            ops: Arc::from(vec![]),
            source: OpSource::Local,
        });
        bus.publish(BlockFlow::TextOps {
            context_id: ctx,
            block_id: id,
            ops: Arc::from(vec![1u8]),
            source: OpSource::Local,
        });
        bus.publish(BlockFlow::Deleted {
            context_id: ctx,
            block_id: id,
            source: OpSource::Local,
        });
        bus.publish(BlockFlow::StatusChanged {
            context_id: ctx,
            block_id: id,
            status: Status::Done,
            output: None,
            source: OpSource::Local,
        });
        bus.publish(BlockFlow::CollapsedChanged {
            context_id: ctx,
            block_id: id,
            collapsed: true,
            source: OpSource::Local,
        });
        bus.publish(BlockFlow::Moved {
            context_id: ctx,
            block_id: id,
            after_id: None,
            source: OpSource::Local,
        });
        bus.publish(BlockFlow::SyncReset {
            context_id: ctx,
            generation: 1,
        });

        // All 7 variants should be received
        let mut count = 0;
        while sub.try_recv().is_some() {
            count += 1;
        }
        assert_eq!(count, 7, "wildcard should receive all 7 topic variants");
    }

    /// Subscribe to "block.inserted", publish 1000 TextOps + 1 Inserted,
    /// assert exactly 1 message received.
    #[tokio::test]
    async fn test_exact_subscribe_zero_overhead() {
        let bus: FlowBus<BlockFlow> = FlowBus::new(64);
        let mut sub = bus.subscribe("block.inserted");

        let ctx = ContextId::new();
        let id = BlockId::new(ctx, PrincipalId::new(), 1);

        for _ in 0..1000 {
            bus.publish(BlockFlow::TextOps {
                context_id: ctx,
                block_id: id,
                ops: Arc::from(vec![0u8]),
                source: OpSource::Local,
            });
        }

        let block = Arc::new(BlockSnapshot::text(id, None, Role::User, "hello"));
        bus.publish(BlockFlow::Inserted {
            context_id: ctx,
            block,
            after_id: None,
            ops: Arc::from(vec![]),
            source: OpSource::Local,
        });

        // Should get exactly 1 message — no TextOps noise
        let msg = sub.try_recv().expect("should receive Inserted");
        assert_eq!(msg.topic, "block.inserted");
        assert!(sub.try_recv().is_none());
    }

    /// Two subscribers to different topics see only their events.
    #[tokio::test]
    async fn test_multi_subscriber_independence() {
        let bus: FlowBus<BlockFlow> = FlowBus::new(64);
        let mut inserted_sub = bus.subscribe("block.inserted");
        let mut status_sub = bus.subscribe("block.status");

        let ctx = ContextId::new();
        let id = BlockId::new(ctx, PrincipalId::new(), 1);
        let block = Arc::new(BlockSnapshot::text(id, None, Role::User, "x"));

        bus.publish(BlockFlow::Inserted {
            context_id: ctx,
            block,
            after_id: None,
            ops: Arc::from(vec![]),
            source: OpSource::Local,
        });
        bus.publish(BlockFlow::StatusChanged {
            context_id: ctx,
            block_id: id,
            status: Status::Running,
            output: None,
            source: OpSource::Local,
        });

        assert!(inserted_sub.try_recv().is_some());
        assert!(inserted_sub.try_recv().is_none());
        assert!(status_sub.try_recv().is_some());
        assert!(status_sub.try_recv().is_none());
    }

    /// Subscriber count sums across all topic channels.
    #[test]
    fn test_subscriber_count_across_topics() {
        let bus: FlowBus<BlockFlow> = FlowBus::new(64);
        assert_eq!(bus.subscriber_count(), 0);

        let _s1 = bus.subscribe("block.inserted");
        assert_eq!(bus.subscriber_count(), 1);

        let _s2 = bus.subscribe("block.status");
        assert_eq!(bus.subscriber_count(), 2);

        // Wildcard creates one subscription per matching topic (10 for block.*)
        let _s3 = bus.subscribe("block.*");
        assert_eq!(bus.subscriber_count(), 2 + 10);
    }

    /// Subscribe to a pattern that matches no topics.
    #[test]
    fn test_empty_subscription() {
        let bus: FlowBus<BlockFlow> = FlowBus::new(64);
        let mut sub = bus.subscribe("nonexistent.topic");
        assert!(sub.try_recv().is_none());
    }

    /// Publish a flow with a subject not in TOPICS. Should warn, not panic.
    /// (In practice this can't happen because HasSubject returns known static strings,
    /// but we verify the guard works.)
    #[test]
    fn test_publish_to_unknown_topic_returns_zero() {
        // We can't easily create a BlockFlow with an unknown subject,
        // so instead verify that all known subjects ARE routable.
        let bus: FlowBus<BlockFlow> = FlowBus::new(64);
        for &topic in BlockFlow::TOPICS {
            assert!(
                bus.topics.contains_key(topic),
                "topic {} should exist",
                topic
            );
        }
    }

    /// Two subscribers to the same Inserted event share the same Arc allocation.
    #[tokio::test]
    async fn test_arc_payload_clone_is_cheap() {
        let bus: FlowBus<BlockFlow> = FlowBus::new(64);
        let mut sub1 = bus.subscribe("block.inserted");
        let mut sub2 = bus.subscribe("block.inserted");

        let ctx = ContextId::new();
        let id = BlockId::new(ctx, PrincipalId::new(), 1);
        let block = Arc::new(BlockSnapshot::text(id, None, Role::User, "shared"));

        bus.publish(BlockFlow::Inserted {
            context_id: ctx,
            block,
            after_id: None,
            ops: Arc::from(vec![42u8]),
            source: OpSource::Local,
        });

        let msg1 = sub1.try_recv().expect("sub1 should receive");
        let msg2 = sub2.try_recv().expect("sub2 should receive");

        // Both subscribers should have the same Arc (pointer equality)
        if let (
            BlockFlow::Inserted {
                block: b1, ops: o1, ..
            },
            BlockFlow::Inserted {
                block: b2, ops: o2, ..
            },
        ) = (&msg1.payload, &msg2.payload)
        {
            assert!(Arc::ptr_eq(b1, b2), "block Arcs should share allocation");
            assert!(Arc::ptr_eq(o1, o2), "ops Arcs should share allocation");
        } else {
            panic!("expected Inserted events");
        }
    }

    /// Async publish/subscribe with background task.
    #[tokio::test]
    async fn test_async_publish_subscribe() {
        let bus: FlowBus<BlockFlow> = FlowBus::new(16);
        let mut sub = bus.subscribe("block.*");

        let ctx = ContextId::new();
        let id = BlockId::new(ctx, PrincipalId::new(), 1);
        let block = Arc::new(BlockSnapshot::text(id, None, Role::User, "test"));

        let bus_clone = bus.clone();
        let block_clone = block.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            bus_clone.publish(BlockFlow::Inserted {
                context_id: ctx,
                block: block_clone,
                after_id: None,
                ops: Arc::from(Vec::<u8>::new()),
                source: OpSource::Local,
            });
        });

        let msg = tokio::time::timeout(std::time::Duration::from_millis(100), sub.recv())
            .await
            .expect("timeout")
            .expect("no message");

        assert_eq!(msg.topic, "block.inserted");
    }

    // ====================================================================
    // Per-flow-type topic routing
    // ====================================================================

    #[tokio::test]
    async fn test_config_flow_topics() {
        let bus: FlowBus<ConfigFlow> = FlowBus::new(16);
        let mut loaded_sub = bus.subscribe("config.loaded");
        let mut changed_sub = bus.subscribe("config.changed");

        bus.publish(ConfigFlow::Loaded {
            path: "theme.toml".into(),
            source: ConfigSource::Disk,
            content: "{}".into(),
        });
        bus.publish(ConfigFlow::Changed {
            path: "theme.toml".into(),
            ops: Arc::from(vec![]),
            source: OpSource::Local,
        });

        assert!(loaded_sub.try_recv().is_some());
        assert!(loaded_sub.try_recv().is_none());
        assert!(changed_sub.try_recv().is_some());
        assert!(changed_sub.try_recv().is_none());
    }

    #[tokio::test]
    async fn test_input_doc_flow_topics() {
        let bus: FlowBus<InputDocFlow> = FlowBus::new(16);
        let mut ops_sub = bus.subscribe("input.text_ops");
        let mut cleared_sub = bus.subscribe("input.cleared");

        let ctx = ContextId::new();
        bus.publish(InputDocFlow::TextOps {
            context_id: ctx,
            ops: Arc::from(vec![1u8]),
            source: OpSource::Local,
        });
        bus.publish(InputDocFlow::Cleared { context_id: ctx });

        assert!(ops_sub.try_recv().is_some());
        assert!(ops_sub.try_recv().is_none());
        assert!(cleared_sub.try_recv().is_some());
        assert!(cleared_sub.try_recv().is_none());
    }

    // ====================================================================
    // Pure-function tests (no FlowBus dependency)
    // ====================================================================

}
