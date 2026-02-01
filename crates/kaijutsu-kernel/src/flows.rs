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
        /// The document ID.
        document_id: String,
        /// Full snapshot of the inserted block.
        block: BlockSnapshot,
        /// Block to insert after (None = beginning).
        after_id: Option<BlockId>,
        /// CRDT operations that created this block (for sync).
        /// Clients should merge these ops instead of creating their own.
        ops: Vec<u8>,
        /// Origin of this operation (Local or Remote).
        #[serde(default)]
        source: OpSource,
    },

    /// CRDT operations for a block's text content.
    /// Clients should use merge_ops() to apply these.
    TextOps {
        /// The document ID.
        document_id: String,
        /// The block that was edited.
        block_id: BlockId,
        /// Serialized CRDT operations (diamond-types format).
        ops: Vec<u8>,
        /// Origin of this operation (Local or Remote).
        #[serde(default)]
        source: OpSource,
    },

    /// A block was deleted.
    Deleted {
        /// The document ID.
        document_id: String,
        /// The block that was deleted.
        block_id: BlockId,
        /// Origin of this operation (Local or Remote).
        #[serde(default)]
        source: OpSource,
    },

    /// Block status changed.
    StatusChanged {
        /// The document ID.
        document_id: String,
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
        /// The document ID.
        document_id: String,
        /// The block whose collapsed state changed.
        block_id: BlockId,
        /// New collapsed state.
        collapsed: bool,
        /// Origin of this operation (Local or Remote).
        #[serde(default)]
        source: OpSource,
    },

    /// Block was moved to a new position.
    Moved {
        /// The document ID.
        document_id: String,
        /// The block that was moved.
        block_id: BlockId,
        /// New position (after this block, None = beginning).
        after_id: Option<BlockId>,
        /// Origin of this operation (Local or Remote).
        #[serde(default)]
        source: OpSource,
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
            Self::Moved { .. } => "block.moved",
        }
    }

    /// Get the document ID for this event.
    pub fn document_id(&self) -> &str {
        match self {
            Self::Inserted { document_id, .. }
            | Self::TextOps { document_id, .. }
            | Self::Deleted { document_id, .. }
            | Self::StatusChanged { document_id, .. }
            | Self::CollapsedChanged { document_id, .. }
            | Self::Moved { document_id, .. } => document_id,
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

    /// Get the source of this event (Local or Remote).
    pub fn source(&self) -> OpSource {
        match self {
            Self::Inserted { source, .. }
            | Self::TextOps { source, .. }
            | Self::Deleted { source, .. }
            | Self::StatusChanged { source, .. }
            | Self::CollapsedChanged { source, .. }
            | Self::Moved { source, .. } => *source,
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
// Resource Flow Events
// ============================================================================

/// Resource-related flow events for MCP resource push notifications.
///
/// These events are emitted when MCP servers notify us of resource changes.
/// The push-first model means we invalidate cache and broadcast immediately.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ResourceFlow {
    /// A resource's content was updated.
    Updated {
        /// The MCP server name (e.g., "git", "files").
        server: String,
        /// The resource URI.
        uri: String,
        /// Serialized resource contents (if available from notification).
        /// None means the content should be fetched on demand.
        content: Option<Vec<u8>>,
        /// Origin of this operation.
        source: OpSource,
    },

    /// The server's resource list changed (resources added or removed).
    ListChanged {
        /// The MCP server name.
        server: String,
        /// Serialized list of resources (if available).
        /// None means the list should be re-fetched.
        resources: Option<Vec<u8>>,
        /// Origin of this operation.
        source: OpSource,
    },

    /// A subscription was added for a resource.
    Subscribed {
        /// The MCP server name.
        server: String,
        /// The resource URI.
        uri: String,
        /// Identifier of the subscriber (e.g., kernel ID, client ID).
        subscriber_id: String,
    },

    /// A subscription was removed for a resource.
    Unsubscribed {
        /// The MCP server name.
        server: String,
        /// The resource URI.
        uri: String,
        /// Identifier of the subscriber.
        subscriber_id: String,
    },
}

impl ResourceFlow {
    /// Get the subject string for this event.
    pub fn subject(&self) -> &'static str {
        match self {
            Self::Updated { .. } => "resource.updated",
            Self::ListChanged { .. } => "resource.list_changed",
            Self::Subscribed { .. } => "resource.subscribed",
            Self::Unsubscribed { .. } => "resource.unsubscribed",
        }
    }

    /// Get the server name for this event.
    pub fn server(&self) -> &str {
        match self {
            Self::Updated { server, .. }
            | Self::ListChanged { server, .. }
            | Self::Subscribed { server, .. }
            | Self::Unsubscribed { server, .. } => server,
        }
    }

    /// Get the source of this event (Local or Remote).
    pub fn source(&self) -> OpSource {
        match self {
            Self::Updated { source, .. } | Self::ListChanged { source, .. } => *source,
            // Subscribed/Unsubscribed are always local operations
            Self::Subscribed { .. } | Self::Unsubscribed { .. } => OpSource::Local,
        }
    }
}

impl HasSubject for ResourceFlow {
    fn subject(&self) -> &str {
        ResourceFlow::subject(self)
    }
}

// ============================================================================
// Progress Flow Events
// ============================================================================

/// Progress notification events from MCP servers during long-running operations.
///
/// These events are emitted when MCP servers send progress updates during tool calls
/// or other long operations. The push model ensures clients can show real-time progress.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ProgressFlow {
    /// Progress update during an operation.
    Update {
        /// The MCP server name.
        server: String,
        /// Progress token identifying the operation.
        token: String,
        /// Current progress value.
        progress: f64,
        /// Total value if known.
        total: Option<f64>,
        /// Human-readable message.
        message: Option<String>,
    },
}

impl ProgressFlow {
    /// Get the subject string for this event.
    pub fn subject(&self) -> &'static str {
        match self {
            Self::Update { .. } => "progress.update",
        }
    }

    /// Get the server name for this event.
    pub fn server(&self) -> &str {
        match self {
            Self::Update { server, .. } => server,
        }
    }
}

