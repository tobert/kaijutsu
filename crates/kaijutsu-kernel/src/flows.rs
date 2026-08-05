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
use kaijutsu_types::{BlockEventFilter, BlockFlowKind, ContextId, PrincipalId};

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
    /// Optional sender identifier (principal_id or user_id).
    pub sender: Option<String>,
    /// **Per-subscription, per-lane** monotonic sequence number, stamped when
    /// the message is enqueued for one particular subscriber. Two subscribers
    /// of the same publish see different `seq` values; within one
    /// subscription's ordered lane it counts 1, 2, 3, … with no holes.
    ///
    /// The contract (see [`TopicClass`]): a gap on an **ordered** topic can
    /// only mean the subscription was terminated or replaced — never that an
    /// event was silently lost. The **timing** lane counts separately and a gap
    /// there is honest and expected: it says beats were missed, which is what
    /// that lane promises. `0` on a message that never went through a queue.
    pub seq: u64,
}

impl<T: HasSubject> FlowMessage<T> {
    /// Create a new flow message. Topic is derived from payload's subject.
    pub fn new(topic: &'static str, payload: T) -> Self {
        Self {
            topic,
            payload,
            timestamp: Instant::now(),
            sender: None,
            seq: 0,
        }
    }

    /// Create a new flow message with sender.
    pub fn with_sender(topic: &'static str, payload: T, sender: impl Into<String>) -> Self {
        Self {
            topic,
            payload,
            timestamp: Instant::now(),
            sender: Some(sender.into()),
            seq: 0,
        }
    }
}

// ============================================================================
// Topic Partitioning
// ============================================================================

/// How a topic behaves when a subscriber cannot keep up.
///
/// Two lanes, and the split is doctrine rather than tuning:
///
/// * [`TopicClass::Ordered`] — **lossless-or-terminated**. Events land in the
///   subscription's queue in global publish order and are delivered in that
///   order. A live subscriber NEVER silently misses one: when its queue fills,
///   the subscription is *terminated* with an explicit signal
///   ([`FlowRecv::Terminated`]) so the consumer knows it has an incomplete view
///   and must resync. Publishers never block and other subscribers are never
///   slowed — the slow one is the only casualty. This is what every
///   client-facing block/turn/content stream needs (2026-08-05: four live
///   incidents where dropped `TurnCompleted`, text ops, and terminal
///   `BlockStatusChanged` patches produced hung prompts, swallowed content,
///   and perpetually-"running" tool calls).
///
/// * [`TopicClass::Timing`] — **latency-first, drop-oldest, never terminating**.
///   Musical time keeps its own semantics (`docs/midi.md`, "The one timebase"):
///   a missed beat is missed, never queued for later and never replayed. A
///   backed-up sink wants the *newest* beat reference, not a faithful history
///   of stale ones — and it already rejects stale timing data on its own
///   ladder. These events also ride their own queue inside a subscription and
///   are drained first, so a text firehose can never delay a beat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TopicClass {
    /// Lossless in publish order; terminate the subscription on overflow.
    Ordered,
    /// Latency-first; drop the oldest on overflow and keep going.
    Timing,
}

/// Trait for flow types that declare their topic set.
///
/// Each flow domain (BlockFlow, TurnFlow, …) declares the set of topic strings
/// its variants can produce. `FlowBus` routes a publish to exactly the
/// subscriptions whose pattern matches, so subscribers only receive events they
/// asked for — no discard loops.
pub trait FlowTopics {
    /// All known topic strings for this flow type.
    const TOPICS: &[&'static str];

    /// Delivery class for a topic. Defaults to the lossless lane; override for
    /// the timing lane (see [`TopicClass`]).
    fn topic_class(_topic: &str) -> TopicClass {
        TopicClass::Ordered
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
        "block.context_switched",
        "block.render_cue",
        "block.beat_sync",
    ];

    fn topic_class(topic: &str) -> TopicClass {
        match topic {
            // Directives on the musical timebase, not block-log events. They
            // carry emission wallclock stamps and a sink back-dates / rejects
            // stale ones, so queueing them is worse than dropping them
            // (docs/midi.md "The one timebase"). Never delayed by the
            // block firehose, never a reason to terminate a subscription.
            "block.render_cue" | "block.beat_sync" => TopicClass::Timing,
            _ => TopicClass::Ordered,
        }
    }
}

impl FlowTopics for InputDocFlow {
    const TOPICS: &[&'static str] = &["input.text_ops", "input.cleared"];
}

impl FlowTopics for TurnFlow {
    const TOPICS: &[&'static str] = &["turn.requested", "turn.completed", "turn.failed"];
}

impl FlowTopics for EditorFlow {
    const TOPICS: &[&'static str] = &["editor.state_changed", "editor.closed"];
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
        /// Per-context monotonic sequence number (M2-B2). Lets clients
        /// detect dropped events when the broadcast channel overflows;
        /// CRDT data is never lost — only the realtime notification.
        #[serde(default)]
        seq_num: u64,
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

    /// The document's CRDT oplog was compacted (generation bump) — clients
    /// must re-sync from the full oplog. Not to be confused with the
    /// (deleted) LLM auto-summarization feature — this is oplog compaction,
    /// a still-live, unrelated mechanism.
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

    /// Block scalar metadata changed (exit_code, stderr, content_type, …).
    ///
    /// Carries the block's current metadata snapshot so subscribers can apply
    /// it directly without a refetch. Frontier-independent — see
    /// [`kaijutsu_types::BlockMetadata`].
    MetadataChanged {
        /// The context ID.
        context_id: ContextId,
        /// The block whose metadata changed.
        block_id: BlockId,
        /// Current scalar metadata for the block.
        metadata: kaijutsu_types::BlockMetadata,
        /// Origin of this operation (Local or Remote).
        #[serde(default)]
        source: OpSource,
    },

    /// Session context switched (server → client control).
    ContextSwitched {
        /// The new active context ID.
        context_id: ContextId,
    },

    /// Render a cue (`kj play`, later the track render seam; docs/pcm.md,
    /// docs/midi.md "Render is a wire cue"). A kernel *directive*, not a
    /// block-log event — it carries no `block_id` because it targets no
    /// block. Rides `BlockFlow` anyway because the server→client fan-out this
    /// event needs (every attached client, unfiltered) is exactly what the
    /// existing `BlockEvents` subscription bridges already do; a parallel bus
    /// would just duplicate that plumbing (docs/pcm.md "The wire").
    RenderCue {
        /// The context that issued the directive (reserved for future
        /// per-listener routing; the standalone slice forwards unconditionally).
        context_id: ContextId,
        /// The cue to render (mime-keyed; the sink dispatches on `cue.mime`).
        cue: kaijutsu_audio::RenderCue,
    },
    /// A low-rate beat reference for a sink's continuous timebase (the metronome
    /// phasor; docs/midi.md "The relative-lead timebase, analyzed"). Like
    /// `RenderCue` it is a kernel *directive*, not a block-log event (no
    /// `block_id`), and rides `BlockFlow` for the same reason — the fan-out to
    /// every attached sink is the plumbing `BlockEvents` already has.
    BeatSync {
        /// The track's score context (the same key `RenderCue` uses), so a sink
        /// can associate a beat reference with its track.
        context_id: ContextId,
        /// The beat coordinate + tempo at emission; the sink's phasor slews toward it.
        beat_ref: kaijutsu_audio::BeatRef,
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
            Self::RenderCue { .. } => "block.render_cue",
            Self::BeatSync { .. } => "block.beat_sync",
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
            | Self::ContextSwitched { context_id, .. }
            | Self::RenderCue { context_id, .. }
            | Self::BeatSync { context_id, .. } => *context_id,
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
            Self::SyncReset { .. }
            | Self::ContextSwitched { .. }
            | Self::RenderCue { .. }
            | Self::BeatSync { .. } => None,
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
            Self::SyncReset { .. }
            | Self::ContextSwitched { .. }
            | Self::RenderCue { .. }
            | Self::BeatSync { .. } => OpSource::Local,
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
            Self::RenderCue { .. } => BlockFlowKind::RenderCue,
            Self::BeatSync { .. } => BlockFlowKind::BeatSync,
        }
    }

