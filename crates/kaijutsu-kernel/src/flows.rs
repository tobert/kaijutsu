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
//!     println!("Got: {}", msg.subject);
//! }
//! ```

use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use kaijutsu_crdt::{BlockId, BlockKind, BlockSnapshot, Status};

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

/// Trait for payloads that know their subject.
pub trait HasSubject {
    /// Get the subject string for this payload.
    fn subject(&self) -> &str;
}

/// A message published to the flow bus.
#[derive(Clone, Debug)]
pub struct FlowMessage<T> {
    /// The subject (derived from payload).
    pub subject: String,
    /// The payload data.
    pub payload: T,
    /// When this message was created.
    pub timestamp: Instant,
    /// Optional sender identifier (agent_id or user_id).
    pub sender: Option<String>,
}

impl<T: HasSubject> FlowMessage<T> {
    /// Create a new flow message.
    pub fn new(payload: T) -> Self {
        let subject = payload.subject().to_string();
        Self {
            subject,
            payload,
            timestamp: Instant::now(),
            sender: None,
        }
    }

    /// Create a new flow message with sender.
    pub fn with_sender(payload: T, sender: impl Into<String>) -> Self {
        let subject = payload.subject().to_string();
        Self {
            subject,
            payload,
            timestamp: Instant::now(),
            sender: Some(sender.into()),
        }
    }
}

// ============================================================================
// Block Flow Events
// ============================================================================

/// Block-related flow events.
///
/// These events are emitted by the BlockStore when blocks are modified.
/// Each variant corresponds to a specific block operation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum BlockFlow {
    /// A new block was inserted.
    Inserted {
        /// The cell/document ID.
        cell_id: String,
        /// Full snapshot of the inserted block.
        block: BlockSnapshot,
        /// Block to insert after (None = beginning).
        after_id: Option<BlockId>,
    },

    /// Text was edited within a block (legacy position-based, deprecated).
    /// Use TextOps for proper CRDT sync.
    #[deprecated(note = "Use TextOps for CRDT-based sync")]
    Edited {
        /// The cell/document ID.
        cell_id: String,
        /// The block that was edited.
        block_id: BlockId,
        /// Character position where edit starts.
        pos: u64,
        /// Text to insert at position.
        insert: String,
        /// Number of characters to delete at position.
        delete: u64,
    },

    /// CRDT operations for a block's text content.
    /// Clients should use merge_ops() to apply these.
    TextOps {
        /// The cell/document ID.
        cell_id: String,
        /// The block that was edited.
        block_id: BlockId,
        /// Serialized CRDT operations (diamond-types format).
        ops: Vec<u8>,
    },

    /// A block was deleted.
    Deleted {
        /// The cell/document ID.
        cell_id: String,
        /// The block that was deleted.
        block_id: BlockId,
    },

    /// Block status changed.
    StatusChanged {
        /// The cell/document ID.
        cell_id: String,
        /// The block whose status changed.
        block_id: BlockId,
        /// The new status.
        status: Status,
    },

    /// Block collapsed state changed (for thinking blocks).
    CollapsedChanged {
        /// The cell/document ID.
        cell_id: String,
        /// The block whose collapsed state changed.
        block_id: BlockId,
        /// New collapsed state.
        collapsed: bool,
    },

    /// Block was moved to a new position.
    Moved {
        /// The cell/document ID.
        cell_id: String,
        /// The block that was moved.
        block_id: BlockId,
        /// New position (after this block, None = beginning).
        after_id: Option<BlockId>,
    },
}

impl BlockFlow {
    /// Get the subject string for this event.
    #[allow(deprecated)]
    pub fn subject(&self) -> &'static str {
        match self {
            Self::Inserted { .. } => "block.inserted",
            Self::Edited { .. } => "block.edited",
            Self::TextOps { .. } => "block.text_ops",
            Self::Deleted { .. } => "block.deleted",
            Self::StatusChanged { .. } => "block.status",
            Self::CollapsedChanged { .. } => "block.collapsed",
            Self::Moved { .. } => "block.moved",
        }
    }

    /// Get the cell ID for this event.
    #[allow(deprecated)]
    pub fn cell_id(&self) -> &str {
        match self {
            Self::Inserted { cell_id, .. }
            | Self::Edited { cell_id, .. }
            | Self::TextOps { cell_id, .. }
            | Self::Deleted { cell_id, .. }
            | Self::StatusChanged { cell_id, .. }
            | Self::CollapsedChanged { cell_id, .. }
            | Self::Moved { cell_id, .. } => cell_id,
        }
    }

    /// Get the block ID for this event (if applicable).
    #[allow(deprecated)]
    pub fn block_id(&self) -> Option<&BlockId> {
        match self {
            Self::Inserted { block, .. } => Some(&block.id),
            Self::Edited { block_id, .. }
            | Self::TextOps { block_id, .. }
            | Self::Deleted { block_id, .. }
            | Self::StatusChanged { block_id, .. }
            | Self::CollapsedChanged { block_id, .. }
            | Self::Moved { block_id, .. } => Some(block_id),
        }
    }

    /// Get the block kind for Inserted events.
    pub fn block_kind(&self) -> Option<BlockKind> {
        match self {
            Self::Inserted { block, .. } => Some(block.kind),
            _ => None,
        }
    }
}

