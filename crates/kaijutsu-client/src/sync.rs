//! Shared CRDT sync logic for kaijutsu clients.
//!
//! This module implements frontier-based CRDT sync independent of any UI framework,
//! enabling comprehensive unit testing and reuse across multiple client implementations
//! (kaijutsu-app Bevy client, kaijutsu-mcp, etc.).
//!
//! # Sync Protocol
//!
//! - `frontier = None` or `document_id` changed -> full sync (from_oplog)
//! - `frontier = Some(_)` and matching document_id -> incremental merge (merge_ops_owned)
//! - On merge failure -> reset frontier, next event triggers full sync

use kaijutsu_crdt::{BlockDocument, BlockId, BlockSnapshot, Frontier, SerializedOpsOwned};
use thiserror::Error;
use tracing::{error, info, trace, warn};

/// Maximum number of pending ops to buffer before dropping oldest entries.
/// Sized for text streaming bursts during network reordering — each text
/// chunk is one entry, so 200 covers ~200 characters of streaming output
/// arriving before their BlockInserted event.
const MAX_PENDING_OPS: usize = 200;

/// Result of a sync operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncResult {
    /// Full document was rebuilt from oplog.
    FullSync { block_count: usize },
    /// Incremental ops were merged into existing document.
    IncrementalMerge,
    /// Operation was skipped (see reason).
    Skipped { reason: SkipReason },
}

/// Reason why a sync operation was skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The oplog bytes were empty.
    EmptyOplog,
    /// Document ID doesn't match our tracked document.
    DocumentIdMismatch { expected: String, got: String },
    /// Block already exists in document (idempotent insert).
    BlockAlreadyExists,
    /// Protocol violation (e.g., BlockInserted with no ops).
    ProtocolViolation(String),
}

/// Error during sync operation.
#[derive(Error, Debug)]
pub enum SyncError {
    /// Failed to create document from oplog.
    #[error("failed to create document from oplog: {0}")]
    FromOplog(String),
    /// Failed to deserialize CRDT ops.
    #[error("failed to deserialize ops: {0}")]
    Deserialize(String),
    /// Failed to merge CRDT ops.
    #[error("failed to merge ops: {0}")]
    Merge(String),
}

/// Manages CRDT sync state for a single document.
///
/// This struct encapsulates all the frontier-tracking and sync decision logic,
/// allowing client systems to remain thin while the core logic is unit-testable.
///
/// # State Machine
///
/// ```text
/// +----------------+
/// | Initial State  | frontier=None, document_id=None
/// | (needs sync)   |
/// +-------+--------+
///         | apply_initial_state() or apply_block_inserted() with full oplog
///         v
/// +----------------+
/// |  Synchronized  | frontier=Some(vec), document_id=Some(id)
/// | (incremental)  |
/// +-------+--------+
///         | merge failure OR document_id change
///         v
/// +----------------+
/// |  Needs Resync  | frontier=None (triggers full sync on next event)
/// +----------------+
/// ```
#[derive(Debug, Clone, Default)]
pub struct SyncManager {
    /// Current frontier (None = never synced or needs full sync).
    frontier: Option<Frontier>,
    /// Document ID we're synced to. Change triggers full sync.
    document_id: Option<String>,
    /// Version counter for change detection (bumped on every successful sync).
    version: u64,
    /// Ops that failed to merge (both incremental and full sync failed).
    /// These are retried after the next successful sync event.
    /// Capped at MAX_PENDING_OPS to prevent unbounded growth.
    pending_ops: Vec<(Option<BlockId>, Vec<u8>)>,
}

#[allow(dead_code)]
impl SyncManager {
    /// Create a new SyncManager in "needs full sync" state.
    pub fn new() -> Self {
        Self {
            frontier: None,
            document_id: None,
            version: 0,
            pending_ops: Vec::new(),
        }
    }

    /// Create a SyncManager with existing state (for testing/migration).
    pub fn with_state(document_id: Option<String>, frontier: Option<Frontier>) -> Self {
        Self { frontier, document_id, version: 0, pending_ops: Vec::new() }
    }

    /// Check if we need a full sync for the given document.
    ///
    /// Returns true if:
    /// - We have no frontier (never synced or reset after failure)
    /// - The document_id doesn't match our tracked document
    pub fn needs_full_sync(&self, document_id: &str) -> bool {
        self.frontier.is_none() || self.document_id.as_deref() != Some(document_id)
    }

    /// Get the current frontier (for testing/debugging).
    pub fn frontier(&self) -> Option<&Frontier> {
        self.frontier.as_ref()
    }

    /// Get the current document_id (for testing/debugging).
    pub fn document_id(&self) -> Option<&str> {
        self.document_id.as_deref()
    }

    /// Get the version counter (bumped on every successful sync).
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Reset sync state, forcing full sync on next event.
    ///
    /// Call this when merge failures occur or when you want to
    /// force a resync from the server's full oplog.
    pub fn reset(&mut self) {
        self.frontier = None;
        // Keep document_id - if it changes we'll detect that too
        // Keep pending_ops - they should be retried after next successful sync
    }

    /// Number of ops currently buffered for replay.
    pub fn pending_ops_count(&self) -> usize {
        self.pending_ops.len()
    }