    /// Check if this event passes a [`BlockEventFilter`].
    ///
    /// Used by the server-side subscription bridge to filter events before
    /// serializing them to the wire.
    pub fn matches_filter(&self, filter: &BlockEventFilter) -> bool {
        // A directive, not a block-level event: it targets no block and no
        // single context in a way `BlockEventFilter` was designed to gate
        // (context/kind constraints exist to reduce wire chatter for a
        // *stream* of block changes). The standalone slice forwards it to
        // every subscriber unconditionally; per-context "distributed
        // listening" is future work (docs/pcm.md).
        if matches!(self, Self::RenderCue { .. } | Self::BeatSync { .. }) {
            return true;
        }
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
// FlowBus — per-subscription, bounded, lossless-or-terminated pub/sub
// ============================================================================
//
// # Why this shape
//
// The bus used to be one shared `async_broadcast` ring per topic with
// `set_overflow(true)`: when ANY receiver fell behind, the ring evicted its
// oldest message and every subscriber sharing that ring lost it — silently, and
// upstream of the wire, so a client could not even detect the hole. On
// 2026-08-05 that produced four live incidents in one afternoon (hung turn
// waits from a dropped `TurnCompleted`, swallowed mid-turn text, tool calls
// rendered as perpetually running because the final status patch evaporated).
//
// The replacement gives every subscription its OWN queue:
//
// * **Lossless in publish order.** Ordered-class topics share one FIFO per
//   subscription, so a `block.status` patch can never overtake the text ops it
//   follows, and a `block.inserted` always precedes its block's ops.
// * **Bounded, and bounded per subscriber.** One slow consumer costs its own
//   queue and nothing else.
// * **Terminated, never lossy.** A full queue ends that subscription with an
//   explicit [`FlowRecv::Terminated`] signal. "I'd rather be disconnected"
//   (Amy, 2026-08-05) — a consumer with a hole in its stream is worse than a
//   consumer that knows it must resync.
// * **Publishers never block and never wait on a consumer.** `publish` takes a
//   short, non-async lock per subscription and returns; there is no `.await` on
//   the produce path at all.
// * **Musical time is exempt.** Timing-class topics ride a separate small
//   drop-oldest ring inside the same subscription and are drained first, so a
//   text firehose can neither delay a beat nor be delayed by one, and a missed
//   beat stays missed (docs/midi.md "The one timebase").

use std::collections::{HashSet, VecDeque};
use std::sync::Mutex;
use std::sync::Weak;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::Notify;

/// Default depth of a subscription's ordered (lossless) queue.
///
/// Sized for the worst realistic burst: a streaming turn emits roughly one
/// `block.text_ops` event per token, so 8192 lets a subscriber stall for
/// thousands of tokens — many seconds of model output, or a full GC pause on a
/// GUI client — before it is judged unable to keep up. Each queued entry is a
/// `FlowMessage` (a few pointers; the block snapshot and op bytes are `Arc`d and
/// shared with every other subscriber), so a fully-backed-up subscription costs
/// on the order of a megabyte, not tens. Generous, and still bounded.
///
/// Override at kernel construction with `KAIJUTSU_FLOW_QUEUE_DEPTH`.
pub const DEFAULT_ORDERED_QUEUE_DEPTH: usize = 8192;

/// Depth of a subscription's timing (drop-oldest) ring.
///
/// Deliberately small. Beat references and render cues are low-rate and
/// self-invalidating: a sink back-dates against the emission stamp and rejects
/// stale timing data on its own ladder, so a deep queue would only hand it
/// events it is about to throw away. 64 is several seconds of headroom at the
/// rates docs/midi.md describes.
pub const DEFAULT_TIMING_QUEUE_DEPTH: usize = 64;

/// Environment override for [`DEFAULT_ORDERED_QUEUE_DEPTH`].
///
/// The one knob worth exposing: memory-per-stalled-subscriber versus how long a
/// hiccuping client is tolerated before it is kicked. Read once, where the
/// kernel builds its buses. Same idiom as `KAISH_RC_THREAD_STACK` — no new
/// config system for a single integer.
pub const FLOW_QUEUE_DEPTH_ENV: &str = "KAIJUTSU_FLOW_QUEUE_DEPTH";

/// Resolve the ordered queue depth, honouring [`FLOW_QUEUE_DEPTH_ENV`].
///
/// A malformed or zero value is a configuration mistake, not a reason to guess:
/// it warns loudly and uses the default rather than silently running with a
/// depth nobody asked for.
pub fn configured_queue_depth() -> usize {
    match std::env::var(FLOW_QUEUE_DEPTH_ENV) {
        Err(_) => DEFAULT_ORDERED_QUEUE_DEPTH,
        Ok(raw) => match raw.trim().parse::<usize>() {
            Ok(n) if n > 0 => n,
            _ => {
                tracing::warn!(
                    value = %raw,
                    default = DEFAULT_ORDERED_QUEUE_DEPTH,
                    "{FLOW_QUEUE_DEPTH_ENV} is not a positive integer — using the default",
                );
                DEFAULT_ORDERED_QUEUE_DEPTH
            }
        },
    }
}

/// Why a subscription ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlowTermination {
    /// The topic whose event could not be enqueued.
    pub topic: &'static str,
    /// How many messages this subscription successfully received before the
    /// overflow. Together with [`FlowMessage::seq`] it pins exactly where the
    /// stream ends.
    pub delivered: u64,
    /// The queue depth that was exceeded (for the operator's benefit).
    pub capacity: usize,
}

impl std::fmt::Display for FlowTermination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "subscription terminated: queue of {} full on topic {} after {} events",
            self.capacity, self.topic, self.delivered
        )
    }
}

/// What a subscription's receive returned.
#[derive(Debug)]
pub enum FlowRecv<T> {
    /// The next message, in publish order.
    Message(FlowMessage<T>),
    /// This subscription fell behind and was terminated by the bus. No further
    /// messages will arrive; the consumer's view is incomplete as of the
    /// reported point and must be rebuilt (for a wire client: reconnect and
    /// full resync).
    Terminated(FlowTermination),
}

/// State of one subscription's queues.
struct SlotState<T> {
    /// Lossless FIFO, global publish order across every ordered topic.
    ordered: VecDeque<FlowMessage<T>>,
    /// Latency-first ring; oldest is dropped when full.
    timing: VecDeque<FlowMessage<T>>,
    /// Next ordered-lane sequence number to stamp (starts at 1; 0 means
    /// "never went through a queue"). Counted separately from the timing lane
    /// so that a legitimately-dropped beat cannot punch a hole in the ordered
    /// stream's continuity — a gap there must mean exactly one thing.
    next_seq: u64,
    /// Next timing-lane sequence number. Gaps here are expected and honest:
    /// they say "beats were missed", which is the whole contract of that lane.
    next_timing_seq: u64,
    /// Messages handed to the consumer so far.
    delivered: u64,
    /// Timing-lane events dropped because the consumer was behind. Never a
    /// reason to terminate — see [`TopicClass::Timing`] — but worth counting.
    timing_dropped: u64,
    /// Set when the ordered queue overflowed. The consumer is told once, then
    /// the subscription is finished.
    terminated: Option<FlowTermination>,
    /// The termination has been logged. Guards against re-logging it on every
    /// subsequent publish to a subscriber that has not noticed yet — that is
    /// exactly the "one warning per text delta" noise this rework set out to
    /// kill, and it would come straight back without this flag.
    logged: bool,
    /// The termination has been reported; further receives return `None`.
    reported: bool,
}

/// One subscription's mailbox, shared between the bus (weakly) and the
/// [`Subscription`] handle (strongly). Dropping the handle reaps the slot.
struct SubscriberSlot<T> {
    /// Topics this subscription resolved its pattern to.
    topics: Vec<&'static str>,
    /// The pattern, kept for diagnostics.
    pattern: String,
    ordered_capacity: usize,
    timing_capacity: usize,
    state: Mutex<SlotState<T>>,
    notify: Notify,
}

impl<T: Clone> SubscriberSlot<T> {
    fn matches(&self, topic: &str) -> bool {
        self.topics.iter().any(|t| *t == topic)
    }

    /// Enqueue one message. Returns true if it was accepted (a drop-oldest
    /// eviction still counts as accepted — the subscriber stays live).
    ///
    /// Never blocks, never awaits, never runs user code: the only work under
    /// the lock is a `VecDeque` push.
    fn offer(&self, mut msg: FlowMessage<T>, class: TopicClass) -> bool {
        let mut st = match self.state.lock() {
            Ok(st) => st,
            // A poisoned slot means a consumer panicked mid-receive. Losing the
            // subscriber is correct; poisoning the publisher is not.
            Err(poisoned) => poisoned.into_inner(),
        };
        if st.terminated.is_some() {
            return false;
        }
        match class {
            TopicClass::Timing => {
                if st.timing.len() >= self.timing_capacity {
                    st.timing.pop_front();
                    st.timing_dropped += 1;
                }
                msg.seq = st.next_timing_seq;
                st.next_timing_seq += 1;
                st.timing.push_back(msg);
            }
            TopicClass::Ordered => {
                if st.ordered.len() >= self.ordered_capacity {
                    let info = FlowTermination {
                        topic: msg.topic,
                        delivered: st.delivered,
                        capacity: self.ordered_capacity,
                    };
                    // Drop the backlog: the consumer is being cut off and will
                    // rebuild from scratch, so holding a megabyte of events it
                    // will never use is pure waste.
                    st.ordered.clear();
                    st.terminated = Some(info);
                    drop(st);
                    self.notify.notify_one();
                    return false;
                }
                msg.seq = st.next_seq;
                st.next_seq += 1;
                st.ordered.push_back(msg);
            }
        }
        drop(st);
        self.notify.notify_one();
        true
    }

    /// Pop the next event, timing lane first (latency-first).
    fn pop(&self) -> Option<FlowRecv<T>> {
        let mut st = match self.state.lock() {
            Ok(st) => st,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(msg) = st.timing.pop_front() {
            st.delivered += 1;
            return Some(FlowRecv::Message(msg));
        }
        if let Some(msg) = st.ordered.pop_front() {
            st.delivered += 1;
            return Some(FlowRecv::Message(msg));
        }
        if let Some(info) = st.terminated.clone()
            && !st.reported
        {
            st.reported = true;
            return Some(FlowRecv::Terminated(info));
        }
        None
    }

    fn is_terminated(&self) -> bool {
        match self.state.lock() {
            Ok(st) => st.terminated.is_some(),
            Err(poisoned) => poisoned.into_inner().terminated.is_some(),
        }
    }

    fn finished(&self) -> bool {
        match self.state.lock() {
            Ok(st) => st.reported,
            Err(poisoned) => poisoned.into_inner().reported,
        }
    }
}

/// Shared bus interior. Cloning a [`FlowBus`] shares this.
struct BusInner<T: Clone + Send + 'static> {
    /// Live subscriptions, weakly held so a dropped handle reaps itself.
    subs: Mutex<Vec<Weak<SubscriberSlot<T>>>>,
    /// Registered topic set — a publish to anything else is a TOPICS/subject()
    /// skew bug, not a routing miss.
    topics: HashSet<&'static str>,
    ordered_capacity: usize,
    timing_capacity: usize,
    /// Total subscriptions ever created (diagnostics).
    created: AtomicU64,
}

impl<T: Clone + Send + 'static> Drop for BusInner<T> {
    fn drop(&mut self) {
        // Wake every waiter so `recv()` can observe the closed bus and return
        // `None` instead of parking forever on a dead process.
        if let Ok(subs) = self.subs.lock() {
            for weak in subs.iter() {
                if let Some(slot) = weak.upgrade() {
                    slot.notify.notify_one();
                }
            }
        }
    }
}

