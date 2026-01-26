//! Testable CRDT sync logic extracted from Bevy systems.
//!
//! This module implements frontier-based CRDT sync independent of Bevy ECS,
//! enabling comprehensive unit testing without mock frameworks.
//!
//! # Sync Protocol
//!
//! - `frontier = None` or `cell_id` changed → full sync (from_oplog)
//! - `frontier = Some(_)` and matching cell_id → incremental merge (merge_ops_owned)
//! - On merge failure → reset frontier, next event triggers full sync

use kaijutsu_crdt::{BlockDocument, BlockSnapshot, SerializedOpsOwned, LV};
use thiserror::Error;
use tracing::{error, info, trace, warn};

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
    /// Cell ID doesn't match our tracked document.
    CellIdMismatch { expected: String, got: String },
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
/// allowing the Bevy system to remain thin while the core logic is unit-testable.
///
/// # State Machine
///
/// ```text
/// ┌────────────────┐
/// │ Initial State  │ frontier=None, cell_id=None
/// │ (needs sync)   │
/// └───────┬────────┘
///         │ apply_initial_state() or apply_block_inserted() with full oplog
///         ▼
/// ┌────────────────┐
/// │  Synchronized  │ frontier=Some(vec), cell_id=Some(id)
/// │ (incremental)  │
/// └───────┬────────┘
///         │ merge failure OR cell_id change
///         ▼
/// ┌────────────────┐
/// │  Needs Resync  │ frontier=None (triggers full sync on next event)
/// └────────────────┘
/// ```
#[derive(Debug, Clone, Default)]
pub struct SyncManager {
    /// Current frontier (None = never synced or needs full sync).
    frontier: Option<Vec<LV>>,
    /// Cell ID we're synced to. Change triggers full sync.
    cell_id: Option<String>,
}

impl SyncManager {
    /// Create a new SyncManager in "needs full sync" state.
    pub fn new() -> Self {
        Self {
            frontier: None,
            cell_id: None,
        }
    }

    /// Create a SyncManager with existing state (for testing/migration).
    pub fn with_state(cell_id: Option<String>, frontier: Option<Vec<LV>>) -> Self {
        Self { frontier, cell_id }
    }

    /// Check if we need a full sync for the given cell.
    ///
    /// Returns true if:
    /// - We have no frontier (never synced or reset after failure)
    /// - The cell_id doesn't match our tracked document
    pub fn needs_full_sync(&self, cell_id: &str) -> bool {
        self.frontier.is_none() || self.cell_id.as_deref() != Some(cell_id)
    }

    /// Get the current frontier (for testing/debugging).
    pub fn frontier(&self) -> Option<&[LV]> {
        self.frontier.as_deref()
    }

    /// Get the current cell_id (for testing/debugging).
    pub fn cell_id(&self) -> Option<&str> {
        self.cell_id.as_deref()
    }

    /// Reset sync state, forcing full sync on next event.
    ///
    /// Call this when merge failures occur or when you want to
    /// force a resync from the server's full oplog.
    pub fn reset(&mut self) {
        self.frontier = None;
        // Keep cell_id - if it changes we'll detect that too
    }

    /// Apply initial state from server (BlockCellInitialState event).
    ///
    /// Always performs a full sync from the provided oplog.
    pub fn apply_initial_state(
        &mut self,
        doc: &mut BlockDocument,
        cell_id: &str,
        oplog_bytes: &[u8],
    ) -> Result<SyncResult, SyncError> {
        if oplog_bytes.is_empty() {
            warn!("BlockCellInitialState has empty oplog, skipping");
            return Ok(SyncResult::Skipped {
                reason: SkipReason::EmptyOplog,
            });
        }

        info!(
            "Received initial state for cell_id='{}', {} bytes oplog",
            cell_id,
            oplog_bytes.len()
        );

        match BlockDocument::from_oplog(cell_id.to_string(), doc.agent_id(), oplog_bytes) {
            Ok(new_doc) => {
                let block_count = new_doc.block_count();
                // Update sync state with frontier
                self.frontier = Some(new_doc.frontier());
                self.cell_id = Some(cell_id.to_string());

                // Replace the document
                *doc = new_doc;

                info!(
                    "Initial sync complete for cell_id='{}' - {} blocks, frontier={:?}",
                    cell_id, block_count, self.frontier
                );

                Ok(SyncResult::FullSync { block_count })
            }
            Err(e) => {
                error!(
                    "Failed to create document from initial oplog for cell '{}': {}",
                    cell_id, e
                );
                Err(SyncError::FromOplog(e.to_string()))
            }
        }
    }

