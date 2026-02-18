//! High-level CRDT sync wrapper for kaijutsu clients.
//!
//! [`SyncedDocument`] bundles a [`BlockDocument`] with a [`SyncManager`], hiding
//! CRDT internals (Frontier, oplog bytes, SerializedOpsOwned) behind a clean API.
//! Both the Bevy app and MCP server consume this instead of duplicating sync logic.

use kaijutsu_crdt::{BlockDocument, BlockId, BlockSnapshot};
use tracing::{info, warn};

use crate::rpc::DocumentState;
use crate::subscriptions::ServerEvent;
use crate::sync::{SyncError, SyncManager};

/// Result of applying an event to a [`SyncedDocument`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncEffect {
    /// Document updated, here's the current block count.
    Updated { block_count: usize },
    /// Full sync performed (initial state or reset).
    FullSync { block_count: usize },
    /// Event was for a different document or not CRDT-relevant — ignored.
    Ignored,
    /// Need full state from server (call `get_document_state`).
    NeedsResync,
}

/// A CRDT document with integrated sync state.
///
/// Wraps [`BlockDocument`] + [`SyncManager`] so consumers don't need to know
/// about Frontier, oplog bytes, or SerializedOpsOwned. The single `apply_event`
/// method replaces the 40+ line match blocks in both Bevy and MCP consumers.
pub struct SyncedDocument {
    doc: BlockDocument,
    sync: SyncManager,
    document_id: String,
}

impl SyncedDocument {
    /// Create a new, empty synced document.
    pub fn new(document_id: &str, agent_id: &str) -> Self {
        Self {
            doc: BlockDocument::new(document_id, agent_id),
            sync: SyncManager::new(),
            document_id: document_id.to_string(),
        }
    }

    /// Create from a full [`DocumentState`] (e.g., from `get_document_state` RPC).
    pub fn from_document_state(
        state: &DocumentState,
        agent_id: &str,
    ) -> Result<Self, SyncError> {
        let mut sd = Self::new(&state.document_id, agent_id);
        if !state.ops.is_empty() {
            sd.sync
                .apply_initial_state(&mut sd.doc, &state.document_id, &state.ops)?;
        }
        Ok(sd)
    }

    // =========================================================================
    // Read accessors — no CRDT types exposed
    // =========================================================================

    /// The document ID this synced document tracks.
    pub fn document_id(&self) -> &str {
        &self.document_id
    }

    /// All blocks in document order.
    pub fn blocks(&self) -> Vec<BlockSnapshot> {
        self.doc.blocks_ordered()
    }

    /// Number of blocks.
    pub fn block_count(&self) -> usize {
        self.doc.block_count()
    }

    /// Look up a single block.
    pub fn get_block(&self, id: &BlockId) -> Option<BlockSnapshot> {
        self.doc.get_block_snapshot(id)
    }

    /// Sync version counter (bumped on every successful sync).
    pub fn version(&self) -> u64 {
        self.sync.version()
    }

    /// Whether we're in a synced state (not waiting for full resync).
    pub fn is_synced(&self) -> bool {
        !self.sync.needs_full_sync(&self.document_id)
    }

    // =========================================================================
    // Apply incoming events
    // =========================================================================