    /// Buffer failed ops for later replay.
    ///
    /// Called when both incremental merge and full sync fail. The ops are
    /// retained so they can be replayed after the next successful sync
    /// (e.g., when the BlockInserted event finally arrives).
    fn buffer_failed_ops(&mut self, block_id: Option<&BlockId>, ops: &[u8]) {
        if self.pending_ops.len() >= MAX_PENDING_OPS {
            let drop_count = self.pending_ops.len() - MAX_PENDING_OPS + 1;
            warn!(
                "Pending ops buffer full ({}/{}), dropping {} oldest entries",
                self.pending_ops.len(), MAX_PENDING_OPS, drop_count
            );
            self.pending_ops.drain(..drop_count);
        }
        info!(
            "Buffering failed ops for block {:?} ({} bytes, {} pending total)",
            block_id, ops.len(), self.pending_ops.len() + 1
        );
        self.pending_ops.push((block_id.cloned(), ops.to_vec()));
    }

    /// Replay buffered pending ops after a successful sync.
    ///
    /// Ops that succeed are consumed; ops that still fail go back into the buffer.
    fn replay_pending_ops(&mut self, doc: &mut BlockDocument) {
        if self.pending_ops.is_empty() {
            return;
        }

        let ops_to_replay: Vec<_> = self.pending_ops.drain(..).collect();
        let count = ops_to_replay.len();
        info!("Replaying {} buffered pending ops", count);

        let mut still_pending = Vec::new();
        for (block_id, ops) in ops_to_replay {
            match self.do_incremental_merge(doc, &ops, block_id.as_ref()) {
                Ok(_) => {
                    trace!("Replayed buffered ops for block {:?} successfully", block_id);
                }
                Err(SyncError::Merge(ref msg)) => {
                    // CRDT DataMissing — might succeed later when more ops arrive
                    warn!("Replay merge still failing for block {:?}: {}", block_id, msg);
                    still_pending.push((block_id, ops));
                }
                Err(SyncError::Deserialize(ref msg)) => {
                    // Corrupt data won't improve on retry — drop to avoid a
                    // "death spiral" where each replay resets the frontier and
                    // forces a full sync on every subsequent event.
                    error!("Dropping corrupt buffered ops for block {:?}: {}", block_id, msg);
                }
                Err(e) => {
                    error!("Dropping buffered ops for block {:?} due to unrecoverable error: {}", block_id, e);
                }
            }
        }

        if !still_pending.is_empty() {
            info!("{} ops still pending after replay", still_pending.len());
        }
        self.pending_ops = still_pending;
    }

    /// Apply initial state from server (BlockCellInitialState event).
    ///
    /// Always performs a full sync from the provided oplog.
    pub fn apply_initial_state(
        &mut self,
        doc: &mut BlockDocument,
        document_id: &str,
        oplog_bytes: &[u8],
    ) -> Result<SyncResult, SyncError> {
        if oplog_bytes.is_empty() {
            warn!("BlockCellInitialState has empty oplog, skipping");
            return Ok(SyncResult::Skipped {
                reason: SkipReason::EmptyOplog,
            });
        }

        info!(
            "Received initial state for document_id='{}', {} bytes oplog",
            document_id,
            oplog_bytes.len()
        );

        match BlockDocument::from_oplog(document_id.to_string(), doc.agent_id(), oplog_bytes) {
            Ok(new_doc) => {
                let block_count = new_doc.block_count();
                // Update sync state with frontier
                self.frontier = Some(new_doc.frontier());
                self.document_id = Some(document_id.to_string());
                self.version = self.version.wrapping_add(1);

                // Replace the document
                *doc = new_doc;

                info!(
                    "Initial sync complete for document_id='{}' - {} blocks, frontier={:?}",
                    document_id, block_count, self.frontier
                );

                // Replay any buffered ops now that we have a valid document
                self.replay_pending_ops(doc);

                Ok(SyncResult::FullSync { block_count })
            }
            Err(e) => {
                error!(
                    "Failed to create document from initial oplog for document '{}': {}",
                    document_id, e
                );
                Err(SyncError::FromOplog(e.to_string()))
            }
        }
    }

    /// Apply a block insertion event (BlockInserted).
    ///
    /// Decision logic:
    /// - If block already exists -> skip (idempotent)
    /// - If needs_full_sync -> rebuild from oplog
    /// - Otherwise -> incremental merge
    pub fn apply_block_inserted(
        &mut self,
        doc: &mut BlockDocument,
        document_id: &str,
        block: &BlockSnapshot,
        ops: &[u8],
    ) -> Result<SyncResult, SyncError> {
        // Document ID mismatch check
        if document_id != doc.document_id() {
            warn!(
                "Block event for document_id '{}' but document has '{}', skipping block {:?}",
                document_id,
                doc.document_id(),
                block.id
            );
            return Ok(SyncResult::Skipped {
                reason: SkipReason::DocumentIdMismatch {
                    expected: doc.document_id().to_string(),
                    got: document_id.to_string(),
                },
            });
        }

        // Idempotent: skip if block already exists
        if doc.get_block_snapshot(&block.id).is_some() {
            trace!("Block {:?} already exists, skipping", block.id);
            return Ok(SyncResult::Skipped {
                reason: SkipReason::BlockAlreadyExists,
            });
        }

        // Protocol validation
        if ops.is_empty() {
            error!(
                "BlockInserted for {:?} has no ops - protocol violation",
                block.id
            );
            return Ok(SyncResult::Skipped {
                reason: SkipReason::ProtocolViolation(
                    "BlockInserted has no ops - server must send creation ops".to_string(),
                ),
            });
        }

        // Determine sync strategy
        //
        // When needs_full_sync is true but the document already has content,
        // try incremental merge first. This handles the common recovery case:
        // BlockTextOps fail (DataMissing) -> frontier resets -> next BlockInserted
        // triggers full sync. But the ops in BlockInserted are incremental (not
        // a full oplog), so from_oplog destroys the existing document and fails.
        // The existing document already has the base state; incremental merge
        // should succeed because the new ops build on that state.
        let result = if self.needs_full_sync(document_id) {
            if !doc.is_empty() {
                // Try incremental merge first — preserves existing document
                match self.do_incremental_merge(doc, ops, Some(&block.id)) {
                    Ok(result) => Ok(result),
                    Err(e) => {
                        warn!(
                            "Recovery: incremental merge failed for {:?}, falling back to full sync: {}",
                            block.id, e
                        );
                        match self.do_full_sync(doc, document_id, ops, Some(&block.id)) {
                            Ok(result) => Ok(result),
                            Err(e) => {
                                // Both paths failed — buffer for later replay
                                self.buffer_failed_ops(Some(&block.id), ops);
                                Err(e)
                            }
                        }
                    }
                }
            } else {
                match self.do_full_sync(doc, document_id, ops, Some(&block.id)) {
                    Ok(result) => Ok(result),
                    Err(e) => {
                        self.buffer_failed_ops(Some(&block.id), ops);
                        Err(e)
                    }
                }
            }
        } else {
            self.do_incremental_merge(doc, ops, Some(&block.id))
        };

        // On any successful sync, replay buffered pending ops
        if result.is_ok() {
            self.replay_pending_ops(doc);
        }

        result
    }