impl HasSubject for ProgressFlow {
    fn subject(&self) -> &str {
        ProgressFlow::subject(self)
    }
}

// ============================================================================
// Elicitation Flow Events
// ============================================================================

/// Action to take in response to an elicitation request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ElicitationAction {
    /// Accept the elicitation with provided content.
    Accept,
    /// Decline the elicitation.
    Decline,
    /// Cancel the elicitation.
    Cancel,
}

impl Default for ElicitationAction {
    fn default() -> Self {
        Self::Decline
    }
}

impl std::fmt::Display for ElicitationAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Accept => write!(f, "accept"),
            Self::Decline => write!(f, "decline"),
            Self::Cancel => write!(f, "cancel"),
        }
    }
}

impl std::str::FromStr for ElicitationAction {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "accept" => Ok(Self::Accept),
            "decline" => Ok(Self::Decline),
            "cancel" => Ok(Self::Cancel),
            _ => Err(format!("invalid elicitation action: {}", s)),
        }
    }
}

/// Response to an elicitation request.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ElicitationResponse {
    /// Action taken (accept, decline, cancel).
    pub action: ElicitationAction,
    /// Content provided if action is Accept.
    pub content: Option<serde_json::Value>,
}

/// Elicitation request events from MCP servers that require client response.
///
/// These events are emitted when MCP servers need user input or confirmation.
/// The server waits for a response through the provided oneshot channel.
///
/// Note: The response_tx is not serializable, so ElicitationFlow itself
/// uses a separate request_id for tracking. The actual channel is managed
/// by the McpServerPool.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ElicitationFlow {
    /// Elicitation request from a server.
    Request {
        /// Unique identifier for this request.
        request_id: String,
        /// The MCP server name.
        server: String,
        /// Human-readable message/prompt.
        message: String,
        /// JSON Schema for expected response format (if any).
        schema: Option<serde_json::Value>,
    },
}

impl ElicitationFlow {
    /// Get the subject string for this event.
    pub fn subject(&self) -> &'static str {
        match self {
            Self::Request { .. } => "elicitation.request",
        }
    }

    /// Get the server name for this event.
    pub fn server(&self) -> &str {
        match self {
            Self::Request { server, .. } => server,
        }
    }

    /// Get the request ID for this event.
    pub fn request_id(&self) -> &str {
        match self {
            Self::Request { request_id, .. } => request_id,
        }
    }
}

impl HasSubject for ElicitationFlow {
    fn subject(&self) -> &str {
        ElicitationFlow::subject(self)
    }
}

// ============================================================================
// Logging Flow Events
// ============================================================================

/// Log message events from MCP servers.
///
/// While logging can be polled, this provides an optional push mechanism
/// for clients that want real-time log streaming.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum LoggingFlow {
    /// Log message from an MCP server.
    Message {
        /// The MCP server name.
        server: String,
        /// Log level (debug, info, warning, error).
        level: String,
        /// Logger name (if available).
        logger: Option<String>,
        /// Log data (typically JSON).
        data: serde_json::Value,
    },
}