    /// Apply a single server event. This is the primary consumer API.
    ///
    /// Handles all CRDT-relevant event variants internally:
    /// - `BlockInserted` → SyncManager insert (full or incremental)
    /// - `BlockTextOps` → SyncManager text merge
    /// - `BlockStatusChanged` → direct doc mutation
    /// - `BlockDeleted` → direct doc mutation
    /// - `BlockCollapsedChanged` → direct doc mutation
    /// - `BlockMoved` → direct doc mutation
    /// - `SyncReset` → returns `NeedsResync`
    /// - Resource events → `Ignored`
    pub fn apply_event(&mut self, event: &ServerEvent) -> SyncEffect {
        match event {
            ServerEvent::BlockInserted {
                document_id,
                block,
                ops,
            } => {
                if document_id != &self.document_id {
                    return SyncEffect::Ignored;
                }
                match self
                    .sync
                    .apply_block_inserted(&mut self.doc, document_id, block, ops)
                {
                    Ok(crate::sync::SyncResult::FullSync { block_count }) => {
                        SyncEffect::FullSync { block_count }
                    }
                    Ok(_) => SyncEffect::Updated {
                        block_count: self.doc.block_count(),
                    },
                    Err(e) => {
                        warn!("SyncedDocument: block insert error: {e}");
                        SyncEffect::Updated {
                            block_count: self.doc.block_count(),
                        }
                    }
                }
            }

            ServerEvent::BlockTextOps {
                document_id, ops, ..
            } => {
                if document_id != &self.document_id {
                    return SyncEffect::Ignored;
                }
                match self.sync.apply_text_ops(&mut self.doc, document_id, ops) {
                    Ok(_) => SyncEffect::Updated {
                        block_count: self.doc.block_count(),
                    },
                    Err(e) => {
                        warn!("SyncedDocument: text ops error: {e}");
                        SyncEffect::Updated {
                            block_count: self.doc.block_count(),
                        }
                    }
                }
            }

            ServerEvent::BlockStatusChanged {
                document_id,
                block_id,
                status,
            } => {
                if document_id != &self.document_id {
                    return SyncEffect::Ignored;
                }
                if let Err(e) = self.doc.set_status(block_id, *status) {
                    warn!("SyncedDocument: set_status error: {e}");
                }
                SyncEffect::Updated {
                    block_count: self.doc.block_count(),
                }
            }

            ServerEvent::BlockDeleted {
                document_id,
                block_id,
            } => {
                if document_id != &self.document_id {
                    return SyncEffect::Ignored;
                }
                if let Err(e) = self.doc.delete_block(block_id) {
                    warn!("SyncedDocument: delete_block error: {e}");
                }
                SyncEffect::Updated {
                    block_count: self.doc.block_count(),
                }
            }

            ServerEvent::BlockCollapsedChanged {
                document_id,
                block_id,
                collapsed,
            } => {
                if document_id != &self.document_id {
                    return SyncEffect::Ignored;
                }
                if let Err(e) = self.doc.set_collapsed(block_id, *collapsed) {
                    warn!("SyncedDocument: set_collapsed error: {e}");
                }
                SyncEffect::Updated {
                    block_count: self.doc.block_count(),
                }
            }

            ServerEvent::BlockMoved {
                document_id,
                block_id,
                after_id,
            } => {
                if document_id != &self.document_id {
                    return SyncEffect::Ignored;
                }
                if let Err(e) = self.doc.move_block(block_id, after_id.as_ref()) {
                    warn!("SyncedDocument: move_block error: {e}");
                }
                SyncEffect::Updated {
                    block_count: self.doc.block_count(),
                }
            }

            ServerEvent::SyncReset {
                document_id,
                generation,
            } => {
                if document_id != &self.document_id {
                    return SyncEffect::Ignored;
                }
                info!(
                    "SyncedDocument: sync reset for {}, generation {}",
                    document_id, generation
                );
                self.sync.reset_frontier();
                SyncEffect::NeedsResync
            }

            // Resource events don't affect document state
            ServerEvent::ResourceUpdated { .. } | ServerEvent::ResourceListChanged { .. } => {
                SyncEffect::Ignored
            }
        }
    }

    /// Apply a full document state (from `get_document_state` RPC or reconnect).
    pub fn apply_document_state(
        &mut self,
        state: &DocumentState,
    ) -> Result<SyncEffect, SyncError> {
        if state.ops.is_empty() {
            return Ok(SyncEffect::Ignored);
        }
        let result = self
            .sync
            .apply_initial_state(&mut self.doc, &state.document_id, &state.ops)?;
        match result {
            crate::sync::SyncResult::FullSync { block_count } => {
                self.document_id = state.document_id.clone();
                Ok(SyncEffect::FullSync { block_count })
            }
            _ => Ok(SyncEffect::Updated {
                block_count: self.doc.block_count(),
            }),
        }
    }

    /// Reset sync state — forces full resync on next event.
    pub fn reset(&mut self) {
        self.sync.reset();
    }

    /// Reset frontier — forces full resync (used after compaction).
    pub fn reset_frontier(&mut self) {
        self.sync.reset_frontier();
    }

    // =========================================================================
    // Escape hatches — for consumers that need internals
    // =========================================================================

    /// Direct access to the underlying document.
    pub fn doc(&self) -> &BlockDocument {
        &self.doc
    }

    /// Mutable access to the underlying document.
    pub fn doc_mut(&mut self) -> &mut BlockDocument {
        &mut self.doc
    }

    /// Direct access to the underlying sync manager.
    pub fn sync(&self) -> &SyncManager {
        &self.sync
    }