/// Type-parameterized pub/sub bus for a specific flow domain.
///
/// See the module-level notes above for the delivery contract. In short:
/// one bounded queue per subscription, lossless in publish order, terminated
/// (never silently truncated) on overflow, with musical time on its own
/// latency-first lane.
pub struct FlowBus<T: Clone + Send + 'static> {
    inner: Arc<BusInner<T>>,
}

impl<T: Clone + Send + 'static> std::fmt::Debug for FlowBus<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlowBus")
            .field("topics", &self.inner.topics)
            .field("ordered_capacity", &self.inner.ordered_capacity)
            .field("timing_capacity", &self.inner.timing_capacity)
            .field("subscribers", &self.subscriber_count())
            .finish()
    }
}

impl<T: Clone + Send + HasSubject + FlowTopics + 'static> FlowBus<T> {
    /// Create a bus whose subscriptions get `ordered_capacity`-deep lossless
    /// queues.
    pub fn with_topics(ordered_capacity: usize) -> Self {
        Self::with_capacities(ordered_capacity, DEFAULT_TIMING_QUEUE_DEPTH)
    }

    /// Create a bus with explicit depths for both lanes.
    pub fn with_capacities(ordered_capacity: usize, timing_capacity: usize) -> Self {
        Self {
            inner: Arc::new(BusInner {
                subs: Mutex::new(Vec::new()),
                topics: T::TOPICS.iter().copied().collect(),
                ordered_capacity: ordered_capacity.max(1),
                timing_capacity: timing_capacity.max(1),
                created: AtomicU64::new(0),
            }),
        }
    }

    /// Backward-compatible constructor; `capacity` is the lossless depth.
    pub fn new(capacity: usize) -> Self {
        Self::with_topics(capacity)
    }

    /// Subscribe to messages matching a NATS-style pattern.
    ///
    /// The pattern is resolved against the declared topic set once, here, so
    /// delivery is a pointer compare rather than a per-message match. A pattern
    /// matching nothing yields a subscription whose `recv()` never resolves —
    /// the historical behaviour, kept so a typo'd pattern fails visibly in a
    /// test rather than looking like an idle bus.
    pub fn subscribe(&self, pattern: &str) -> Subscription<T> {
        let topics: Vec<&'static str> = self
            .inner
            .topics
            .iter()
            .filter(|topic| matches_pattern(pattern, topic))
            .copied()
            .collect();

        if topics.is_empty() {
            return Subscription {
                slot: None,
                bus: Arc::downgrade(&self.inner),
            };
        }

        let slot = Arc::new(SubscriberSlot {
            topics,
            pattern: pattern.to_string(),
            ordered_capacity: self.inner.ordered_capacity,
            timing_capacity: self.inner.timing_capacity,
            state: Mutex::new(SlotState {
                ordered: VecDeque::new(),
                timing: VecDeque::new(),
                next_seq: 1,
                next_timing_seq: 1,
                delivered: 0,
                timing_dropped: 0,
                terminated: None,
                logged: false,
                reported: false,
            }),
            notify: Notify::new(),
        });

        if let Ok(mut subs) = self.inner.subs.lock() {
            subs.retain(|w| w.strong_count() > 0);
            subs.push(Arc::downgrade(&slot));
        }
        self.inner.created.fetch_add(1, Ordering::Relaxed);

        Subscription {
            slot: Some(slot),
            bus: Arc::downgrade(&self.inner),
        }
    }
}

impl<T: Clone + Send + HasSubject + FlowTopics + 'static> FlowBus<T> {
    /// Publish a payload to every subscription whose pattern matches its
    /// subject. Returns the number of subscriptions that accepted it.
    ///
    /// Never blocks. A subscription that cannot accept the message is
    /// terminated (ordered lane) or has its oldest timing event evicted
    /// (timing lane); either way this call returns promptly and every other
    /// subscriber is unaffected.
    pub fn publish(&self, payload: T) -> usize {
        self.dispatch(payload, None)
    }

    /// Publish a payload with sender information.
    pub fn publish_with_sender(&self, payload: T, sender: impl Into<String>) -> usize {
        self.dispatch(payload, Some(sender.into()))
    }

    fn dispatch(&self, payload: T, sender: Option<String>) -> usize {
        let topic = payload.subject();
        if !self.inner.topics.contains(topic) {
            // A subject without a registered topic means the payload's
            // FlowTopics::TOPICS list is out of sync with its subject() — the
            // publish reaches nobody. That class of bug hid
            // `block.context_switched` for months. Scream in debug; stay
            // non-crashing in release.
            debug_assert!(false, "published to unregistered topic {topic:?}");
            tracing::error!(topic, "published to unknown topic — dropped");
            return 0;
        }
        let class = T::topic_class(topic);
        let template = FlowMessage {
            topic,
            payload,
            timestamp: Instant::now(),
            sender,
            seq: 0,
        };

        let mut delivered = 0usize;
        let mut kicked: Vec<(String, FlowTermination)> = Vec::new();
        {
            let mut subs = match self.inner.subs.lock() {
                Ok(subs) => subs,
                Err(poisoned) => poisoned.into_inner(),
            };
            subs.retain(|weak| {
                let Some(slot) = weak.upgrade() else {
                    return false;
                };
                if !slot.matches(topic) {
                    return true;
                }
                if slot.offer(template.clone(), class) {
                    delivered += 1;
                    return true;
                }
                // Refused: either terminated by THIS message, or already
                // terminated and waiting for the consumer to notice. Log the
                // first case exactly once — a terminated slot lingers until its
                // handle drops, and every publish in between would otherwise
                // re-log it.
                if let Ok(mut st) = slot.state.lock()
                    && !st.logged
                    && let Some(info) = st.terminated.clone()
                {
                    st.logged = true;
                    kicked.push((slot.pattern.clone(), info));
                }
                true
            });
        }

        // Log outside the lock. This is the loud replacement for the old
        // "FlowBus overflow — a slow subscriber dropped the oldest event"
        // warning that fired once per text delta: a termination happens at most
        // once per subscription, so it can afford to be an error.
        for (pattern, info) in kicked {
            tracing::error!(
                pattern = %pattern,
                topic = info.topic,
                delivered = info.delivered,
                capacity = info.capacity,
                "FlowBus subscription terminated — consumer fell behind; \
                 it will be told, not silently truncated"
            );
        }
        delivered
    }
}

impl<T: Clone + Send + 'static> FlowBus<T> {
    /// Number of live subscriptions (a wildcard subscription counts once).
    pub fn subscriber_count(&self) -> usize {
        match self.inner.subs.lock() {
            Ok(subs) => subs.iter().filter(|w| w.strong_count() > 0).count(),
            Err(poisoned) => poisoned
                .into_inner()
                .iter()
                .filter(|w| w.strong_count() > 0)
                .count(),
        }
    }

    /// Number of live subscriptions matching a single topic (0 if unknown).
    ///
    /// Lets a producer skip expensive work — a CAS read, a render — when nobody
    /// is listening on that specific subject (a headless kernel with no sink
    /// attached should do no render work: docs/midi.md).
    pub fn topic_subscribers(&self, topic: &str) -> usize {
        let subs = match self.inner.subs.lock() {
            Ok(subs) => subs,
            Err(poisoned) => poisoned.into_inner(),
        };
        subs.iter()
            .filter_map(|w| w.upgrade())
            .filter(|slot| slot.matches(topic))
            .count()
    }

    /// The lossless queue depth each subscription gets.
    pub fn capacity(&self) -> usize {
        self.inner.ordered_capacity
    }
}

impl<T: Clone + Send + 'static> Clone for FlowBus<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

// ============================================================================
// Subscription
// ============================================================================

/// A live subscription to one or more FlowBus topics.
///
/// Created by [`FlowBus::subscribe()`]. Dropping it unsubscribes.
///
/// Two receive surfaces:
/// * [`Subscription::recv`] / [`Subscription::try_recv`] — the historical
///   `Option<FlowMessage<T>>` shape. `None` means the stream is over, whether
///   because the bus closed or because this subscription was terminated for
///   falling behind; ask [`Subscription::is_terminated`] to tell them apart.
/// * [`Subscription::recv_event`] / [`Subscription::try_recv_event`] — the full
///   [`FlowRecv`] shape, which reports the termination explicitly. Every
///   client-facing forwarder uses these, because "you were cut off" is a fact
///   the far end has to learn.
pub struct Subscription<T: Clone + Send + 'static> {
    slot: Option<Arc<SubscriberSlot<T>>>,
    bus: Weak<BusInner<T>>,
}