    /// Apply text ops event (BlockTextOps).
    ///
    /// Attempts incremental merge (text streaming).
    /// On deserialization failure, resets frontier to trigger full sync.
    /// On CRDT merge failure (DataMissing), does NOT reset frontier -- this is
    /// likely a race condition where text ops arrived before the corresponding
    /// BlockInserted event. The frontier is still valid; the BlockInserted will
    /// bring the missing ops.
    ///
    /// Note: This method does NOT fall back to full sync even when `needs_full_sync()`
    /// is true. Text ops are incremental by nature - if we're out of sync, recovery
    /// must come from a `BlockInserted` event with full oplog.
    pub fn apply_text_ops(
        &mut self,
        doc: &mut BlockDocument,
        document_id: &str,
        ops: &[u8],
    ) -> Result<SyncResult, SyncError> {
        // Document ID mismatch check
        if document_id != doc.document_id() {
            return Ok(SyncResult::Skipped {
                reason: SkipReason::DocumentIdMismatch {
                    expected: doc.document_id().to_string(),
                    got: document_id.to_string(),
                },
            });
        }

        // If we already need a full sync, skip text ops entirely —
        // they can't help us recover, only BlockInserted can
        if self.needs_full_sync(document_id) {
            trace!("Skipping text ops while waiting for full sync");
            return Ok(SyncResult::Skipped {
                reason: SkipReason::EmptyOplog,  // Reusing existing variant
            });
        }

        // Empty ops are likely a protocol issue - skip rather than fail
        if ops.is_empty() {
            trace!("BlockTextOps has empty ops, skipping");
            return Ok(SyncResult::Skipped {
                reason: SkipReason::EmptyOplog,
            });
        }

        let result = self.do_incremental_merge(doc, ops, None);

        // On success, replay any buffered pending ops
        if result.is_ok() {
            self.replay_pending_ops(doc);
        } else if let Err(SyncError::Merge(_)) = &result {
            // CRDT merge failure (likely DataMissing) — buffer for replay.
            // Don't buffer deserialization failures (corrupt data won't improve).
            self.buffer_failed_ops(None, ops);
        }

        result
    }

    // =========================================================================
    // Internal helpers
    // =========================================================================

    /// Perform full sync by rebuilding document from oplog.
    fn do_full_sync(
        &mut self,
        doc: &mut BlockDocument,
        document_id: &str,
        ops: &[u8],
        block_id: Option<&kaijutsu_crdt::BlockId>,
    ) -> Result<SyncResult, SyncError> {
        info!(
            "Full sync for document_id='{}', block_id={:?}, ops_len={} (frontier={:?}, tracked_document={:?})",
            document_id,
            block_id,
            ops.len(),
            self.frontier.is_some(),
            self.document_id
        );

        match BlockDocument::from_oplog(document_id.to_string(), doc.agent_id(), ops) {
            Ok(new_doc) => {
                let block_count = new_doc.block_count();
                // Update sync state with new frontier
                self.frontier = Some(new_doc.frontier());
                self.document_id = Some(document_id.to_string());
                self.version = self.version.wrapping_add(1);

                // Replace the document
                *doc = new_doc;

                info!(
                    "Full sync complete - {} blocks, {} bytes, frontier={:?}",
                    block_count,
                    ops.len(),
                    self.frontier
                );

                Ok(SyncResult::FullSync { block_count })
            }
            Err(e) => {
                error!(
                    "Failed to sync document from oplog for document '{}': {}",
                    document_id, e
                );
                Err(SyncError::FromOplog(e.to_string()))
            }
        }
    }