    /// Mutable access to the underlying sync manager.
    pub fn sync_mut(&mut self) -> &mut SyncManager {
        &mut self.sync
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_crdt::{BlockKind, Role};

    fn create_server_doc(document_id: &str) -> BlockDocument {
        let mut doc = BlockDocument::new(document_id, "server-agent");
        doc.insert_block(
            None,
            None,
            Role::User,
            BlockKind::Text,
            "Hello from server",
            "server",
        )
        .expect("insert block");
        doc
    }

    #[test]
    fn test_new_and_from_document_state() {
        let server = create_server_doc("doc-1");
        let oplog = server.oplog_bytes().unwrap();

        let state = DocumentState {
            document_id: "doc-1".to_string(),
            blocks: server.blocks_ordered(),
            version: 1,
            ops: oplog,
        };

        let sd = SyncedDocument::from_document_state(&state, "client").unwrap();
        assert_eq!(sd.document_id(), "doc-1");
        assert_eq!(sd.block_count(), 1);
        assert!(sd.is_synced());
    }

    #[test]
    fn test_apply_event_block_inserted() {
        let mut server = create_server_doc("doc-1");
        let initial_oplog = server.oplog_bytes().unwrap();

        let state = DocumentState {
            document_id: "doc-1".to_string(),
            blocks: server.blocks_ordered(),
            version: 1,
            ops: initial_oplog,
        };
        let mut sd = SyncedDocument::from_document_state(&state, "client").unwrap();

        // Add a new block on the server
        let frontier_before = server.frontier();
        let block_id = server
            .insert_block(
                None,
                None,
                Role::Model,
                BlockKind::Text,
                "Response",
                "server",
            )
            .unwrap();
        let block = server.get_block_snapshot(&block_id).unwrap();
        let ops = postcard::to_stdvec(&server.ops_since(&frontier_before)).unwrap();

        let effect = sd.apply_event(&ServerEvent::BlockInserted {
            document_id: "doc-1".to_string(),
            block: Box::new(block),
            ops,
        });

        assert!(matches!(effect, SyncEffect::Updated { block_count: 2 }));
        assert_eq!(sd.block_count(), 2);
    }

    #[test]
    fn test_apply_event_wrong_document_ignored() {
        let mut sd = SyncedDocument::new("doc-1", "client");

        let effect = sd.apply_event(&ServerEvent::BlockDeleted {
            document_id: "doc-2".to_string(),
            block_id: BlockId {
                document_id: "doc-2".to_string(),
                agent_id: "x".to_string(),
                seq: 0,
            },
        });

        assert_eq!(effect, SyncEffect::Ignored);
    }

    #[test]
    fn test_apply_event_sync_reset() {
        let server = create_server_doc("doc-1");
        let oplog = server.oplog_bytes().unwrap();
        let state = DocumentState {
            document_id: "doc-1".to_string(),
            blocks: server.blocks_ordered(),
            version: 1,
            ops: oplog,
        };
        let mut sd = SyncedDocument::from_document_state(&state, "client").unwrap();
        assert!(sd.is_synced());

        let effect = sd.apply_event(&ServerEvent::SyncReset {
            document_id: "doc-1".to_string(),
            generation: 1,
        });

        assert_eq!(effect, SyncEffect::NeedsResync);
        assert!(!sd.is_synced());
    }

    #[test]
    fn test_apply_event_text_streaming() {
        let mut server = create_server_doc("doc-1");
        let initial_oplog = server.oplog_bytes().unwrap();
        let state = DocumentState {
            document_id: "doc-1".to_string(),
            blocks: server.blocks_ordered(),
            version: 1,
            ops: initial_oplog,
        };
        let mut sd = SyncedDocument::from_document_state(&state, "client").unwrap();

        // Create a streaming block
        let frontier_before = server.frontier();
        let block_id = server
            .insert_block(None, None, Role::Model, BlockKind::Text, "", "server")
            .unwrap();
        let block = server.get_block_snapshot(&block_id).unwrap();
        let ops = postcard::to_stdvec(&server.ops_since(&frontier_before)).unwrap();

        sd.apply_event(&ServerEvent::BlockInserted {
            document_id: "doc-1".to_string(),
            block: Box::new(block),
            ops,
        });

        // Stream text
        let frontier_before = server.frontier();
        server.append_text(&block_id, "Hello!").unwrap();
        let ops = postcard::to_stdvec(&server.ops_since(&frontier_before)).unwrap();

        let effect = sd.apply_event(&ServerEvent::BlockTextOps {
            document_id: "doc-1".to_string(),
            block_id: block_id.clone(),
            ops,
        });

        assert!(matches!(effect, SyncEffect::Updated { .. }));
        let b = sd.get_block(&block_id).unwrap();
        assert_eq!(b.content, "Hello!");
    }

    #[test]
    fn test_apply_document_state() {
        let mut sd = SyncedDocument::new("doc-1", "client");
        assert!(!sd.is_synced());

        let server = create_server_doc("doc-1");
        let oplog = server.oplog_bytes().unwrap();
        let state = DocumentState {
            document_id: "doc-1".to_string(),
            blocks: server.blocks_ordered(),
            version: 1,
            ops: oplog,
        };

        let effect = sd.apply_document_state(&state).unwrap();
        assert!(matches!(effect, SyncEffect::FullSync { block_count: 1 }));
        assert!(sd.is_synced());
        assert_eq!(sd.block_count(), 1);
    }

    #[test]
    fn test_resource_events_ignored() {
        let mut sd = SyncedDocument::new("doc-1", "client");

        let effect = sd.apply_event(&ServerEvent::ResourceUpdated {
            server: "test".to_string(),
            uri: "test://foo".to_string(),
        });
        assert_eq!(effect, SyncEffect::Ignored);

        let effect = sd.apply_event(&ServerEvent::ResourceListChanged {
            server: "test".to_string(),
        });
        assert_eq!(effect, SyncEffect::Ignored);
    }
}