impl<T: Clone + Send + 'static> Subscription<T> {
    /// Receive the next event, waiting if necessary.
    ///
    /// Returns `None` once the subscription is finished — bus closed, or the
    /// termination has already been reported.
    pub async fn recv_event(&mut self) -> Option<FlowRecv<T>> {
        let Some(slot) = self.slot.clone() else {
            // No topic matched the pattern: park forever, as this bus always has.
            return std::future::pending().await;
        };
        loop {
            if let Some(ev) = slot.pop() {
                return Some(ev);
            }
            if slot.finished() {
                return None;
            }
            if self.bus.upgrade().is_none() {
                // Bus gone and nothing buffered — the stream really is over.
                return None;
            }
            // `notify_one` stores a permit when nobody is waiting, so a publish
            // that lands between the `pop` above and this await is not lost.
            slot.notify.notified().await;
        }
    }

    /// Receive the next message, waiting if necessary.
    ///
    /// `None` covers both "bus closed" and "this subscription was terminated";
    /// use [`Subscription::recv_event`] when the difference matters.
    pub async fn recv(&mut self) -> Option<FlowMessage<T>> {
        match self.recv_event().await {
            Some(FlowRecv::Message(msg)) => Some(msg),
            Some(FlowRecv::Terminated(info)) => {
                tracing::error!(
                    topic = info.topic,
                    delivered = info.delivered,
                    capacity = info.capacity,
                    "flow subscription terminated — this consumer's view is \
                     incomplete and it is not resyncing"
                );
                None
            }
            None => None,
        }
    }

    /// Non-blocking receive of the next event.
    pub fn try_recv_event(&mut self) -> Option<FlowRecv<T>> {
        self.slot.as_ref().and_then(|slot| slot.pop())
    }

    /// Non-blocking receive of the next message.
    pub fn try_recv(&mut self) -> Option<FlowMessage<T>> {
        match self.try_recv_event() {
            Some(FlowRecv::Message(msg)) => Some(msg),
            Some(FlowRecv::Terminated(info)) => {
                tracing::error!(
                    topic = info.topic,
                    delivered = info.delivered,
                    "flow subscription terminated (try_recv)"
                );
                None
            }
            None => None,
        }
    }

    /// Whether this subscription was terminated for falling behind.
    pub fn is_terminated(&self) -> bool {
        self.slot.as_ref().is_some_and(|slot| slot.is_terminated())
    }

    /// How many messages this subscription has handed out.
    pub fn delivered(&self) -> u64 {
        self.slot.as_ref().map_or(0, |slot| {
            match slot.state.lock() {
                Ok(st) => st.delivered,
                Err(p) => p.into_inner().delivered,
            }
        })
    }

    /// Timing-lane events dropped because this consumer was behind. Always 0
    /// for a subscription with no timing topics; never a termination cause.
    pub fn timing_dropped(&self) -> u64 {
        self.slot.as_ref().map_or(0, |slot| {
            match slot.state.lock() {
                Ok(st) => st.timing_dropped,
                Err(p) => p.into_inner().timing_dropped,
            }
        })
    }

    /// The topics this subscription resolved to.
    pub fn topics(&self) -> Vec<&'static str> {
        self.slot.as_ref().map(|s| s.topics.clone()).unwrap_or_default()
    }
}

impl<T: Clone + Send + 'static> std::fmt::Debug for Subscription<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.slot {
            None => f.write_str("Subscription::empty"),
            Some(slot) => f
                .debug_struct("Subscription")
                .field("pattern", &slot.pattern)
                .field("topics", &slot.topics)
                .field("terminated", &slot.is_terminated())
                .finish(),
        }
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
        /// Serialized DTE operations (CBOR-encoded SerializedOpsOwned).
        /// Arc-wrapped to avoid per-subscriber deep cloning.
        ops: Arc<[u8]>,
        /// Origin of this operation.
        source: OpSource,
        /// Per-context monotonic sequence number (M2-B2). Mirrors the
        /// same gap-detection idiom on `BlockFlow::TextOps`.
        #[serde(default)]
        seq_num: u64,
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
// Turn Flow Events
// ============================================================================

/// Why a turn stopped — the structured half of the turn outcome.
///
/// A turn that ran out of tokens, one a player cancelled, and one that simply
/// finished are three different endings, and a consumer has to tell them apart
/// to react correctly. Before this existed, a cancel arrived as
/// `Failed { error: "turn interrupted before completion" }` — a string an
/// observer had to pattern-match to learn anything, which is exactly the silent
/// ambiguity the house style rejects. `Failed` now means *only* "the turn broke";
/// every ending the turn driver reached on purpose is a `Completed` carrying its
/// reason.
///
/// This is the substrate for the ACP adapter's `stopReason` (`end_turn`,
/// `max_tokens`, `max_turn_requests`, `cancelled`) — the mapping is 1:1 by
/// construction, and the wire form is capnp `TurnStopReason`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnStopReason {
    /// The model finished its phrase and asked for nothing more — ACP `end_turn`.
    #[default]
    EndTurn,
    /// A player interrupted the turn (`interruptContext`).
    ///
    /// `immediate` is the RPC's soft/hard distinction, kept because the two
    /// endings differ in kind, not just in urgency: a **soft** cancel
    /// (`immediate: false`) lets the in-flight model call finish and stops the
    /// agentic loop before the next one, so the output block is a *complete*
    /// phrase; a **hard** cancel (`immediate: true`) aborts the HTTP stream
    /// mid-token, so whatever text landed is a fragment. Both map to ACP
    /// `cancelled`; only the consumer that cares about phrase integrity (the
    /// beat scheduler's OODA Act) reads the bool.
    Cancelled {
        /// `true` for a hard interrupt (stream aborted mid-flight).
        immediate: bool,
    },
    /// The provider hit the output-token ceiling — ACP `max_tokens`. Detected
    /// from the provider's own terminal stop reason (`max_tokens` on Anthropic,
    /// `length` on the OpenAI-compatible providers).
    MaxTokens,
    /// The agentic loop hit its per-turn tool-iteration cap — ACP
    /// `max_turn_requests`.
    MaxIterations,
}

impl TurnStopReason {
    /// Whether the turn's `output_block_id` names a *finished* phrase.
    ///
    /// False only for a hard cancel, which severs the stream mid-token. Every
    /// other ending — including a soft cancel, a token-ceiling truncation, and
    /// an iteration-cap halt — leaves the last model block syntactically whole,
    /// because the provider closed the message before the driver stopped.
    /// The beat scheduler gates the OODA **Act** on this: crystallizing a
    /// half-written phrase would schedule notation that was never played.
    pub fn output_is_complete(self) -> bool {
        !matches!(self, Self::Cancelled { immediate: true })
    }

    /// Stable wire/log name, matching the ACP `stopReason` vocabulary.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EndTurn => "end_turn",
            Self::Cancelled { immediate: false } => "cancelled_soft",
            Self::Cancelled { immediate: true } => "cancelled_immediate",
            Self::MaxTokens => "max_tokens",
            Self::MaxIterations => "max_iterations",
        }
    }
}

/// Who asked for the turn — a human at a prompt, or the kernel driving itself.
///
/// The turn driver used to encode this as an `announce_completion: bool`
/// *producer-side gate*: interactive turns published nothing at all, because
/// the one consumer (the beat scheduler's OODA Act) must never crystallize a
/// human-prompted turn. That gate is now here, on the event, where it belongs —
/// every turn announces, and the consumer that cares filters. Without the move,
/// no wire subscriber could ever see an interactive turn complete, which is the
/// case ACP frontends care about most.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnOrigin {
    /// A player submitted a prompt (`prompt` / `submitInput`). The default,
    /// because it is the conservative answer for a decoded-without-the-field
    /// payload: an interactive turn feeds no autonomous machinery.
    #[default]
    Interactive,
    /// The kernel drove the turn itself (`TurnFlow::Requested` — `kj fork
    /// --prompt`, drift delivery, the cruise director).
    Autonomous,
}

impl TurnOrigin {
    /// Stable wire/log name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Autonomous => "autonomous",
        }
    }
}