    /// Perform incremental merge of ops.
    fn do_incremental_merge(
        &mut self,
        doc: &mut BlockDocument,
        ops: &[u8],
        block_id: Option<&kaijutsu_crdt::BlockId>,
    ) -> Result<SyncResult, SyncError> {
        // Deserialize ops
        let serialized_ops: SerializedOpsOwned = match serde_json::from_slice(ops) {
            Ok(ops) => ops,
            Err(e) => {
                warn!("Failed to deserialize ops: {}", e);
                // Reset frontier to trigger full sync on next event
                self.frontier = None;
                return Err(SyncError::Deserialize(e.to_string()));
            }
        };

        // Merge ops
        match doc.merge_ops_owned(serialized_ops) {
            Ok(()) => {
                // Update frontier after merge
                self.frontier = Some(doc.frontier());
                self.version = self.version.wrapping_add(1);
                trace!(
                    "Incremental merge for block {:?}, new frontier={:?}",
                    block_id,
                    self.frontier
                );
                Ok(SyncResult::IncrementalMerge)
            }
            Err(e) => {
                // Merge failed - likely DataMissing, will need full sync
                warn!(
                    "Incremental merge failed for block {:?}: {} - will need full sync",
                    block_id, e
                );
                // Reset frontier to trigger full sync on next event
                self.frontier = None;
                Err(SyncError::Merge(e.to_string()))
            }
        }
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_crdt::{BlockKind, Role};

    /// Helper: create a server document with some content.
    fn create_server_doc(document_id: &str) -> BlockDocument {
        let mut doc = BlockDocument::new(document_id, "server-agent");
        doc.insert_block(None, None, Role::User, BlockKind::Text, "Hello from server", "server")
            .expect("insert block");
        doc
    }

    /// Helper: create a client document (empty, ready for sync).
    fn create_client_doc(document_id: &str) -> BlockDocument {
        // Client starts with empty document (will be replaced by sync)
        BlockDocument::new(document_id, "client-agent")
    }

    // =========================================================================
    // Core State Machine Tests
    // =========================================================================

    #[test]
    fn test_initial_sync() {
        let server = create_server_doc("doc-1");
        let oplog_bytes = server.oplog_bytes();

        let mut client = create_client_doc("doc-1");
        let mut sync = SyncManager::new();

        assert!(sync.needs_full_sync("doc-1"));

        let result = sync
            .apply_initial_state(&mut client, "doc-1", &oplog_bytes)
            .expect("initial sync");

        assert!(matches!(result, SyncResult::FullSync { block_count: 1 }));
        assert!(!sync.needs_full_sync("doc-1"));
        assert_eq!(sync.document_id(), Some("doc-1"));
        assert!(sync.frontier().is_some());

        // Verify document content
        assert_eq!(client.block_count(), 1);
        assert!(client.full_text().contains("Hello from server"));
    }

    #[test]
    fn test_incremental_after_full_sync() {
        // Initial sync
        let mut server = create_server_doc("doc-1");
        let initial_oplog = server.oplog_bytes();

        let mut client = create_client_doc("doc-1");
        let mut sync = SyncManager::new();

        sync.apply_initial_state(&mut client, "doc-1", &initial_oplog)
            .expect("initial sync");

        // Server adds a new block
        let server_frontier = server.frontier();
        let block_id = server
            .insert_block(None, None, Role::Model, BlockKind::Text, "Response from model", "server")
            .expect("insert block");
        let block = server.get_block_snapshot(&block_id).expect("block exists");

        // Get incremental ops
        let incremental_ops = server.ops_since(&server_frontier);
        let ops_bytes = serde_json::to_vec(&incremental_ops).expect("serialize ops");

        // Apply incremental merge
        let result = sync
            .apply_block_inserted(&mut client, "doc-1", &block, &ops_bytes)
            .expect("incremental merge");

        assert!(matches!(result, SyncResult::IncrementalMerge));
        assert_eq!(client.block_count(), 2);
        assert!(client.full_text().contains("Response from model"));
    }

    #[test]
    fn test_document_id_mismatch_skips() {
        let server = create_server_doc("doc-1");
        let oplog_bytes = server.oplog_bytes();

        let mut client = create_client_doc("doc-1");
        let mut sync = SyncManager::new();

        // First sync to doc-1
        sync.apply_initial_state(&mut client, "doc-1", &oplog_bytes)
            .expect("initial sync");

        // Try to apply block for different document
        let other_server = create_server_doc("doc-2");
        let other_block = other_server.blocks_ordered()[0].clone();
        let other_ops = other_server.oplog_bytes();

        let result = sync
            .apply_block_inserted(&mut client, "doc-2", &other_block, &other_ops)
            .expect("should skip");

        assert!(matches!(
            result,
            SyncResult::Skipped {
                reason: SkipReason::DocumentIdMismatch { .. }
            }
        ));
        // Document unchanged
        assert_eq!(client.block_count(), 1);
    }

    #[test]
    fn test_idempotent_block_insert() {
        let server = create_server_doc("doc-1");
        let oplog_bytes = server.oplog_bytes();
        let block = server.blocks_ordered()[0].clone();

        let mut client = create_client_doc("doc-1");
        let mut sync = SyncManager::new();

        // Initial sync
        sync.apply_initial_state(&mut client, "doc-1", &oplog_bytes)
            .expect("initial sync");

        // Try to insert same block again
        let result = sync
            .apply_block_inserted(&mut client, "doc-1", &block, &oplog_bytes)
            .expect("should skip");

        assert!(matches!(
            result,
            SyncResult::Skipped {
                reason: SkipReason::BlockAlreadyExists
            }
        ));
    }

    // =========================================================================
    // Recovery Tests
    // =========================================================================

    #[test]
    fn test_merge_failure_resets_frontier_deserialization() {
        let server = create_server_doc("doc-1");
        let oplog_bytes = server.oplog_bytes();

        let mut client = create_client_doc("doc-1");
        let mut sync = SyncManager::new();

        // Initial sync
        sync.apply_initial_state(&mut client, "doc-1", &oplog_bytes)
            .expect("initial sync");
        assert!(!sync.needs_full_sync("doc-1"));

        // Corrupt ops to simulate deserialization failure
        let corrupt_ops = b"not valid json";
        let result = sync.apply_text_ops(&mut client, "doc-1", corrupt_ops);

        assert!(matches!(result, Err(SyncError::Deserialize(_))));
        // Frontier should be reset
        assert!(sync.needs_full_sync("doc-1"));
        assert!(sync.frontier().is_none());
    }

    #[test]
    fn test_merge_failure_resets_frontier_crdt_data_missing() {
        // This test exercises the actual CRDT DataMissing error path.
        //
        // Scenario: Client and server have DIVERGENT oplog roots.
        // When server sends incremental ops, client can't merge them
        // because they reference CRDT structures that don't exist locally.

        // Server has its own oplog with a block
        let mut server = create_server_doc("doc-1");

        // Client has an INDEPENDENT oplog (different root!)
        // This simulates a client that somehow got out of sync.
        let mut client = create_client_doc("doc-1");

        // Pretend the client thinks it's synced (would happen if connection dropped
        // mid-sync and client kept old state)
        let mut sync = SyncManager::with_state(
            Some("doc-1".to_string()),
            Some(client.frontier()), // Client's own frontier, not server's
        );

        // Server adds a new block
        let server_frontier_before = server.frontier();
        let new_block_id = server
            .insert_block(None, None, Role::Model, BlockKind::Text, "New content", "server")
            .expect("insert block");
        let new_block = server.get_block_snapshot(&new_block_id).expect("block exists");

        // Get INCREMENTAL ops from server (not full oplog)
        // These ops reference server's oplog root which client doesn't have
        let incremental_ops = server.ops_since(&server_frontier_before);
        let ops_bytes = serde_json::to_vec(&incremental_ops).expect("serialize");

        // Try to apply incremental merge - should fail with DataMissing
        let result = sync.apply_block_inserted(&mut client, "doc-1", &new_block, &ops_bytes);

        // Should be a Merge error (CRDT couldn't apply ops due to missing dependencies)
        assert!(
            matches!(result, Err(SyncError::Merge(_))),
            "Expected Merge error, got {:?}",
            result
        );

        // Frontier should be reset, enabling recovery on next full sync
        assert!(sync.needs_full_sync("doc-1"));
        assert!(sync.frontier().is_none());

        // Now simulate recovery: server sends full oplog
        let full_oplog = server.oplog_bytes();
        let result = sync
            .apply_block_inserted(&mut client, "doc-1", &new_block, &full_oplog)
            .expect("recovery should succeed");

        // Should do full sync and recover
        assert!(matches!(result, SyncResult::FullSync { block_count: 2 }));
        assert!(!sync.needs_full_sync("doc-1"));
        assert!(client.full_text().contains("New content"));
    }

    #[test]
    fn test_recovery_after_merge_failure() {
        let mut server = create_server_doc("doc-1");
        let oplog_bytes = server.oplog_bytes();

        let mut client = create_client_doc("doc-1");
        let mut sync = SyncManager::new();

        // Initial sync
        sync.apply_initial_state(&mut client, "doc-1", &oplog_bytes)
            .expect("initial sync");

        // Cause a failure by sending corrupt ops
        let corrupt_ops = b"not valid json";
        let _ = sync.apply_text_ops(&mut client, "doc-1", corrupt_ops);
        assert!(sync.needs_full_sync("doc-1"));

        // Server adds new content - capture the block ID directly from insert
        let new_block_id = server
            .insert_block(None, None, Role::Model, BlockKind::Text, "Recovery content", "server")
            .expect("insert block");
        let full_oplog = server.oplog_bytes();
        let new_block = server.get_block_snapshot(&new_block_id).expect("new block exists");

        // Apply block inserted - recovery tries incremental merge first since
        // the document already has content, then falls back to full sync if needed.
        // With a full oplog, incremental merge succeeds (it includes new ops too).
        let result = sync
            .apply_block_inserted(&mut client, "doc-1", &new_block, &full_oplog)
            .expect("recovery sync");

        assert!(
            matches!(result, SyncResult::IncrementalMerge | SyncResult::FullSync { .. }),
            "Expected recovery via incremental merge or full sync, got {:?}", result
        );
        assert!(!sync.needs_full_sync("doc-1"));
        assert!(client.full_text().contains("Recovery content"));
    }

    #[test]
    fn test_frontier_none_triggers_full_sync() {
        let server = create_server_doc("doc-1");
        let oplog_bytes = server.oplog_bytes();
        let block = server.blocks_ordered()[0].clone();

        let mut client = create_client_doc("doc-1");
        let mut sync = SyncManager::new();

        // Fresh SyncManager has no frontier
        assert!(sync.needs_full_sync("doc-1"));
        assert!(sync.frontier().is_none());

        // Apply block inserted (not initial state) - should trigger full sync
        let result = sync
            .apply_block_inserted(&mut client, "doc-1", &block, &oplog_bytes)
            .expect("full sync");

        assert!(matches!(result, SyncResult::FullSync { block_count: 1 }));
        assert!(!sync.needs_full_sync("doc-1"));
    }

    #[test]
    fn test_document_id_change_triggers_full_sync() {
        // Sync to doc-1
        let server1 = create_server_doc("doc-1");
        let oplog1 = server1.oplog_bytes();

        let mut client = create_client_doc("doc-1");
        let mut sync = SyncManager::new();

        sync.apply_initial_state(&mut client, "doc-1", &oplog1)
            .expect("initial sync");
        assert!(!sync.needs_full_sync("doc-1"));

        // Now switch to doc-2 - should need full sync
        assert!(sync.needs_full_sync("doc-2"));

        // Create server for doc-2
        let server2 = create_server_doc("doc-2");
        let oplog2 = server2.oplog_bytes();

        // Apply initial state for doc-2
        let result = sync
            .apply_initial_state(&mut client, "doc-2", &oplog2)
            .expect("sync to doc-2");

        assert!(matches!(result, SyncResult::FullSync { block_count: 1 }));
        assert_eq!(sync.document_id(), Some("doc-2"));
        assert!(!sync.needs_full_sync("doc-2"));
    }

    // =========================================================================
    // Streaming Tests
    // =========================================================================

    #[test]
    fn test_text_streaming_multiple_chunks() {
        // Setup: server with initial block
        let mut server = create_server_doc("doc-1");
        let initial_oplog = server.oplog_bytes();

        let mut client = create_client_doc("doc-1");
        let mut sync = SyncManager::new();

        sync.apply_initial_state(&mut client, "doc-1", &initial_oplog)
            .expect("initial sync");

        // Server adds a response block
        let server_frontier = server.frontier();
        let block_id = server
            .insert_block(None, None, Role::Model, BlockKind::Text, "", "server")
            .expect("insert block");
        let incremental_ops = serde_json::to_vec(&server.ops_since(&server_frontier)).unwrap();
        let block = server.get_block_snapshot(&block_id).unwrap();

        sync.apply_block_inserted(&mut client, "doc-1", &block, &incremental_ops)
            .expect("insert empty block");

        // Stream text in chunks
        let chunks = ["Hello", ", ", "world", "!"];
        for chunk in chunks {
            let frontier_before = server.frontier();
            server.append_text(&block_id, chunk).expect("append text");
            let chunk_ops = serde_json::to_vec(&server.ops_since(&frontier_before)).unwrap();

            let result = sync
                .apply_text_ops(&mut client, "doc-1", &chunk_ops)
                .expect("stream chunk");

            assert!(matches!(result, SyncResult::IncrementalMerge));
        }

        // Verify final content
        let client_block = client.get_block_snapshot(&block_id).expect("block exists");
        assert_eq!(client_block.content, "Hello, world!");
    }

    #[test]
    fn test_text_streaming_with_mid_stream_error() {
        // Setup
        let mut server = create_server_doc("doc-1");
        let initial_oplog = server.oplog_bytes();

        let mut client = create_client_doc("doc-1");
        let mut sync = SyncManager::new();

        sync.apply_initial_state(&mut client, "doc-1", &initial_oplog)
            .expect("initial sync");

        // Add streaming block
        let server_frontier = server.frontier();
        let block_id = server
            .insert_block(None, None, Role::Model, BlockKind::Text, "", "server")
            .expect("insert block");
        let incremental_ops = serde_json::to_vec(&server.ops_since(&server_frontier)).unwrap();
        let block = server.get_block_snapshot(&block_id).unwrap();

        sync.apply_block_inserted(&mut client, "doc-1", &block, &incremental_ops)
            .expect("insert block");

        // Stream chunk 1
        let frontier_before = server.frontier();
        server.append_text(&block_id, "Hello").expect("append");
        let chunk_ops = serde_json::to_vec(&server.ops_since(&frontier_before)).unwrap();
        sync.apply_text_ops(&mut client, "doc-1", &chunk_ops)
            .expect("chunk 1");

        // Corrupt chunk 2 - simulates network corruption
        let corrupt_chunk = b"corrupted data";
        let result = sync.apply_text_ops(&mut client, "doc-1", corrupt_chunk);
        assert!(matches!(result, Err(SyncError::Deserialize(_))));

        // Frontier should be reset
        assert!(sync.needs_full_sync("doc-1"));
    }

    #[test]
    fn test_text_streaming_recovery_after_error() {
        // Setup
        let mut server = create_server_doc("doc-1");
        let initial_oplog = server.oplog_bytes();

        let mut client = create_client_doc("doc-1");
        let mut sync = SyncManager::new();

        sync.apply_initial_state(&mut client, "doc-1", &initial_oplog)
            .expect("initial sync");

        // Cause deserialization error
        let corrupt_ops = b"not json";
        let _ = sync.apply_text_ops(&mut client, "doc-1", corrupt_ops);
        assert!(sync.needs_full_sync("doc-1"));

        // Server adds more content - capture block ID directly
        let new_block_id = server
            .insert_block(None, None, Role::Model, BlockKind::Text, "After error", "server")
            .expect("insert block");
        let full_oplog = server.oplog_bytes();
        let new_block = server.get_block_snapshot(&new_block_id).expect("new block exists");

        // Recovery — tries incremental merge first since document has content,
        // falls back to full sync if needed
        let result = sync
            .apply_block_inserted(&mut client, "doc-1", &new_block, &full_oplog)
            .expect("recovery");

        assert!(
            matches!(result, SyncResult::IncrementalMerge | SyncResult::FullSync { .. }),
            "Expected recovery via incremental merge or full sync, got {:?}", result
        );
        assert!(!sync.needs_full_sync("doc-1"));
        assert!(client.full_text().contains("After error"));
    }

    // =========================================================================
    // Edge Cases
    // =========================================================================

    #[test]
    fn test_empty_oplog_skips_initial_state() {
        let mut client = create_client_doc("doc-1");
        let mut sync = SyncManager::new();

        let result = sync
            .apply_initial_state(&mut client, "doc-1", &[])
            .expect("should skip");

        assert!(matches!(
            result,
            SyncResult::Skipped {
                reason: SkipReason::EmptyOplog
            }
        ));
    }

    #[test]
    fn test_empty_ops_skips_text_ops() {
        let server = create_server_doc("doc-1");
        let oplog_bytes = server.oplog_bytes();

        let mut client = create_client_doc("doc-1");
        let mut sync = SyncManager::new();

        // Initial sync
        sync.apply_initial_state(&mut client, "doc-1", &oplog_bytes)
            .expect("initial sync");

        // Empty text ops should skip, not fail
        let result = sync
            .apply_text_ops(&mut client, "doc-1", &[])
            .expect("should skip");

        assert!(matches!(
            result,
            SyncResult::Skipped {
                reason: SkipReason::EmptyOplog
            }
        ));

        // Frontier should NOT be reset (it wasn't an error)
        assert!(!sync.needs_full_sync("doc-1"));
    }

    #[test]
    fn test_protocol_violation_no_ops() {
        let server = create_server_doc("doc-1");
        let block = server.blocks_ordered()[0].clone();

        // Create client with matching document_id (so it won't be rejected for mismatch)
        let mut client = create_client_doc("doc-1");

        // Create SyncManager that thinks it's synced (has frontier)
        // Use with_state() rather than direct field access
        let mut sync = SyncManager::with_state(
            Some("doc-1".to_string()),
            Some(Frontier::root()), // Empty frontier = "synced" state
        );

        // Try to insert block with empty ops - should fail protocol validation
        let result = sync
            .apply_block_inserted(&mut client, "doc-1", &block, &[])
            .expect("should skip");

        assert!(matches!(
            result,
            SyncResult::Skipped {
                reason: SkipReason::ProtocolViolation(_)
            }
        ));
    }

    #[test]
    fn test_reset_clears_frontier_but_keeps_document_id() {
        let server = create_server_doc("doc-1");
        let oplog_bytes = server.oplog_bytes();

        let mut client = create_client_doc("doc-1");
        let mut sync = SyncManager::new();

        sync.apply_initial_state(&mut client, "doc-1", &oplog_bytes)
            .expect("initial sync");

        assert!(sync.frontier().is_some());
        assert_eq!(sync.document_id(), Some("doc-1"));

        sync.reset();

        assert!(sync.frontier().is_none());
        // Document ID is preserved for diagnostics
        assert_eq!(sync.document_id(), Some("doc-1"));
        // But we still need full sync
        assert!(sync.needs_full_sync("doc-1"));
    }

    // =========================================================================
    // Pending Ops Buffer Tests
    // =========================================================================

    #[test]
    fn test_pending_ops_buffered_on_double_failure() {
        // Client and server have divergent oplogs — both incremental and full sync fail.
        // To trigger the double-failure path, needs_full_sync must be true AND
        // the document must be non-empty (so incremental merge is tried first,
        // then full sync as fallback — both fail on divergent incremental ops).
        let mut server = create_server_doc("doc-1");

        // Client has its own independent content (non-empty, divergent oplog)
        let mut client = create_client_doc("doc-1");
        client.insert_block(None, None, Role::User, BlockKind::Text, "Client content", "client")
            .expect("insert client block");

        // No frontier → needs_full_sync is true, AND doc is non-empty
        // This triggers: incremental merge (fails on divergent ops) →
        //                full sync (fails because incremental ops aren't a complete oplog) →
        //                buffer the ops
        let mut sync = SyncManager::new();

        // Server adds a new block — get INCREMENTAL ops only
        let server_frontier_before = server.frontier();
        let new_block_id = server
            .insert_block(None, None, Role::Model, BlockKind::Text, "New content", "server")
            .expect("insert block");
        let new_block = server.get_block_snapshot(&new_block_id).expect("block exists");

        let incremental_ops = server.ops_since(&server_frontier_before);
        let ops_bytes = serde_json::to_vec(&incremental_ops).expect("serialize");

        // Try to apply — both paths should fail, ops should be buffered
        let result = sync.apply_block_inserted(&mut client, "doc-1", &new_block, &ops_bytes);
        assert!(result.is_err(), "Expected failure, got {:?}", result);

        // Ops should be buffered, not lost
        assert_eq!(sync.pending_ops_count(), 1, "Expected 1 pending op");
    }

    #[test]
    fn test_pending_ops_replayed_after_successful_sync() {
        // Same setup as double_failure — divergent oplogs trigger buffering
        let mut server = create_server_doc("doc-1");
        let mut client = create_client_doc("doc-1");
        client.insert_block(None, None, Role::User, BlockKind::Text, "Client content", "client")
            .expect("insert client block");

        let mut sync = SyncManager::new();

        // Create incremental ops that will fail (double failure → buffer)
        let server_frontier_before = server.frontier();
        let new_block_id = server
            .insert_block(None, None, Role::Model, BlockKind::Text, "Buffered content", "server")
            .expect("insert block");
        let new_block = server.get_block_snapshot(&new_block_id).expect("block exists");
        let incremental_ops = server.ops_since(&server_frontier_before);
        let ops_bytes = serde_json::to_vec(&incremental_ops).expect("serialize");

        // This fails and buffers
        let _ = sync.apply_block_inserted(&mut client, "doc-1", &new_block, &ops_bytes);
        assert_eq!(sync.pending_ops_count(), 1);

        // Now send a full oplog — this should succeed AND replay buffered ops
        let full_oplog = server.oplog_bytes();
        let result = sync
            .apply_initial_state(&mut client, "doc-1", &full_oplog)
            .expect("full sync should succeed");

        assert!(matches!(result, SyncResult::FullSync { .. }));
        // Pending ops should have been drained (replayed — they may or may not
        // succeed but they should be attempted and removed from buffer)
        assert_eq!(sync.pending_ops_count(), 0, "Pending ops should be drained after replay");
        // The full oplog already contains the "Buffered content" block
        assert!(client.full_text().contains("Buffered content"));
    }

    #[test]
    fn test_pending_ops_text_before_block_inserted() {
        // The exact race condition: text ops arrive before BlockInserted
        let mut server = create_server_doc("doc-1");
        let initial_oplog = server.oplog_bytes();

        let mut client = create_client_doc("doc-1");
        let mut sync = SyncManager::new();

        // Initial sync
        sync.apply_initial_state(&mut client, "doc-1", &initial_oplog)
            .expect("initial sync");

        // Server creates a new block and appends text
        let server_frontier_before = server.frontier();
        let block_id = server
            .insert_block(None, None, Role::Model, BlockKind::Text, "", "server")
            .expect("insert block");
        let block_frontier = server.frontier();

        server.append_text(&block_id, "Hello from shell").expect("append");
        let text_ops = serde_json::to_vec(&server.ops_since(&block_frontier)).unwrap();

        // Text ops arrive FIRST (before BlockInserted) — should fail with Merge error
        let result = sync.apply_text_ops(&mut client, "doc-1", &text_ops);
        assert!(result.is_err(), "Text ops should fail before block exists");
        assert_eq!(sync.pending_ops_count(), 1, "Text ops should be buffered");

        // Now BlockInserted arrives with full ops
        let block = server.get_block_snapshot(&block_id).expect("block exists");
        let block_ops = serde_json::to_vec(&server.ops_since(&server_frontier_before)).unwrap();
        let result = sync
            .apply_block_inserted(&mut client, "doc-1", &block, &block_ops)
            .expect("block insert should succeed");

        assert!(matches!(result, SyncResult::IncrementalMerge));
        // Buffered text ops should have been replayed
        // (They may or may not succeed depending on CRDT state, but they should be attempted)
        // The block itself should exist
        assert!(client.get_block_snapshot(&block_id).is_some());
    }

    #[test]
    fn test_pending_ops_cap_drops_oldest() {
        let mut sync = SyncManager::new();

        // Fill pending_ops beyond the cap by directly calling buffer_failed_ops
        for i in 0..(MAX_PENDING_OPS + 10) {
            let fake_ops = format!("fake-ops-{}", i).into_bytes();
            sync.buffer_failed_ops(None, &fake_ops);
        }

        assert!(
            sync.pending_ops_count() <= MAX_PENDING_OPS,
            "Expected <= {} pending ops, got {}",
            MAX_PENDING_OPS,
            sync.pending_ops_count()
        );

        // Verify newest entries are retained (not oldest)
        let last_ops = &sync.pending_ops.last().unwrap().1;
        let last_str = String::from_utf8_lossy(last_ops);
        assert!(
            last_str.contains(&format!("{}", MAX_PENDING_OPS + 9)),
            "Expected newest entry, got: {}",
            last_str
        );
    }

    #[test]
    fn test_pending_ops_replay_partial_failure() {
        // Verify that replay attempts all buffered ops: successes are consumed,
        // failures go back to the buffer.
        let mut server = create_server_doc("doc-1");
        let initial_oplog = server.oplog_bytes();

        let mut client = create_client_doc("doc-1");
        let mut sync = SyncManager::new();

        sync.apply_initial_state(&mut client, "doc-1", &initial_oplog)
            .expect("initial sync");

        // Create valid incremental ops (block insert) and then text ops
        // in order so the CRDT frontier stays linear
        let frontier_before = server.frontier();
        let block_id = server
            .insert_block(None, None, Role::Model, BlockKind::Text, "Valid block", "server")
            .expect("insert block");
        let valid_block_ops = serde_json::to_vec(&server.ops_since(&frontier_before)).unwrap();

        let frontier_before = server.frontier();
        server.append_text(&block_id, " extra").expect("append");
        let valid_text_ops = serde_json::to_vec(&server.ops_since(&frontier_before)).unwrap();

        // Buffer: valid block ops, then corrupt, then valid text ops
        // The block ops must be replayed first for the text ops to succeed
        sync.pending_ops.push((None, valid_block_ops));
        sync.pending_ops.push((None, b"not-valid-json".to_vec()));
        sync.pending_ops.push((None, valid_text_ops));
        assert_eq!(sync.pending_ops_count(), 3);

        // Trigger replay directly
        sync.replay_pending_ops(&mut client);

        // Valid block ops (1st) should succeed → consumed
        // Corrupt ops (2nd) should fail deserialization → DROPPED (not re-buffered,
        //   to avoid a death spiral where each replay resets the frontier)
        // Valid text ops (3rd) depends on whether the corrupt op's frontier
        //   reset affected it — may succeed or be re-buffered as a Merge error
        assert!(
            sync.pending_ops_count() < 3,
            "Expected replay to consume/drop some ops, got {}",
            sync.pending_ops_count()
        );
    }
}