impl HasSubject for BlockFlow {
    fn subject(&self) -> &str {
        BlockFlow::subject(self)
    }
}

// ============================================================================
// FlowBus
// ============================================================================

/// Type-parameterized pub/sub bus for a specific flow domain.
///
/// Uses a broadcast channel internally for multi-subscriber delivery.
/// Subscribers receive only messages matching their pattern.
#[derive(Debug)]
pub struct FlowBus<T: Clone + Send + 'static> {
    tx: broadcast::Sender<FlowMessage<T>>,
    capacity: usize,
}

impl<T: Clone + Send + 'static> FlowBus<T> {
    /// Create a new flow bus with the given channel capacity.
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx, capacity }
    }

    /// Get the channel capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Get the number of active subscribers.
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl<T: Clone + Send + HasSubject + 'static> FlowBus<T> {
    /// Publish a payload to the bus.
    ///
    /// The subject is derived from the payload via HasSubject.
    /// Returns the number of subscribers that received the message.
    pub fn publish(&self, payload: T) -> usize {
        let msg = FlowMessage::new(payload);
        self.tx.send(msg).unwrap_or(0)
    }

    /// Publish a payload with sender information.
    pub fn publish_with_sender(&self, payload: T, sender: impl Into<String>) -> usize {
        let msg = FlowMessage::with_sender(payload, sender);
        self.tx.send(msg).unwrap_or(0)
    }

    /// Subscribe to messages matching a pattern.
    ///
    /// The pattern uses NATS-style wildcards:
    /// - `*` matches exactly one token
    /// - `>` matches one or more tokens (only at end)
    pub fn subscribe(&self, pattern: &str) -> Subscription<T> {
        Subscription {
            pattern: pattern.to_string(),
            rx: self.tx.subscribe(),
        }
    }
}

impl<T: Clone + Send + 'static> Clone for FlowBus<T> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            capacity: self.capacity,
        }
    }
}

// ============================================================================
// Subscription
// ============================================================================

/// A subscription to a FlowBus with pattern filtering.
///
/// Only messages whose subject matches the subscription pattern are delivered.
pub struct Subscription<T: Clone> {
    pattern: String,
    rx: broadcast::Receiver<FlowMessage<T>>,
}

impl<T: Clone> Subscription<T> {
    /// Get the subscription pattern.
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// Receive the next matching message, waiting if necessary.
    ///
    /// Returns None if the channel is closed.
    pub async fn recv(&mut self) -> Option<FlowMessage<T>> {
        loop {
            match self.rx.recv().await {
                Ok(msg) => {
                    if matches_pattern(&self.pattern, &msg.subject) {
                        return Some(msg);
                    }
                    // Message didn't match pattern, continue waiting
                }
                Err(broadcast::error::RecvError::Closed) => return None,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    // We fell behind, log and continue
                    tracing::warn!(
                        pattern = %self.pattern,
                        lagged = n,
                        "Flow subscription lagged behind"
                    );
                }
            }
        }
    }

    /// Try to receive the next matching message without blocking.
    ///
    /// Returns None if no matching message is available.
    pub fn try_recv(&mut self) -> Option<FlowMessage<T>> {
        loop {
            match self.rx.try_recv() {
                Ok(msg) => {
                    if matches_pattern(&self.pattern, &msg.subject) {
                        return Some(msg);
                    }
                    // Message didn't match pattern, try again
                }
                Err(broadcast::error::TryRecvError::Empty) => return None,
                Err(broadcast::error::TryRecvError::Closed) => return None,
                Err(broadcast::error::TryRecvError::Lagged(n)) => {
                    tracing::warn!(
                        pattern = %self.pattern,
                        lagged = n,
                        "Flow subscription lagged behind"
                    );
                    // Continue trying to receive
                }
            }
        }
    }
}

