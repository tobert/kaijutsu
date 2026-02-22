//! High-level CRDT sync wrapper for kaijutsu clients.
//!
//! [`SyncedDocument`] bundles a [`BlockDocument`] with a [`SyncManager`], hiding
//! CRDT internals (Frontier, oplog bytes, SerializedOpsOwned) behind a clean API.
//! Both the Bevy app and MCP server consume this instead of duplicating sync logic.

use kaijutsu_crdt::{BlockDocument, ContextId};
use kaijutsu_types::{BlockId, BlockSnapshot, PrincipalId};
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
    context_id: ContextId,
}

impl SyncedDocument {
    /// Create a new, empty synced document.
    pub fn new(context_id: ContextId, agent_id: PrincipalId) -> Self {
        Self {
            doc: BlockDocument::new(context_id, agent_id),
            sync: SyncManager::new(),
            context_id,
        }
    }

    /// Create from a full [`DocumentState`] (e.g., from `get_document_state` RPC).
    pub fn from_document_state(
        state: &DocumentState,
        agent_id: PrincipalId,
    ) -> Result<Self, SyncError> {
        let mut sd = Self::new(state.context_id, agent_id);
        if !state.ops.is_empty() {
            sd.sync
                .apply_initial_state(&mut sd.doc, state.context_id, &state.ops)?;
        }
        Ok(sd)
    }

    // =========================================================================
    // Read accessors — no CRDT types exposed
    // =========================================================================