impl LoggingFlow {
    /// Get the subject string for this event.
    pub fn subject(&self) -> &'static str {
        match self {
            Self::Message { .. } => "logging.message",
        }
    }

    /// Get the server name for this event.
    pub fn server(&self) -> &str {
        match self {
            Self::Message { server, .. } => server,
        }
    }
}

impl HasSubject for LoggingFlow {
    fn subject(&self) -> &str {
        LoggingFlow::subject(self)
    }
}

// ============================================================================
// Shared FlowBus Handle
// ============================================================================

/// Thread-safe handle to a BlockFlow bus.
pub type SharedBlockFlowBus = Arc<FlowBus<BlockFlow>>;

/// Thread-safe handle to a ResourceFlow bus.
pub type SharedResourceFlowBus = Arc<FlowBus<ResourceFlow>>;

/// Thread-safe handle to a ProgressFlow bus.
pub type SharedProgressFlowBus = Arc<FlowBus<ProgressFlow>>;

/// Thread-safe handle to an ElicitationFlow bus.
pub type SharedElicitationFlowBus = Arc<FlowBus<ElicitationFlow>>;

/// Thread-safe handle to a LoggingFlow bus.
pub type SharedLoggingFlowBus = Arc<FlowBus<LoggingFlow>>;

/// Create a new shared block flow bus.
pub fn shared_block_flow_bus(capacity: usize) -> SharedBlockFlowBus {
    Arc::new(FlowBus::new(capacity))
}

/// Create a new shared resource flow bus.
pub fn shared_resource_flow_bus(capacity: usize) -> SharedResourceFlowBus {
    Arc::new(FlowBus::new(capacity))
}

/// Create a new shared progress flow bus.
pub fn shared_progress_flow_bus(capacity: usize) -> SharedProgressFlowBus {
    Arc::new(FlowBus::new(capacity))
}

/// Create a new shared elicitation flow bus.
pub fn shared_elicitation_flow_bus(capacity: usize) -> SharedElicitationFlowBus {
    Arc::new(FlowBus::new(capacity))
}