/// Turn-driving flow events.
///
/// The keystone of headless autonomy: kernel-side code (which can't reach the
/// server's turn driver directly) publishes a `Requested` event to ask the
/// server to run an LLM turn for a context that has no interactive client
/// attached. The first producer is `kj fork --prompt` (POSIX-style: the child
/// starts acting immediately while the parent keeps running); later producers
/// are drift delivery and the "cruise director". The server subscribes and
/// calls `spawn_llm_for_prompt`.
///
/// The seed itself already lives in the context's block log (e.g. the fork
/// note), so this carries only what the driver needs to anchor and attribute
/// the turn — it does NOT re-insert the seed.
///
/// # On the wire, but never journaled
///
/// `Requested` stays in-process — it is a request *to* the server's turn
/// driver, and nothing outside the kernel process can serve it. The two
/// outcome variants are different: `Completed`/`Failed` cross the capnp
/// boundary through the `TurnEvents` callback interface (`subscribeTurnEvents`,
/// bridged in `kaijutsu-server::rpc`), because completion is the one turn fact
/// every client needs and none can compute. Before that bridge existed, clients
/// inferred completion by polling block status — a heuristic that cannot
/// distinguish "finished" from "cancelled" from "still thinking".
///
/// Still **never journaled**, and that is deliberate: blocks are the durable
/// record of what a turn produced. These events are a live signal about a live
/// turn; replaying them after a restart would announce completions for turns
/// nobody is waiting on.
///
/// A subscriber that missed the push can fall back to the block log, but
/// only for **what the turn wrote** — the text and status ops are durable.
/// It cannot recover **why the turn stopped**: `reason` (`TurnStopReason`)
/// has no block-log shadow, and `EndTurn`, `MaxTokens`, and a soft cancel all
/// leave the same thing behind (a `Done` block with ordinary text) —
/// indistinguishable from block status alone. A missed push genuinely loses
/// that fact, not just delays it. The real fix is a catch-up story for the
/// bus itself (deepseek post-merge review, `docs/issues.md`: "TurnFlow bus
/// lossy + in-memory"), not a claim that the block log already covers it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TurnFlow {
    /// Drive one autonomous turn for `context_id`.
    Requested {
        /// The context that should take a turn.
        context_id: ContextId,
        /// Block to anchor the response after (typically the context's last
        /// block at request time — the seed and any fork markers precede it).
        after_block_id: BlockId,
        /// Seed text. Only a hydration-failure fallback; the authoritative
        /// seed is the block already in the log.
        content: String,
        /// Principal the turn runs as — authors the seed half of the
        /// exchange. The context's configured model answers.
        principal_id: PrincipalId,
        /// Model override, or None to use the context's configured model.
        model: Option<String>,
    },

    /// A turn reached an ending the driver arrived at on purpose — it finished,
    /// ran out of tokens or iterations, or a player cancelled it. `reason` says
    /// which; only a genuine breakage publishes [`TurnFlow::Failed`].
    ///
    /// Every turn publishes this, interactive and autonomous alike. It is the
    /// join-side substrate for a blocking waiter (`kj wait`, the delegation
    /// join) and the payload the `TurnEvents` wire bridge forwards to clients.
    Completed {
        /// The context whose turn completed.
        context_id: ContextId,
        /// Principal the turn ran as.
        principal_id: PrincipalId,
        /// The block the turn produced that the OODA **Act** should crystallize
        /// — the last `Role::Model` / `BlockKind::Text` block this stream
        /// inserted, or `None` when the turn produced no text. Carried so the
        /// beat scheduler schedules *that* block's ABC instead of a blind
        /// last-block read that races the model (F2 §7).
        #[serde(default)]
        output_block_id: Option<BlockId>,
        /// Why the turn stopped. `#[serde(default)]` → [`TurnStopReason::EndTurn`]
        /// for a payload that predates the field, which is the only honest
        /// default: before this field existed, every `Completed` *was* a clean
        /// end-of-turn (cancels went out as `Failed`).
        #[serde(default)]
        reason: TurnStopReason,
        /// Who asked for the turn. `#[serde(default)]` →
        /// [`TurnOrigin::Interactive`], the answer that feeds no autonomous
        /// machinery if the field is ever missing.
        #[serde(default)]
        origin: TurnOrigin,
    },

    /// A turn **broke** — it never reached an ending the driver chose. Carries
    /// the error so a waiter (`kj wait`, an ACP frontend) can surface it rather
    /// than hanging or silently succeeding.
    ///
    /// A cancelled turn is NOT a failure: it publishes
    /// `Completed { reason: Cancelled { .. } }`. Reserve this for hydration
    /// failures, provider/stream errors, and unreadable policy — the cases
    /// where there is a real error to report and no turn happened.
    Failed {
        /// The context whose turn failed.
        context_id: ContextId,
        /// Principal the turn ran as.
        principal_id: PrincipalId,
        /// Human-readable failure description.
        error: String,
        /// Who asked for the turn. See [`TurnFlow::Completed::origin`].
        #[serde(default)]
        origin: TurnOrigin,
    },
}

impl TurnFlow {
    /// Get the subject string for this event.
    pub fn subject(&self) -> &'static str {
        match self {
            Self::Requested { .. } => "turn.requested",
            Self::Completed { .. } => "turn.completed",
            Self::Failed { .. } => "turn.failed",
        }
    }

    /// Get the context ID for this event.
    pub fn context_id(&self) -> ContextId {
        match self {
            Self::Requested { context_id, .. } => *context_id,
            Self::Completed { context_id, .. } => *context_id,
            Self::Failed { context_id, .. } => *context_id,
        }
    }
}

impl HasSubject for TurnFlow {
    fn subject(&self) -> &'static str {
        TurnFlow::subject(self)
    }
}

// ============================================================================
// Editor Flow Events
// ============================================================================

/// Editor-session flow events — the push channel for the in-app vi editor.
///
/// A session's renderer-facing [`EditorState`](crate::editor::EditorState)
/// changed, or a session closed. Rides the in-process FlowBus only (like
/// [`TurnFlow`]); the server's `subscribe_editor` bridge serializes each event
/// onto the `EditorEvents` capnp callback so every renderer of a session sees
/// state the instant it lands — the reason the editor channel is push, not poll.
///
/// Published by the kernel's `editor_keys`/`editor_save` (→ `StateChanged`) and
/// `editor_quit` (→ `Closed`). The remote-merge producer — a *peer's* CRDT edit
/// reconciling into an open session — wires in with `EditorCore::apply_remote_ops`
/// (vi.md step 1b) and publishes `StateChanged` from the block `TextOps` path.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum EditorFlow {
    /// A session's state changed (a keystroke landed, a save moved the
    /// checkpoint, or a remote op merged). Carries the full snapshot so the
    /// bridge needs no follow-up read.
    StateChanged {
        /// The session whose state changed (`EditorSessionId::as_u64`).
        session_id: u64,
        /// The new renderer-facing snapshot.
        state: crate::editor::EditorState,
    },
    /// A session closed (`ZQ`/quit). Renderers drop it.
    Closed {
        /// The session that closed.
        session_id: u64,
    },
}

impl EditorFlow {
    /// Get the subject string for this event.
    pub fn subject(&self) -> &'static str {
        match self {
            Self::StateChanged { .. } => "editor.state_changed",
            Self::Closed { .. } => "editor.closed",
        }
    }

    /// The session this event concerns.
    pub fn session_id(&self) -> u64 {
        match self {
            Self::StateChanged { session_id, .. } | Self::Closed { session_id } => *session_id,
        }
    }
}

impl HasSubject for EditorFlow {
    fn subject(&self) -> &'static str {
        EditorFlow::subject(self)
    }
}

// ============================================================================
// Shared FlowBus Handle
// ============================================================================

/// Thread-safe handle to a BlockFlow bus.
pub type SharedBlockFlowBus = Arc<FlowBus<BlockFlow>>;

/// Thread-safe handle to an InputDocFlow bus.
pub type SharedInputDocFlowBus = Arc<FlowBus<InputDocFlow>>;

/// Thread-safe handle to a TurnFlow bus.
pub type SharedTurnFlowBus = Arc<FlowBus<TurnFlow>>;

/// Thread-safe handle to an EditorFlow bus.
pub type SharedEditorFlowBus = Arc<FlowBus<EditorFlow>>;

/// Create a new shared block flow bus.
pub fn shared_block_flow_bus(capacity: usize) -> SharedBlockFlowBus {
    Arc::new(FlowBus::new(capacity))
}

/// Create a new shared input doc flow bus.
pub fn shared_input_doc_flow_bus(capacity: usize) -> SharedInputDocFlowBus {
    Arc::new(FlowBus::new(capacity))
}

/// Create a new shared turn flow bus.
pub fn shared_turn_flow_bus(capacity: usize) -> SharedTurnFlowBus {
    Arc::new(FlowBus::new(capacity))
}