    /// The context ID this synced document tracks.
    pub fn context_id(&self) -> ContextId {
        self.context_id
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
        !self.sync.needs_full_sync(self.context_id)
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
                context_id,
                block,
                ops,
            } => {
                if *context_id != self.context_id {
                    return SyncEffect::Ignored;
                }
                match self
                    .sync
                    .apply_block_inserted(&mut self.doc, *context_id, block, ops)
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
                context_id, ops, ..
            } => {
                if *context_id != self.context_id {
                    return SyncEffect::Ignored;
                }
                match self.sync.apply_text_ops(&mut self.doc, *context_id, ops) {
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
                context_id,
                block_id,
                status,
            } => {
                if *context_id != self.context_id {
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
                context_id,
                block_id,
            } => {
                if *context_id != self.context_id {
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
                context_id,
                block_id,
                collapsed,
            } => {
                if *context_id != self.context_id {
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
                context_id,
                block_id,
                after_id,
            } => {
                if *context_id != self.context_id {
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
                context_id,
                generation,
            } => {
                if *context_id != self.context_id {
                    return SyncEffect::Ignored;
                }
                info!(
                    "SyncedDocument: sync reset for {}, generation {}",
                    context_id, generation
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
            .apply_initial_state(&mut self.doc, state.context_id, &state.ops)?;
        match result {
            crate::sync::SyncResult::FullSync { block_count } => {
                self.context_id = state.context_id;
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
    use kaijutsu_types::{BlockKind, Role};

    fn test_context_id() -> ContextId {
        ContextId::new()
    }

    fn test_agent_id() -> PrincipalId {
        PrincipalId::new()
    }

    fn create_server_doc(context_id: ContextId) -> BlockDocument {
        let mut doc = BlockDocument::new(context_id, test_agent_id());
        doc.insert_block(
            None,
            None,
            Role::User,
            BlockKind::Text,
            "Hello from server",
        )
        .expect("insert block");
        doc
    }

    #[test]
    fn test_new_and_from_document_state() {
        let ctx = test_context_id();
        let server = create_server_doc(ctx);
        let oplog = server.oplog_bytes().unwrap();

        let state = DocumentState {
            context_id: ctx,
            blocks: server.blocks_ordered(),
            version: 1,
            ops: oplog,
        };

        let sd = SyncedDocument::from_document_state(&state, test_agent_id()).unwrap();
        assert_eq!(sd.context_id(), ctx);
        assert_eq!(sd.block_count(), 1);
        assert!(sd.is_synced());
    }

    #[test]
    fn test_apply_event_block_inserted() {
        let ctx = test_context_id();
        let mut server = create_server_doc(ctx);
        let initial_oplog = server.oplog_bytes().unwrap();

        let state = DocumentState {
            context_id: ctx,
            blocks: server.blocks_ordered(),
            version: 1,
            ops: initial_oplog,
        };
        let mut sd = SyncedDocument::from_document_state(&state, test_agent_id()).unwrap();

        // Add a new block on the server
        let frontier_before = server.frontier();
        let block_id = server
            .insert_block(
                None,
                None,
                Role::Model,
                BlockKind::Text,
                "Response",
            )
            .unwrap();
        let block = server.get_block_snapshot(&block_id).unwrap();
        let ops = postcard::to_stdvec(&server.ops_since(&frontier_before)).unwrap();

        let effect = sd.apply_event(&ServerEvent::BlockInserted {
            context_id: ctx,
            block: Box::new(block),
            ops,
        });

        assert!(matches!(effect, SyncEffect::Updated { block_count: 2 }));
        assert_eq!(sd.block_count(), 2);
    }

    #[test]
    fn test_apply_event_wrong_document_ignored() {
        let ctx = test_context_id();
        let other_ctx = test_context_id();
        let mut sd = SyncedDocument::new(ctx, test_agent_id());

        let effect = sd.apply_event(&ServerEvent::BlockDeleted {
            context_id: other_ctx,
            block_id: BlockId::new(other_ctx, PrincipalId::new(), 0),
        });

        assert_eq!(effect, SyncEffect::Ignored);
    }

    #[test]
    fn test_apply_event_sync_reset() {
        let ctx = test_context_id();
        let server = create_server_doc(ctx);
        let oplog = server.oplog_bytes().unwrap();
        let state = DocumentState {
            context_id: ctx,
            blocks: server.blocks_ordered(),
            version: 1,
            ops: oplog,
        };
        let mut sd = SyncedDocument::from_document_state(&state, test_agent_id()).unwrap();
        assert!(sd.is_synced());

        let effect = sd.apply_event(&ServerEvent::SyncReset {
            context_id: ctx,
            generation: 1,
        });

        assert_eq!(effect, SyncEffect::NeedsResync);
        assert!(!sd.is_synced());
    }

    #[test]
    fn test_apply_event_text_streaming() {
        let ctx = test_context_id();
        let mut server = create_server_doc(ctx);
        let initial_oplog = server.oplog_bytes().unwrap();
        let state = DocumentState {
            context_id: ctx,
            blocks: server.blocks_ordered(),
            version: 1,
            ops: initial_oplog,
        };
        let mut sd = SyncedDocument::from_document_state(&state, test_agent_id()).unwrap();

        // Create a streaming block
        let frontier_before = server.frontier();
        let block_id = server
            .insert_block(None, None, Role::Model, BlockKind::Text, "")
            .unwrap();
        let block = server.get_block_snapshot(&block_id).unwrap();
        let ops = postcard::to_stdvec(&server.ops_since(&frontier_before)).unwrap();

        sd.apply_event(&ServerEvent::BlockInserted {
            context_id: ctx,
            block: Box::new(block),
            ops,
        });

        // Stream text
        let frontier_before = server.frontier();
        server.append_text(&block_id, "Hello!").unwrap();
        let ops = postcard::to_stdvec(&server.ops_since(&frontier_before)).unwrap();

        let effect = sd.apply_event(&ServerEvent::BlockTextOps {
            context_id: ctx,
            block_id,
            ops,
        });

        assert!(matches!(effect, SyncEffect::Updated { .. }));
        let b = sd.get_block(&block_id).unwrap();
        assert_eq!(b.content, "Hello!");
    }

    #[test]
    fn test_apply_document_state() {
        let ctx = test_context_id();
        let mut sd = SyncedDocument::new(ctx, test_agent_id());
        assert!(!sd.is_synced());

        let server = create_server_doc(ctx);
        let oplog = server.oplog_bytes().unwrap();
        let state = DocumentState {
            context_id: ctx,
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
        let ctx = test_context_id();
        let mut sd = SyncedDocument::new(ctx, test_agent_id());

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