/// Create a new shared logging flow bus.
pub fn shared_logging_flow_bus(capacity: usize) -> SharedLoggingFlowBus {
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
        let id = BlockId::new("doc-1", "agent", 1);
        let block = BlockSnapshot::text(id.clone(), None, Role::User, "test", "author");

        assert_eq!(
            BlockFlow::Inserted {
                document_id: "doc-1".into(),
                block,
                after_id: None,
                ops: vec![],
                source: OpSource::Local,
            }
            .subject(),
            "block.inserted"
        );

        assert_eq!(
            BlockFlow::Deleted {
                document_id: "doc-1".into(),
                block_id: id.clone(),
                source: OpSource::Local,
            }
            .subject(),
            "block.deleted"
        );

        assert_eq!(
            BlockFlow::StatusChanged {
                document_id: "doc-1".into(),
                block_id: id.clone(),
                status: Status::Done,
                source: OpSource::Local,
            }
            .subject(),
            "block.status"
        );

        assert_eq!(
            BlockFlow::CollapsedChanged {
                document_id: "doc-1".into(),
                block_id: id.clone(),
                collapsed: true,
                source: OpSource::Local,
            }
            .subject(),
            "block.collapsed"
        );

        assert_eq!(
            BlockFlow::Moved {
                document_id: "doc-1".into(),
                block_id: id,
                after_id: None,
                source: OpSource::Local,
            }
            .subject(),
            "block.moved"
        );
    }

    #[tokio::test]
    async fn test_flow_bus_publish_subscribe() {
        let bus: FlowBus<BlockFlow> = FlowBus::new(16);
        let mut sub = bus.subscribe("block.*");

        let id = BlockId::new("doc-1", "agent", 1);
        let block = BlockSnapshot::text(id.clone(), None, Role::User, "test", "author");

        // Publish in background task
        let bus_clone = bus.clone();
        let block_clone = block.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            bus_clone.publish(BlockFlow::Inserted {
                document_id: "doc-1".into(),
                block: block_clone,
                after_id: None,
                ops: vec![],
                source: OpSource::Local,
            });
        });

        // Should receive the message
        let msg = tokio::time::timeout(std::time::Duration::from_millis(100), sub.recv())
            .await
            .expect("timeout")
            .expect("no message");

        assert_eq!(msg.subject, "block.inserted");
        match msg.payload {
            BlockFlow::Inserted { document_id, .. } => assert_eq!(document_id, "doc-1"),
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

        let id = BlockId::new("doc-1", "agent", 1);
        let block = BlockSnapshot::text(id.clone(), None, Role::User, "test", "author");

        // Publish an insertion
        bus.publish(BlockFlow::Inserted {
            document_id: "doc-1".into(),
            block,
            after_id: None,
            ops: vec![],
            source: OpSource::Local,
        });

        // Publish a status change
        bus.publish(BlockFlow::StatusChanged {
            document_id: "doc-1".into(),
            block_id: id,
            status: Status::Done,
            source: OpSource::Local,
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

    #[test]
    fn test_progress_flow_subjects() {
        let flow = ProgressFlow::Update {
            server: "test-server".into(),
            token: "token-123".into(),
            progress: 50.0,
            total: Some(100.0),
            message: Some("Processing...".into()),
        };

        assert_eq!(flow.subject(), "progress.update");
        assert_eq!(flow.server(), "test-server");
    }

    #[tokio::test]
    async fn test_progress_flow_publish_subscribe() {
        let bus = shared_progress_flow_bus(16);
        let mut sub = bus.subscribe("progress.*");

        bus.publish(ProgressFlow::Update {
            server: "git".into(),
            token: "op-1".into(),
            progress: 25.0,
            total: Some(100.0),
            message: Some("Cloning...".into()),
        });

        let msg = sub.try_recv().expect("should have message");
        assert_eq!(msg.subject, "progress.update");
        match msg.payload {
            ProgressFlow::Update { server, progress, .. } => {
                assert_eq!(server, "git");
                assert!((progress - 25.0).abs() < f64::EPSILON);
            }
        }
    }

    #[test]
    fn test_elicitation_action_parsing() {
        assert_eq!("accept".parse::<ElicitationAction>().unwrap(), ElicitationAction::Accept);
        assert_eq!("decline".parse::<ElicitationAction>().unwrap(), ElicitationAction::Decline);
        assert_eq!("cancel".parse::<ElicitationAction>().unwrap(), ElicitationAction::Cancel);
        assert_eq!("ACCEPT".parse::<ElicitationAction>().unwrap(), ElicitationAction::Accept);
        assert!("invalid".parse::<ElicitationAction>().is_err());
    }

    #[test]
    fn test_elicitation_flow_subjects() {
        let flow = ElicitationFlow::Request {
            request_id: "req-123".into(),
            server: "auth-server".into(),
            message: "Please confirm".into(),
            schema: None,
        };

        assert_eq!(flow.subject(), "elicitation.request");
        assert_eq!(flow.server(), "auth-server");
        assert_eq!(flow.request_id(), "req-123");
    }

    #[tokio::test]
    async fn test_elicitation_flow_publish_subscribe() {
        let bus = shared_elicitation_flow_bus(16);
        let mut sub = bus.subscribe("elicitation.*");

        bus.publish(ElicitationFlow::Request {
            request_id: "req-1".into(),
            server: "oauth".into(),
            message: "Enter code".into(),
            schema: Some(serde_json::json!({"type": "string"})),
        });

        let msg = sub.try_recv().expect("should have message");
        assert_eq!(msg.subject, "elicitation.request");
        match msg.payload {
            ElicitationFlow::Request { request_id, server, message, schema } => {
                assert_eq!(request_id, "req-1");
                assert_eq!(server, "oauth");
                assert_eq!(message, "Enter code");
                assert!(schema.is_some());
            }
        }
    }

    #[test]
    fn test_logging_flow_subjects() {
        let flow = LoggingFlow::Message {
            server: "debug-server".into(),
            level: "info".into(),
            logger: Some("main".into()),
            data: serde_json::json!({"event": "started"}),
        };

        assert_eq!(flow.subject(), "logging.message");
        assert_eq!(flow.server(), "debug-server");
    }

    #[tokio::test]
    async fn test_logging_flow_publish_subscribe() {
        let bus = shared_logging_flow_bus(16);
        let mut sub = bus.subscribe("logging.*");

        bus.publish(LoggingFlow::Message {
            server: "app".into(),
            level: "warning".into(),
            logger: None,
            data: serde_json::json!("rate limit approaching"),
        });

        let msg = sub.try_recv().expect("should have message");
        assert_eq!(msg.subject, "logging.message");
        match msg.payload {
            LoggingFlow::Message { server, level, logger, .. } => {
                assert_eq!(server, "app");
                assert_eq!(level, "warning");
                assert!(logger.is_none());
            }
        }
    }

    #[test]
    fn test_elicitation_response_default() {
        let response = ElicitationResponse::default();
        assert_eq!(response.action, ElicitationAction::Decline);
        assert!(response.content.is_none());
    }
}