impl<T: Clone> std::fmt::Debug for Subscription<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription")
            .field("pattern", &self.pattern)
            .finish_non_exhaustive()
    }
}

// ============================================================================
// Shared FlowBus Handle
// ============================================================================

/// Thread-safe handle to a BlockFlow bus.
pub type SharedBlockFlowBus = Arc<FlowBus<BlockFlow>>;

/// Create a new shared block flow bus.
pub fn shared_block_flow_bus(capacity: usize) -> SharedBlockFlowBus {
    Arc::new(FlowBus::new(capacity))
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_crdt::Role;

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

    #[test]
    fn test_block_flow_subjects() {
        let id = BlockId::new("cell-1", "agent", 1);
        let block = BlockSnapshot::text(id.clone(), None, Role::User, "test", "author");

        assert_eq!(
            BlockFlow::Inserted {
                cell_id: "cell-1".into(),
                block,
                after_id: None
            }
            .subject(),
            "block.inserted"
        );

        assert_eq!(
            BlockFlow::Edited {
                cell_id: "cell-1".into(),
                block_id: id.clone(),
                pos: 0,
                insert: "x".into(),
                delete: 0
            }
            .subject(),
            "block.edited"
        );

        assert_eq!(
            BlockFlow::Deleted {
                cell_id: "cell-1".into(),
                block_id: id.clone()
            }
            .subject(),
            "block.deleted"
        );

        assert_eq!(
            BlockFlow::StatusChanged {
                cell_id: "cell-1".into(),
                block_id: id.clone(),
                status: Status::Done
            }
            .subject(),
            "block.status"
        );

        assert_eq!(
            BlockFlow::CollapsedChanged {
                cell_id: "cell-1".into(),
                block_id: id.clone(),
                collapsed: true
            }
            .subject(),
            "block.collapsed"
        );

        assert_eq!(
            BlockFlow::Moved {
                cell_id: "cell-1".into(),
                block_id: id,
                after_id: None
            }
            .subject(),
            "block.moved"
        );
    }

    #[tokio::test]
    async fn test_flow_bus_publish_subscribe() {
        let bus: FlowBus<BlockFlow> = FlowBus::new(16);
        let mut sub = bus.subscribe("block.*");

        let id = BlockId::new("cell-1", "agent", 1);
        let block = BlockSnapshot::text(id.clone(), None, Role::User, "test", "author");

        // Publish in background task
        let bus_clone = bus.clone();
        let block_clone = block.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            bus_clone.publish(BlockFlow::Inserted {
                cell_id: "cell-1".into(),
                block: block_clone,
                after_id: None,
            });
        });

        // Should receive the message
        let msg = tokio::time::timeout(std::time::Duration::from_millis(100), sub.recv())
            .await
            .expect("timeout")
            .expect("no message");

        assert_eq!(msg.subject, "block.inserted");
        match msg.payload {
            BlockFlow::Inserted { cell_id, .. } => assert_eq!(cell_id, "cell-1"),
            _ => panic!("wrong event type"),
        }
    }

    #[tokio::test]
    async fn test_subscription_pattern_filtering() {
        let bus: FlowBus<BlockFlow> = FlowBus::new(16);

        // Subscribe only to insertions
        let mut insert_sub = bus.subscribe("block.inserted");
        // Subscribe only to status changes
        let mut status_sub = bus.subscribe("block.status");

        let id = BlockId::new("cell-1", "agent", 1);
        let block = BlockSnapshot::text(id.clone(), None, Role::User, "test", "author");

        // Publish an insertion
        bus.publish(BlockFlow::Inserted {
            cell_id: "cell-1".into(),
            block,
            after_id: None,
        });

        // Publish a status change
        bus.publish(BlockFlow::StatusChanged {
            cell_id: "cell-1".into(),
            block_id: id,
            status: Status::Done,
        });

        // insert_sub should only get insertion
        let msg = insert_sub.try_recv().expect("should have message");
        assert_eq!(msg.subject, "block.inserted");
        assert!(insert_sub.try_recv().is_none());

        // status_sub should only get status change
        let msg = status_sub.try_recv().expect("should have message");
        assert_eq!(msg.subject, "block.status");
        assert!(status_sub.try_recv().is_none());
    }

    #[test]
    fn test_shared_block_flow_bus() {
        let bus = shared_block_flow_bus(1024);
        assert_eq!(bus.capacity(), 1024);
        assert_eq!(bus.subscriber_count(), 0);

        let _sub = bus.subscribe("block.*");
        assert_eq!(bus.subscriber_count(), 1);
    }
}