/// Create a new shared editor flow bus.
pub fn shared_editor_flow_bus(capacity: usize) -> SharedEditorFlowBus {
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
                seq_num: 0,
            });
        }

        // Publish 1 StatusChanged
        bus.publish(BlockFlow::StatusChanged {
            context_id: ctx,
            block_id: id,
            status: Status::Done,
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
            seq_num: 0,
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
                seq_num: 0,
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
            source: OpSource::Local,
        });

        assert!(inserted_sub.try_recv().is_some());
        assert!(inserted_sub.try_recv().is_none());
        assert!(status_sub.try_recv().is_some());
        assert!(status_sub.try_recv().is_none());
    }

    /// One subscription counts once, however many topics its pattern spans —
    /// the queue is per-subscription now, not per-topic-channel. `topic_subscribers`
    /// still answers the producer's question ("is anyone listening to THIS?").
    #[test]
    fn test_subscriber_count_counts_subscriptions() {
        let bus: FlowBus<BlockFlow> = FlowBus::new(64);
        assert_eq!(bus.subscriber_count(), 0);

        let _s1 = bus.subscribe("block.inserted");
        assert_eq!(bus.subscriber_count(), 1);

        let _s2 = bus.subscribe("block.status");
        assert_eq!(bus.subscriber_count(), 2);

        // A wildcard is ONE subscriber with one queue, not one per topic.
        let s3 = bus.subscribe("block.*");
        assert_eq!(bus.subscriber_count(), 3);
        assert_eq!(s3.topics().len(), BlockFlow::TOPICS.len());

        // Per-topic interest: the exact subscriber plus the wildcard.
        assert_eq!(bus.topic_subscribers("block.inserted"), 2);
        assert_eq!(bus.topic_subscribers("block.text_ops"), 1);
        assert_eq!(bus.topic_subscribers("nope.nothing"), 0);

        // Dropping a subscription reaps it.
        drop(s3);
        assert_eq!(bus.subscriber_count(), 2);
        assert_eq!(bus.topic_subscribers("block.text_ops"), 0);
    }

    /// Subscribe to a pattern that matches no topics.
    #[test]
    fn test_empty_subscription() {
        let bus: FlowBus<BlockFlow> = FlowBus::new(64);
        let mut sub = bus.subscribe("nonexistent.topic");
        assert!(sub.try_recv().is_none());
    }

    /// Every subject in TOPICS must have a pre-created channel. (We can't
    /// construct a BlockFlow whose subject() is unregistered — and publishing
    /// to an unknown topic now debug_asserts — so we verify the forward
    /// direction here; the exhaustive `*_all_subjects_in_topics` tests cover
    /// the reverse, that every variant's subject() is registered.)
    #[test]
    fn test_publish_to_unknown_topic_returns_zero() {
        // Verify that all known subjects ARE routable.
        let bus: FlowBus<BlockFlow> = FlowBus::new(64);
        for &topic in BlockFlow::TOPICS {
            assert!(
                bus.inner.topics.contains(topic),
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
    async fn test_input_doc_flow_topics() {
        let bus: FlowBus<InputDocFlow> = FlowBus::new(16);
        let mut ops_sub = bus.subscribe("input.text_ops");
        let mut cleared_sub = bus.subscribe("input.cleared");

        let ctx = ContextId::new();
        bus.publish(InputDocFlow::TextOps {
            context_id: ctx,
            ops: Arc::from(vec![1u8]),
            source: OpSource::Local,
            seq_num: 0,
        });
        bus.publish(InputDocFlow::Cleared { context_id: ctx });

        assert!(ops_sub.try_recv().is_some());
        assert!(ops_sub.try_recv().is_none());
        assert!(cleared_sub.try_recv().is_some());
        assert!(cleared_sub.try_recv().is_none());
    }

    #[tokio::test]
    async fn test_turn_flow_topics() {
        let bus: FlowBus<TurnFlow> = FlowBus::new(16);
        let mut sub = bus.subscribe("turn.requested");

        let ctx = ContextId::new();
        let principal = PrincipalId::new();
        let after = BlockId::new(ctx, principal, 7);

        let delivered = bus.publish(TurnFlow::Requested {
            context_id: ctx,
            after_block_id: after,
            content: "explore the auth module".into(),
            principal_id: principal,
            model: Some("claude-haiku-4-5".into()),
        });
        assert_eq!(delivered, 1, "exactly one subscriber should receive it");

        let msg = sub.try_recv().expect("should receive the turn request");
        assert_eq!(msg.topic, "turn.requested");
        match msg.payload {
            TurnFlow::Requested {
                context_id,
                after_block_id,
                content,
                principal_id,
                model,
            } => {
                assert_eq!(context_id, ctx);
                assert_eq!(after_block_id, after);
                assert_eq!(content, "explore the auth module");
                assert_eq!(principal_id, principal);
                assert_eq!(model.as_deref(), Some("claude-haiku-4-5"));
            }
            other => panic!("expected Requested, got {other:?}"),
        }
        assert!(sub.try_recv().is_none(), "no duplicate delivery");
    }

    #[tokio::test]
    async fn test_turn_flow_outcome_topics() {
        let bus: FlowBus<TurnFlow> = FlowBus::new(16);
        let mut completed_sub = bus.subscribe("turn.completed");
        let mut failed_sub = bus.subscribe("turn.failed");

        let ctx = ContextId::new();
        let principal = PrincipalId::new();

        let delivered = bus.publish(TurnFlow::Completed {
            context_id: ctx,
            principal_id: principal,
            output_block_id: None,
            reason: TurnStopReason::EndTurn,
            origin: TurnOrigin::Autonomous,
        });
        assert_eq!(delivered, 1, "exactly one subscriber on turn.completed");

        let delivered = bus.publish(TurnFlow::Failed {
            context_id: ctx,
            principal_id: principal,
            error: "boom".into(),
            origin: TurnOrigin::Autonomous,
        });
        assert_eq!(delivered, 1, "exactly one subscriber on turn.failed");

        let msg = completed_sub.try_recv().expect("should receive completion");
        assert_eq!(msg.topic, "turn.completed");
        match msg.payload {
            TurnFlow::Completed {
                context_id,
                principal_id,
                ..
            } => {
                assert_eq!(context_id, ctx);
                assert_eq!(principal_id, principal);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        assert!(completed_sub.try_recv().is_none(), "no duplicate completion");

        let msg = failed_sub.try_recv().expect("should receive failure");
        assert_eq!(msg.topic, "turn.failed");
        match msg.payload {
            TurnFlow::Failed {
                context_id,
                principal_id,
                error,
                origin,
            } => {
                assert_eq!(context_id, ctx);
                assert_eq!(principal_id, principal);
                assert_eq!(error, "boom");
                assert_eq!(origin, TurnOrigin::Autonomous);
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(failed_sub.try_recv().is_none(), "no duplicate failure");

        // Cross-topic isolation: completed sub never saw the failure.
        assert!(
            completed_sub.try_recv().is_none(),
            "turn.completed must not receive turn.failed"
        );
    }

    /// F2 §7/§13 — `TurnFlow::Completed` carries the turn's output block id so
    /// the beat scheduler schedules *that* block's ABC, not a blind last-block
    /// read. It round-trips a real id and tolerates `None` (a turn with no
    /// text); the wire half of the same field is covered by the capnp
    /// round-trip tests in `kaijutsu-client`.
    #[tokio::test]
    async fn completed_carries_output_block_id() {
        let bus: FlowBus<TurnFlow> = FlowBus::new(16);
        let mut sub = bus.subscribe("turn.completed");

        let ctx = ContextId::new();
        let principal = PrincipalId::new();
        let output = BlockId::new(ctx, principal, 42);

        bus.publish(TurnFlow::Completed {
            context_id: ctx,
            principal_id: principal,
            output_block_id: Some(output),
            reason: TurnStopReason::EndTurn,
            origin: TurnOrigin::Autonomous,
        });
        let msg = sub.try_recv().expect("should receive completion");
        match msg.payload {
            TurnFlow::Completed {
                output_block_id, ..
            } => assert_eq!(output_block_id, Some(output), "the model block id rides Completed"),
            other => panic!("expected Completed, got {other:?}"),
        }

        // A turn that produced no text publishes `None` — not an error.
        bus.publish(TurnFlow::Completed {
            context_id: ctx,
            principal_id: principal,
            output_block_id: None,
            reason: TurnStopReason::EndTurn,
            origin: TurnOrigin::Autonomous,
        });
        let msg = sub.try_recv().expect("should receive the second completion");
        match msg.payload {
            TurnFlow::Completed {
                output_block_id, ..
            } => assert_eq!(output_block_id, None, "no text → None, never a fabricated id"),
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// `#[serde(default)]` — an older in-process payload (or a serialized form
    /// that predates the field) decodes with `output_block_id = None` rather
    /// than failing. TurnFlow is FlowBus-only, but the default keeps the field
    /// purely additive against any future serialization of the enum.
    #[test]
    fn completed_output_block_id_defaults_when_absent() {
        let ctx = ContextId::new();
        let principal = PrincipalId::new();
        // A JSON form lacking the field — decodes to None via #[serde(default)].
        let json = serde_json::json!({
            "Completed": {
                "context_id": ctx,
                "principal_id": principal
            }
        });
        let decoded: TurnFlow =
            serde_json::from_value(json).expect("Completed decodes without output_block_id");
        match decoded {
            TurnFlow::Completed {
                output_block_id, ..
            } => assert_eq!(output_block_id, None),
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// `reason` and `origin` are additive the same way `output_block_id` was: a
    /// payload that predates them decodes to the conservative defaults rather
    /// than failing. `EndTurn` is honest for an old `Completed` (cancels used to
    /// ride `Failed`); `Interactive` is the origin that feeds no autonomous
    /// machinery, so a missing field can never spuriously fire the OODA Act.
    #[test]
    fn completed_reason_and_origin_default_when_absent() {
        let ctx = ContextId::new();
        let principal = PrincipalId::new();
        let json = serde_json::json!({
            "Completed": { "context_id": ctx, "principal_id": principal }
        });
        let decoded: TurnFlow =
            serde_json::from_value(json).expect("Completed decodes without reason/origin");
        match decoded {
            TurnFlow::Completed { reason, origin, .. } => {
                assert_eq!(reason, TurnStopReason::EndTurn);
                assert_eq!(origin, TurnOrigin::Interactive);
            }
            other => panic!("expected Completed, got {other:?}"),
        }

        let json = serde_json::json!({
            "Failed": { "context_id": ctx, "principal_id": principal, "error": "boom" }
        });
        let decoded: TurnFlow =
            serde_json::from_value(json).expect("Failed decodes without origin");
        match decoded {
            TurnFlow::Failed { origin, .. } => assert_eq!(origin, TurnOrigin::Interactive),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// A cancelled turn is a `Completed` with a reason, never a bare error
    /// string — the whole point of the structured stop reason. Soft and hard
    /// cancels stay distinguishable, because only the hard one severs the phrase.
    #[test]
    fn cancel_is_a_completed_reason_not_a_failure() {
        let soft = TurnStopReason::Cancelled { immediate: false };
        let hard = TurnStopReason::Cancelled { immediate: true };
        assert_ne!(soft, hard, "soft and hard cancels are distinguishable");

        // Only a hard cancel leaves a half-written phrase behind.
        assert!(
            soft.output_is_complete(),
            "a soft cancel lets the in-flight model call finish"
        );
        assert!(
            !hard.output_is_complete(),
            "a hard cancel aborts mid-token — the output is a fragment"
        );

        // Every non-cancel ending leaves a whole phrase.
        for reason in [
            TurnStopReason::EndTurn,
            TurnStopReason::MaxTokens,
            TurnStopReason::MaxIterations,
        ] {
            assert!(reason.output_is_complete(), "{reason:?} closes the message");
        }
    }

    /// The stop-reason names are the ACP `stopReason` vocabulary and the capnp
    /// enumerant names. They are wire-visible, so pin them.
    #[test]
    fn stop_reason_names_are_stable() {
        assert_eq!(TurnStopReason::EndTurn.as_str(), "end_turn");
        assert_eq!(
            TurnStopReason::Cancelled { immediate: false }.as_str(),
            "cancelled_soft"
        );
        assert_eq!(
            TurnStopReason::Cancelled { immediate: true }.as_str(),
            "cancelled_immediate"
        );
        assert_eq!(TurnStopReason::MaxTokens.as_str(), "max_tokens");
        assert_eq!(TurnStopReason::MaxIterations.as_str(), "max_iterations");
        assert_eq!(TurnOrigin::Interactive.as_str(), "interactive");
        assert_eq!(TurnOrigin::Autonomous.as_str(), "autonomous");
    }

    /// Round-trip every stop reason through serde so a future journaling or
    /// cross-process hop can't silently collapse two endings into one.
    #[test]
    fn stop_reason_round_trips_through_serde() {
        for reason in [
            TurnStopReason::EndTurn,
            TurnStopReason::Cancelled { immediate: false },
            TurnStopReason::Cancelled { immediate: true },
            TurnStopReason::MaxTokens,
            TurnStopReason::MaxIterations,
        ] {
            let json = serde_json::to_string(&reason).expect("serialize");
            let back: TurnStopReason = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, reason, "{json} must round-trip");
        }
    }

    // ====================================================================
    // Exhaustive subject ⊆ TOPICS coverage (durable regression guard)
    //
    // Every variant's subject() MUST be registered in its TOPICS list, or
    // FlowBus pre-creates no channel for it and publish() drops the event
    // ("published to unknown topic"). This bit `block.context_switched`.
    //
    // Each helper builds ONE of every variant via an exhaustive match (no
    // `_ =>` arm) so that adding a future variant fails to compile here
    // until its subject is asserted — the test cannot silently fall behind.
    // ====================================================================

    fn assert_subjects_registered<T: HasSubject>(variants: &[T], topics: &[&'static str]) {
        for v in variants {
            let subject = v.subject();
            assert!(
                topics.contains(&subject),
                "subject {subject:?} is not in TOPICS {topics:?} — \
                 FlowBus will drop publishes to it"
            );
        }
    }

    #[test]
    fn test_block_flow_all_subjects_in_topics() {
        let ctx = ContextId::new();
        let id = BlockId::new(ctx, PrincipalId::new(), 1);
        let block = Arc::new(BlockSnapshot::text(id, None, Role::User, "test"));

        let variants = vec![
            BlockFlow::Inserted {
                context_id: ctx,
                block: block.clone(),
                after_id: None,
                ops: Arc::from(Vec::<u8>::new()),
                source: OpSource::Local,
            },
            BlockFlow::TextOps {
                context_id: ctx,
                block_id: id,
                ops: Arc::from(Vec::<u8>::new()),
                source: OpSource::Local,
                seq_num: 0,
            },
            BlockFlow::Deleted {
                context_id: ctx,
                block_id: id,
                source: OpSource::Local,
            },
            BlockFlow::StatusChanged {
                context_id: ctx,
                block_id: id,
                status: Status::Done,
                source: OpSource::Local,
            },
            BlockFlow::CollapsedChanged {
                context_id: ctx,
                block_id: id,
                collapsed: true,
                source: OpSource::Local,
            },
            BlockFlow::ExcludedChanged {
                context_id: ctx,
                block_id: id,
                excluded: true,
                source: OpSource::Local,
            },
            BlockFlow::Moved {
                context_id: ctx,
                block_id: id,
                after_id: None,
                source: OpSource::Local,
            },
            BlockFlow::SyncReset {
                context_id: ctx,
                generation: 1,
            },
            BlockFlow::OutputChanged {
                context_id: ctx,
                block_id: id,
                output: None,
                source: OpSource::Local,
            },
            BlockFlow::MetadataChanged {
                context_id: ctx,
                block_id: id,
                metadata: kaijutsu_types::BlockMetadata::default(),
                source: OpSource::Local,
            },
            BlockFlow::ContextSwitched { context_id: ctx },
            BlockFlow::RenderCue {
                context_id: ctx,
                cue: kaijutsu_audio::RenderCue::now_inline("audio/wav", vec![1, 2, 3]),
            },
            BlockFlow::BeatSync {
                context_id: ctx,
                beat_ref: kaijutsu_audio::BeatRef::new(0.0, 2.0),
            },
        ];

        // Exhaustiveness gate: a new variant without an arm here breaks
        // compilation, forcing this test (and TOPICS) to be updated.
        for v in &variants {
            match v {
                BlockFlow::Inserted { .. }
                | BlockFlow::TextOps { .. }
                | BlockFlow::Deleted { .. }
                | BlockFlow::StatusChanged { .. }
                | BlockFlow::CollapsedChanged { .. }
                | BlockFlow::ExcludedChanged { .. }
                | BlockFlow::Moved { .. }
                | BlockFlow::SyncReset { .. }
                | BlockFlow::OutputChanged { .. }
                | BlockFlow::MetadataChanged { .. }
                | BlockFlow::ContextSwitched { .. }
                | BlockFlow::RenderCue { .. }
                | BlockFlow::BeatSync { .. } => {}
            }
        }

        assert_subjects_registered(&variants, BlockFlow::TOPICS);
    }

    /// `RenderCue` is a directive, not a filterable block-level event —
    /// `matches_filter` must bypass every constraint unconditionally, even a
    /// filter naming a *different* context (the standalone slice forwards to
    /// every subscriber; per-context routing is future work, docs/pcm.md).
    #[test]
    fn render_cue_bypasses_matches_filter_unconditionally() {
        let ctx = ContextId::new();
        let other_ctx = ContextId::new();
        let flow = BlockFlow::RenderCue {
            context_id: ctx,
            cue: kaijutsu_audio::RenderCue::now_inline("audio/wav", vec![1, 2, 3]),
        };

        let constrained = kaijutsu_types::BlockEventFilter {
            context_ids: vec![other_ctx],
            event_types: vec![kaijutsu_types::BlockFlowKind::StatusChanged],
            block_kinds: vec![],
        };
        assert!(
            flow.matches_filter(&constrained),
            "RenderCue must bypass context/event-type filtering unconditionally"
        );
    }

    #[test]
    fn test_input_doc_flow_all_subjects_in_topics() {
        let ctx = ContextId::new();
        let variants = vec![
            InputDocFlow::TextOps {
                context_id: ctx,
                ops: Arc::from(Vec::<u8>::new()),
                source: OpSource::Local,
                seq_num: 0,
            },
            InputDocFlow::Cleared { context_id: ctx },
        ];

        for v in &variants {
            match v {
                InputDocFlow::TextOps { .. } | InputDocFlow::Cleared { .. } => {}
            }
        }

        assert_subjects_registered(&variants, InputDocFlow::TOPICS);
    }

    #[test]
    fn test_turn_flow_all_subjects_in_topics() {
        let ctx = ContextId::new();
        let principal = PrincipalId::new();
        let after = BlockId::new(ctx, principal, 1);
        let variants = vec![
            TurnFlow::Requested {
                context_id: ctx,
                after_block_id: after,
                content: "go".into(),
                principal_id: principal,
                model: None,
            },
            TurnFlow::Completed {
                context_id: ctx,
                principal_id: principal,
                output_block_id: None,
                reason: TurnStopReason::EndTurn,
                origin: TurnOrigin::Autonomous,
            },
            TurnFlow::Failed {
                context_id: ctx,
                principal_id: principal,
                error: "boom".into(),
                origin: TurnOrigin::Autonomous,
            },
        ];

        for v in &variants {
            match v {
                TurnFlow::Requested { .. }
                | TurnFlow::Completed { .. }
                | TurnFlow::Failed { .. } => {}
            }
        }

        assert_subjects_registered(&variants, TurnFlow::TOPICS);
    }

    // ====================================================================
    // Backpressure contract (2026-08-05 rework)
    //
    // The bus used to drop the oldest event out of a shared per-topic ring
    // whenever ANY subscriber fell behind — silently, upstream of the wire.
    // These pin the replacement: lossless-or-terminated per subscription,
    // publishers never blocked, musical time exempt.
    // ====================================================================

    fn text_op(ctx: ContextId, id: BlockId, byte: u8) -> BlockFlow {
        BlockFlow::TextOps {
            context_id: ctx,
            block_id: id,
            ops: Arc::from(vec![byte]),
            source: OpSource::Local,
            seq_num: 0,
        }
    }

    /// The headline property. One subscriber never reads; another reads
    /// everything. Under a burst far larger than the queue:
    ///   (a) the fast subscriber receives EVERY event, seq contiguous,
    ///   (b) the slow one is TERMINATED with an explicit signal — not silently
    ///       lagged, not handed a stream with a hole in it,
    ///   (c) the publisher never blocks.
    ///
    /// On the old bus (a) failed: the shared ring evicted the oldest for
    /// everyone, and (b) surfaced only as a log line.
    #[tokio::test]
    async fn slow_subscriber_is_terminated_and_fast_one_loses_nothing() {
        let bus: FlowBus<BlockFlow> = FlowBus::with_capacities(32, 8);
        let mut fast = bus.subscribe("block.text_ops");
        let mut slow = bus.subscribe("block.text_ops");

        let ctx = ContextId::new();
        let id = BlockId::new(ctx, PrincipalId::new(), 1);

        const BURST: usize = 500;
        let started = std::time::Instant::now();
        for i in 0..BURST {
            bus.publish(text_op(ctx, id, (i % 251) as u8));
            // The fast subscriber keeps up; the slow one never reads.
            let got = fast.try_recv_event().expect("fast subscriber has an event");
            match got {
                FlowRecv::Message(msg) => {
                    assert_eq!(
                        msg.seq,
                        i as u64 + 1,
                        "per-subscription seq is contiguous — no silent gap"
                    );
                    match msg.payload {
                        BlockFlow::TextOps { ref ops, .. } => {
                            assert_eq!(ops[0], (i % 251) as u8, "content arrives intact, in order")
                        }
                        ref other => panic!("expected TextOps, got {other:?}"),
                    }
                }
                FlowRecv::Terminated(info) => {
                    panic!("the fast subscriber must never be terminated: {info}")
                }
            }
        }
        // (c) A stalled subscriber must not slow the producer down. This is a
        // generous bound; the point is "no blocking", not a benchmark.
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "publishing must never wait on a stalled subscriber (took {:?})",
            started.elapsed()
        );

        // (b) The slow one learns it was cut off, explicitly.
        assert!(slow.is_terminated(), "the slow subscriber is terminated");
        let ev = slow.recv_event().await.expect("a termination is reported");
        match ev {
            FlowRecv::Terminated(info) => {
                assert_eq!(info.topic, "block.text_ops");
                assert_eq!(info.capacity, 32);
            }
            FlowRecv::Message(_) => panic!("a terminated subscription reports the kick first"),
        }
        // Reported exactly once, then the stream is over.
        assert!(
            slow.recv_event().await.is_none(),
            "the termination is reported once, then the subscription is finished"
        );
    }

    /// `recv()` (the historical `Option` surface) reports a termination as
    /// end-of-stream, and `is_terminated()` distinguishes it from a closed bus.
    /// In-process consumers keep compiling; the ones that care ask.
    #[tokio::test]
    async fn terminated_reads_as_end_of_stream_on_the_option_surface() {
        let bus: FlowBus<BlockFlow> = FlowBus::with_capacities(4, 4);
        let mut sub = bus.subscribe("block.text_ops");
        let ctx = ContextId::new();
        let id = BlockId::new(ctx, PrincipalId::new(), 1);
        for i in 0..64 {
            bus.publish(text_op(ctx, id, i as u8));
        }
        // The backlog is dropped on termination — the consumer is rebuilding
        // from scratch anyway, so buffered events are pure waste.
        assert!(sub.recv().await.is_none());
        assert!(sub.is_terminated(), "and it can tell WHY the stream ended");
    }

    /// Turn events are low-rate and critical. Bulk text pressure on the block
    /// bus must not be able to evict, delay, or terminate a healthy
    /// subscriber's `turn.completed` — the incident-1/2 signature (a dropped
    /// `TurnCompleted` that left an ACP prompt promise unresolved forever).
    #[tokio::test]
    async fn turn_completion_survives_block_firehose_pressure() {
        let blocks: FlowBus<BlockFlow> = FlowBus::with_capacities(16, 8);
        let turns: FlowBus<TurnFlow> = FlowBus::with_capacities(16, 8);

        // A client-shaped consumer: subscribed to both, reading neither
        // while the firehose runs.
        let _block_sub = blocks.subscribe("block.*");
        let mut turn_sub = turns.subscribe("turn.completed");

        let ctx = ContextId::new();
        let principal = PrincipalId::new();
        let id = BlockId::new(ctx, principal, 1);
        for i in 0..5_000 {
            blocks.publish(text_op(ctx, id, i as u8));
        }

        let delivered = turns.publish(TurnFlow::Completed {
            context_id: ctx,
            principal_id: principal,
            output_block_id: Some(id),
            reason: TurnStopReason::EndTurn,
            origin: TurnOrigin::Interactive,
        });
        assert_eq!(delivered, 1, "the turn subscriber is still live and healthy");

        let ev = turn_sub.recv_event().await.expect("completion arrives");
        match ev {
            FlowRecv::Message(msg) => assert_eq!(msg.topic, "turn.completed"),
            FlowRecv::Terminated(info) => {
                panic!("block pressure must not terminate the turn stream: {info}")
            }
        }
        assert!(!turn_sub.is_terminated());
    }

    /// Musical time keeps its own semantics: a missed beat is missed. The
    /// timing lane drops the oldest and NEVER terminates the subscription
    /// (docs/midi.md "The one timebase"), and it is drained ahead of the
    /// ordered lane so a text firehose can't delay a beat.
    #[tokio::test]
    async fn timing_lane_drops_oldest_and_never_terminates() {
        let bus: FlowBus<BlockFlow> = FlowBus::with_capacities(4096, 4);
        let mut sub = bus.subscribe("block.*");
        let ctx = ContextId::new();
        let id = BlockId::new(ctx, PrincipalId::new(), 1);

        // Ordered backlog first, then a burst of beats.
        for i in 0..100 {
            bus.publish(text_op(ctx, id, i as u8));
        }
        for i in 0..40 {
            bus.publish(BlockFlow::BeatSync {
                context_id: ctx,
                beat_ref: kaijutsu_audio::BeatRef::new(i as f64, 2.0),
            });
        }

        assert!(!sub.is_terminated(), "a beat backlog never kicks a subscriber");
        assert_eq!(sub.timing_dropped(), 36, "oldest beats dropped, newest kept");

        // Latency-first: the beats come out AHEAD of the queued text ops.
        for _ in 0..4 {
            let msg = sub.try_recv().expect("a beat");
            assert_eq!(
                msg.topic, "block.beat_sync",
                "the timing lane is drained before the ordered backlog"
            );
        }
        let msg = sub.try_recv().expect("then the ordered backlog");
        assert_eq!(msg.topic, "block.text_ops");
    }

    /// One FIFO per subscription means global publish order is preserved
    /// ACROSS topics. Incident 4 was a terminal `block.status` patch going
    /// missing; the inverse hazard — a `Done` status overtaking the final text
    /// op — is what this pins.
    #[tokio::test]
    async fn ordered_lane_preserves_cross_topic_publish_order() {
        let bus: FlowBus<BlockFlow> = FlowBus::new(64);
        let mut sub = bus.subscribe("block.*");
        let ctx = ContextId::new();
        let id = BlockId::new(ctx, PrincipalId::new(), 1);
        let block = Arc::new(BlockSnapshot::text(id, None, Role::Model, ""));

        bus.publish(BlockFlow::Inserted {
            context_id: ctx,
            block,
            after_id: None,
            ops: Arc::from(Vec::<u8>::new()),
            source: OpSource::Local,
        });
        bus.publish(text_op(ctx, id, 1));
        bus.publish(text_op(ctx, id, 2));
        bus.publish(BlockFlow::StatusChanged {
            context_id: ctx,
            block_id: id,
            status: Status::Done,
            source: OpSource::Local,
        });

        let seen: Vec<&'static str> = std::iter::from_fn(|| sub.try_recv().map(|m| m.topic))
            .collect();
        assert_eq!(
            seen,
            vec![
                "block.inserted",
                "block.text_ops",
                "block.text_ops",
                "block.status"
            ],
            "a status patch never overtakes the text ops it follows"
        );
    }

    /// Two subscriptions of the same publishes get independent, each-contiguous
    /// sequence numbers. The contract a client asserts on: a gap can only mean
    /// "terminated or reconnected", never silent loss.
    #[tokio::test]
    async fn seq_is_per_subscription_and_starts_at_one() {
        let bus: FlowBus<BlockFlow> = FlowBus::new(64);
        let mut early = bus.subscribe("block.text_ops");
        let ctx = ContextId::new();
        let id = BlockId::new(ctx, PrincipalId::new(), 1);

        bus.publish(text_op(ctx, id, 1));
        bus.publish(text_op(ctx, id, 2));
        let mut late = bus.subscribe("block.text_ops");
        bus.publish(text_op(ctx, id, 3));

        let early_seqs: Vec<u64> = std::iter::from_fn(|| early.try_recv().map(|m| m.seq)).collect();
        let late_seqs: Vec<u64> = std::iter::from_fn(|| late.try_recv().map(|m| m.seq)).collect();
        assert_eq!(early_seqs, vec![1, 2, 3]);
        assert_eq!(
            late_seqs,
            vec![1],
            "a late subscriber starts its own count at 1 — seq is per-subscription"
        );
    }

    /// A terminated subscription that nobody has drained yet must be logged
    /// EXACTLY ONCE, however many publishes keep arriving. The bug this rework
    /// replaced announced itself once per text delta; reintroducing that noise
    /// on the termination path would be the same mistake wearing a new hat.
    #[tokio::test]
    async fn termination_is_announced_once_not_once_per_publish() {
        let bus: FlowBus<BlockFlow> = FlowBus::with_capacities(4, 4);
        let sub = bus.subscribe("block.text_ops");
        let ctx = ContextId::new();
        let id = BlockId::new(ctx, PrincipalId::new(), 1);

        // Overflow, then keep publishing at the dead subscriber.
        for i in 0..200 {
            bus.publish(text_op(ctx, id, i as u8));
        }
        assert!(sub.is_terminated());

        // The `logged` latch is what bounds the log volume; assert it directly
        // (the tracing output itself is not capturable here).
        let slot = sub.slot.as_ref().expect("a live slot");
        assert!(
            slot.state.lock().unwrap().logged,
            "the termination was announced"
        );

        // And the publisher still refuses to slow down or block on it.
        let started = std::time::Instant::now();
        for i in 0..1_000 {
            bus.publish(text_op(ctx, id, i as u8));
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "publishing past a dead subscriber must stay cheap"
        );
    }

    /// A dropped bus wakes its waiters instead of parking them forever, so a
    /// `while let Some(..) = sub.recv().await` consumer exits at shutdown.
    #[tokio::test]
    async fn dropping_the_bus_ends_waiting_subscriptions() {
        let bus: FlowBus<BlockFlow> = FlowBus::new(8);
        let mut sub = bus.subscribe("block.text_ops");
        let handle = tokio::spawn(async move { sub.recv().await.is_none() });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        drop(bus);
        let ended = tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("the waiter must wake when the bus goes away")
            .expect("task");
        assert!(ended);
    }

    /// The queue-depth knob is an env override, and a bad value is loud rather
    /// than silently substituted with something nobody asked for.
    #[test]
    fn queue_depth_default_is_generous() {
        assert_eq!(DEFAULT_ORDERED_QUEUE_DEPTH, 8192);
        assert_eq!(FLOW_QUEUE_DEPTH_ENV, "KAIJUTSU_FLOW_QUEUE_DEPTH");
        // Unset in the test process → the default.
        assert!(configured_queue_depth() >= 1);
    }

    // ====================================================================
    // Pure-function tests (no FlowBus dependency)
    // ====================================================================

}