    /// Apply a block insertion event (BlockInserted).
    ///
    /// Decision logic:
    /// - If block already exists → skip (idempotent)
    /// - If needs_full_sync → rebuild from oplog
    /// - Otherwise → incremental merge
    pub fn apply_block_inserted(
        &mut self,
        doc: &mut BlockDocument,
        cell_id: &str,
        block: &BlockSnapshot,
        ops: &[u8],
    ) -> Result<SyncResult, SyncError> {
        // Cell ID mismatch check
        if cell_id != doc.cell_id() {
            warn!(
                "Block event for cell_id '{}' but document has '{}', skipping block {:?}",
                cell_id,
                doc.cell_id(),
                block.id
            );
            return Ok(SyncResult::Skipped {
                reason: SkipReason::CellIdMismatch {
                    expected: doc.cell_id().to_string(),
                    got: cell_id.to_string(),
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
        if self.needs_full_sync(cell_id) {
            self.do_full_sync(doc, cell_id, ops, Some(&block.id))
        } else {
            self.do_incremental_merge(doc, ops, Some(&block.id))
        }
    }

    /// Apply text ops event (BlockTextOps).
    ///
    /// Always attempts incremental merge (text streaming).
    /// On failure, resets frontier to trigger full sync on next block event.
    ///
    /// Note: This method does NOT fall back to full sync even when `needs_full_sync()`
    /// is true. Text ops are incremental by nature - if we're out of sync, recovery
    /// must come from a `BlockInserted` event with full oplog.
    pub fn apply_text_ops(
        &mut self,
        doc: &mut BlockDocument,
        cell_id: &str,
        ops: &[u8],
    ) -> Result<SyncResult, SyncError> {
        // Cell ID mismatch check
        if cell_id != doc.cell_id() {
            return Ok(SyncResult::Skipped {
                reason: SkipReason::CellIdMismatch {
                    expected: doc.cell_id().to_string(),
                    got: cell_id.to_string(),
                },
            });
        }

        // Empty ops are likely a protocol issue - skip rather than fail
        if ops.is_empty() {
            trace!("BlockTextOps has empty ops, skipping");
            return Ok(SyncResult::Skipped {
                reason: SkipReason::EmptyOplog,
            });
        }

        self.do_incremental_merge(doc, ops, None)
    }

    // =========================================================================
    // Internal helpers
    // =========================================================================

    /// Perform full sync by rebuilding document from oplog.
    fn do_full_sync(
        &mut self,
        doc: &mut BlockDocument,
        cell_id: &str,
        ops: &[u8],
        block_id: Option<&kaijutsu_crdt::BlockId>,
    ) -> Result<SyncResult, SyncError> {
        info!(
            "Full sync for cell_id='{}', block_id={:?}, ops_len={} (frontier={:?}, tracked_cell={:?})",
            cell_id,
            block_id,
            ops.len(),
            self.frontier.is_some(),
            self.cell_id
        );

        match BlockDocument::from_oplog(cell_id.to_string(), doc.agent_id(), ops) {
            Ok(new_doc) => {
                let block_count = new_doc.block_count();
                // Update sync state with new frontier
                self.frontier = Some(new_doc.frontier());
                self.cell_id = Some(cell_id.to_string());

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
                    "Failed to sync document from oplog for cell '{}': {}",
                    cell_id, e
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
    fn create_server_doc(cell_id: &str) -> BlockDocument {
        let mut doc = BlockDocument::new(cell_id, "server-agent");
        doc.insert_block(None, None, Role::User, BlockKind::Text, "Hello from server", "server")
            .expect("insert block");
        doc
    }

    /// Helper: create a client document (empty, ready for sync).
    fn create_client_doc(cell_id: &str) -> BlockDocument {
        // Client starts with empty document (will be replaced by sync)
        BlockDocument::new(cell_id, "client-agent")
    }

    // =========================================================================
    // Core State Machine Tests
    // =========================================================================

    #[test]
    fn test_initial_sync() {
        let server = create_server_doc("cell-1");
        let oplog_bytes = server.oplog_bytes();

        let mut client = create_client_doc("cell-1");
        let mut sync = SyncManager::new();

        assert!(sync.needs_full_sync("cell-1"));

        let result = sync
            .apply_initial_state(&mut client, "cell-1", &oplog_bytes)
            .expect("initial sync");

        assert!(matches!(result, SyncResult::FullSync { block_count: 1 }));
        assert!(!sync.needs_full_sync("cell-1"));
        assert_eq!(sync.cell_id(), Some("cell-1"));
        assert!(sync.frontier().is_some());

        // Verify document content
        assert_eq!(client.block_count(), 1);
        assert!(client.full_text().contains("Hello from server"));
    }

    #[test]
    fn test_incremental_after_full_sync() {
        // Initial sync
        let mut server = create_server_doc("cell-1");
        let initial_oplog = server.oplog_bytes();

        let mut client = create_client_doc("cell-1");
        let mut sync = SyncManager::new();

        sync.apply_initial_state(&mut client, "cell-1", &initial_oplog)
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
            .apply_block_inserted(&mut client, "cell-1", &block, &ops_bytes)
            .expect("incremental merge");

        assert!(matches!(result, SyncResult::IncrementalMerge));
        assert_eq!(client.block_count(), 2);
        assert!(client.full_text().contains("Response from model"));
    }

    #[test]
    fn test_cell_id_mismatch_skips() {
        let server = create_server_doc("cell-1");
        let oplog_bytes = server.oplog_bytes();

        let mut client = create_client_doc("cell-1");
        let mut sync = SyncManager::new();

        // First sync to cell-1
        sync.apply_initial_state(&mut client, "cell-1", &oplog_bytes)
            .expect("initial sync");

        // Try to apply block for different cell
        let other_server = create_server_doc("cell-2");
        let other_block = other_server.blocks_ordered()[0].clone();
        let other_ops = other_server.oplog_bytes();

        let result = sync
            .apply_block_inserted(&mut client, "cell-2", &other_block, &other_ops)
            .expect("should skip");

        assert!(matches!(
            result,
            SyncResult::Skipped {
                reason: SkipReason::CellIdMismatch { .. }
            }
        ));
        // Document unchanged
        assert_eq!(client.block_count(), 1);
    }

    #[test]
    fn test_idempotent_block_insert() {
        let server = create_server_doc("cell-1");
        let oplog_bytes = server.oplog_bytes();
        let block = server.blocks_ordered()[0].clone();

        let mut client = create_client_doc("cell-1");
        let mut sync = SyncManager::new();

        // Initial sync
        sync.apply_initial_state(&mut client, "cell-1", &oplog_bytes)
            .expect("initial sync");

        // Try to insert same block again
        let result = sync
            .apply_block_inserted(&mut client, "cell-1", &block, &oplog_bytes)
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
        let server = create_server_doc("cell-1");
        let oplog_bytes = server.oplog_bytes();

        let mut client = create_client_doc("cell-1");
        let mut sync = SyncManager::new();

        // Initial sync
        sync.apply_initial_state(&mut client, "cell-1", &oplog_bytes)
            .expect("initial sync");
        assert!(!sync.needs_full_sync("cell-1"));

        // Corrupt ops to simulate deserialization failure
        let corrupt_ops = b"not valid json";
        let result = sync.apply_text_ops(&mut client, "cell-1", corrupt_ops);

        assert!(matches!(result, Err(SyncError::Deserialize(_))));
        // Frontier should be reset
        assert!(sync.needs_full_sync("cell-1"));
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
        let mut server = create_server_doc("cell-1");

        // Client has an INDEPENDENT oplog (different root!)
        // This simulates a client that somehow got out of sync.
        let mut client = create_client_doc("cell-1");

        // Pretend the client thinks it's synced (would happen if connection dropped
        // mid-sync and client kept old state)
        let mut sync = SyncManager::with_state(
            Some("cell-1".to_string()),
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
        let result = sync.apply_block_inserted(&mut client, "cell-1", &new_block, &ops_bytes);

        // Should be a Merge error (CRDT couldn't apply ops due to missing dependencies)
        assert!(
            matches!(result, Err(SyncError::Merge(_))),
            "Expected Merge error, got {:?}",
            result
        );

        // Frontier should be reset, enabling recovery on next full sync
        assert!(sync.needs_full_sync("cell-1"));
        assert!(sync.frontier().is_none());

        // Now simulate recovery: server sends full oplog
        let full_oplog = server.oplog_bytes();
        let result = sync
            .apply_block_inserted(&mut client, "cell-1", &new_block, &full_oplog)
            .expect("recovery should succeed");

        // Should do full sync and recover
        assert!(matches!(result, SyncResult::FullSync { block_count: 2 }));
        assert!(!sync.needs_full_sync("cell-1"));
        assert!(client.full_text().contains("New content"));
    }

    #[test]
    fn test_recovery_after_merge_failure() {
        let mut server = create_server_doc("cell-1");
        let oplog_bytes = server.oplog_bytes();

        let mut client = create_client_doc("cell-1");
        let mut sync = SyncManager::new();

        // Initial sync
        sync.apply_initial_state(&mut client, "cell-1", &oplog_bytes)
            .expect("initial sync");

        // Cause a failure by sending corrupt ops
        let corrupt_ops = b"not valid json";
        let _ = sync.apply_text_ops(&mut client, "cell-1", corrupt_ops);
        assert!(sync.needs_full_sync("cell-1"));

        // Server adds new content - capture the block ID directly from insert
        let new_block_id = server
            .insert_block(None, None, Role::Model, BlockKind::Text, "Recovery content", "server")
            .expect("insert block");
        let full_oplog = server.oplog_bytes();
        let new_block = server.get_block_snapshot(&new_block_id).expect("new block exists");

        // Apply block inserted - should trigger full sync since frontier was reset
        let result = sync
            .apply_block_inserted(&mut client, "cell-1", &new_block, &full_oplog)
            .expect("recovery sync");

        assert!(matches!(result, SyncResult::FullSync { block_count: 2 }));
        assert!(!sync.needs_full_sync("cell-1"));
        assert!(client.full_text().contains("Recovery content"));
    }

    #[test]
    fn test_frontier_none_triggers_full_sync() {
        let server = create_server_doc("cell-1");
        let oplog_bytes = server.oplog_bytes();
        let block = server.blocks_ordered()[0].clone();

        let mut client = create_client_doc("cell-1");
        let mut sync = SyncManager::new();

        // Fresh SyncManager has no frontier
        assert!(sync.needs_full_sync("cell-1"));
        assert!(sync.frontier().is_none());

        // Apply block inserted (not initial state) - should trigger full sync
        let result = sync
            .apply_block_inserted(&mut client, "cell-1", &block, &oplog_bytes)
            .expect("full sync");

        assert!(matches!(result, SyncResult::FullSync { block_count: 1 }));
        assert!(!sync.needs_full_sync("cell-1"));
    }

    #[test]
    fn test_cell_id_change_triggers_full_sync() {
        // Sync to cell-1
        let server1 = create_server_doc("cell-1");
        let oplog1 = server1.oplog_bytes();

        let mut client = create_client_doc("cell-1");
        let mut sync = SyncManager::new();

        sync.apply_initial_state(&mut client, "cell-1", &oplog1)
            .expect("initial sync");
        assert!(!sync.needs_full_sync("cell-1"));

        // Now switch to cell-2 - should need full sync
        assert!(sync.needs_full_sync("cell-2"));

        // Create server for cell-2
        let server2 = create_server_doc("cell-2");
        let oplog2 = server2.oplog_bytes();

        // Apply initial state for cell-2
        let result = sync
            .apply_initial_state(&mut client, "cell-2", &oplog2)
            .expect("sync to cell-2");

        assert!(matches!(result, SyncResult::FullSync { block_count: 1 }));
        assert_eq!(sync.cell_id(), Some("cell-2"));
        assert!(!sync.needs_full_sync("cell-2"));
    }

    // =========================================================================
    // Streaming Tests
    // =========================================================================

    #[test]
    fn test_text_streaming_multiple_chunks() {
        // Setup: server with initial block
        let mut server = create_server_doc("cell-1");
        let initial_oplog = server.oplog_bytes();

        let mut client = create_client_doc("cell-1");
        let mut sync = SyncManager::new();

        sync.apply_initial_state(&mut client, "cell-1", &initial_oplog)
            .expect("initial sync");

        // Server adds a response block
        let server_frontier = server.frontier();
        let block_id = server
            .insert_block(None, None, Role::Model, BlockKind::Text, "", "server")
            .expect("insert block");
        let incremental_ops = serde_json::to_vec(&server.ops_since(&server_frontier)).unwrap();
        let block = server.get_block_snapshot(&block_id).unwrap();

        sync.apply_block_inserted(&mut client, "cell-1", &block, &incremental_ops)
            .expect("insert empty block");

        // Stream text in chunks
        let chunks = ["Hello", ", ", "world", "!"];
        for chunk in chunks {
            let frontier_before = server.frontier();
            server.append_text(&block_id, chunk).expect("append text");
            let chunk_ops = serde_json::to_vec(&server.ops_since(&frontier_before)).unwrap();

            let result = sync
                .apply_text_ops(&mut client, "cell-1", &chunk_ops)
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
        let mut server = create_server_doc("cell-1");
        let initial_oplog = server.oplog_bytes();

        let mut client = create_client_doc("cell-1");
        let mut sync = SyncManager::new();

        sync.apply_initial_state(&mut client, "cell-1", &initial_oplog)
            .expect("initial sync");

        // Add streaming block
        let server_frontier = server.frontier();
        let block_id = server
            .insert_block(None, None, Role::Model, BlockKind::Text, "", "server")
            .expect("insert block");
        let incremental_ops = serde_json::to_vec(&server.ops_since(&server_frontier)).unwrap();
        let block = server.get_block_snapshot(&block_id).unwrap();

        sync.apply_block_inserted(&mut client, "cell-1", &block, &incremental_ops)
            .expect("insert block");

        // Stream chunk 1
        let frontier_before = server.frontier();
        server.append_text(&block_id, "Hello").expect("append");
        let chunk_ops = serde_json::to_vec(&server.ops_since(&frontier_before)).unwrap();
        sync.apply_text_ops(&mut client, "cell-1", &chunk_ops)
            .expect("chunk 1");

        // Corrupt chunk 2 - simulates network corruption
        let corrupt_chunk = b"corrupted data";
        let result = sync.apply_text_ops(&mut client, "cell-1", corrupt_chunk);
        assert!(matches!(result, Err(SyncError::Deserialize(_))));

        // Frontier should be reset
        assert!(sync.needs_full_sync("cell-1"));
    }

    #[test]
    fn test_text_streaming_recovery_after_error() {
        // Setup
        let mut server = create_server_doc("cell-1");
        let initial_oplog = server.oplog_bytes();

        let mut client = create_client_doc("cell-1");
        let mut sync = SyncManager::new();

        sync.apply_initial_state(&mut client, "cell-1", &initial_oplog)
            .expect("initial sync");

        // Cause deserialization error
        let corrupt_ops = b"not json";
        let _ = sync.apply_text_ops(&mut client, "cell-1", corrupt_ops);
        assert!(sync.needs_full_sync("cell-1"));

        // Server adds more content - capture block ID directly
        let new_block_id = server
            .insert_block(None, None, Role::Model, BlockKind::Text, "After error", "server")
            .expect("insert block");
        let full_oplog = server.oplog_bytes();
        let new_block = server.get_block_snapshot(&new_block_id).expect("new block exists");

        // Recovery via full sync
        let result = sync
            .apply_block_inserted(&mut client, "cell-1", &new_block, &full_oplog)
            .expect("recovery");

        assert!(matches!(result, SyncResult::FullSync { block_count: 2 }));
        assert!(!sync.needs_full_sync("cell-1"));
        assert!(client.full_text().contains("After error"));
    }

    // =========================================================================
    // Edge Cases
    // =========================================================================

    #[test]
    fn test_empty_oplog_skips_initial_state() {
        let mut client = create_client_doc("cell-1");
        let mut sync = SyncManager::new();

        let result = sync
            .apply_initial_state(&mut client, "cell-1", &[])
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
        let server = create_server_doc("cell-1");
        let oplog_bytes = server.oplog_bytes();

        let mut client = create_client_doc("cell-1");
        let mut sync = SyncManager::new();

        // Initial sync
        sync.apply_initial_state(&mut client, "cell-1", &oplog_bytes)
            .expect("initial sync");

        // Empty text ops should skip, not fail
        let result = sync
            .apply_text_ops(&mut client, "cell-1", &[])
            .expect("should skip");

        assert!(matches!(
            result,
            SyncResult::Skipped {
                reason: SkipReason::EmptyOplog
            }
        ));

        // Frontier should NOT be reset (it wasn't an error)
        assert!(!sync.needs_full_sync("cell-1"));
    }

    #[test]
    fn test_protocol_violation_no_ops() {
        let server = create_server_doc("cell-1");
        let block = server.blocks_ordered()[0].clone();

        // Create client with matching cell_id (so it won't be rejected for mismatch)
        let mut client = create_client_doc("cell-1");

        // Create SyncManager that thinks it's synced (has frontier)
        // Use with_state() rather than direct field access
        let mut sync = SyncManager::with_state(
            Some("cell-1".to_string()),
            Some(vec![]), // Empty frontier = "synced" state
        );

        // Try to insert block with empty ops - should fail protocol validation
        let result = sync
            .apply_block_inserted(&mut client, "cell-1", &block, &[])
            .expect("should skip");

        assert!(matches!(
            result,
            SyncResult::Skipped {
                reason: SkipReason::ProtocolViolation(_)
            }
        ));
    }

    #[test]
    fn test_reset_clears_frontier_but_keeps_cell_id() {
        let server = create_server_doc("cell-1");
        let oplog_bytes = server.oplog_bytes();

        let mut client = create_client_doc("cell-1");
        let mut sync = SyncManager::new();

        sync.apply_initial_state(&mut client, "cell-1", &oplog_bytes)
            .expect("initial sync");

        assert!(sync.frontier().is_some());
        assert_eq!(sync.cell_id(), Some("cell-1"));

        sync.reset();

        assert!(sync.frontier().is_none());
        // Cell ID is preserved for diagnostics
        assert_eq!(sync.cell_id(), Some("cell-1"));
        // But we still need full sync
        assert!(sync.needs_full_sync("cell-1"));
    }
}
