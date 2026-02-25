//! High-level CRDT sync wrapper for kaijutsu clients.
//!
//! [`SyncedDocument`] bundles a [`CrdtBlockStore`] with a [`SyncManager`], hiding
//! CRDT internals (Frontier, SyncPayload, StoreSnapshot) behind a clean API.
//! Both the Bevy app and MCP server consume this instead of duplicating sync logic.

use kaijutsu_crdt::block_store::BlockStore as CrdtBlockStore;
use kaijutsu_crdt::ContextId;
use kaijutsu_types::{BlockId, BlockSnapshot, PrincipalId};
use tracing::{info, warn};

use crate::rpc::SyncState;
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
/// Wraps [`CrdtBlockStore`] + [`SyncManager`] so consumers don't need to know
/// about Frontier, SyncPayload, or StoreSnapshot. The single `apply_event`
/// method replaces the 40+ line match blocks in both Bevy and MCP consumers.
pub struct SyncedDocument {
    doc: CrdtBlockStore,
    sync: SyncManager,
    context_id: ContextId,
}

impl SyncedDocument {
    /// Create a new, empty synced document.
    pub fn new(context_id: ContextId, agent_id: PrincipalId) -> Self {
        Self {
            doc: CrdtBlockStore::new(context_id, agent_id),
            sync: SyncManager::new(),
            context_id,
        }
    }

    /// Create from a [`SyncState`] (ops + version, no blocks).
    pub fn from_sync_state(
        state: &SyncState,
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

    /// Get a store snapshot (for rendering pipeline).
    pub fn snapshot(&self) -> kaijutsu_crdt::block_store::StoreSnapshot {
        self.doc.snapshot()
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

            // Resource and input document events don't affect conversation document state
            ServerEvent::ResourceUpdated { .. }
            | ServerEvent::ResourceListChanged { .. }
            | ServerEvent::InputTextOps { .. }
            | ServerEvent::InputCleared { .. } => SyncEffect::Ignored,
        }
    }

    /// Apply a sync state (from `get_context_sync` RPC or reconnect).
    pub fn apply_sync_state(
        &mut self,
        state: &SyncState,
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

    /// Direct access to the underlying block store.
    pub fn doc(&self) -> &CrdtBlockStore {
        &self.doc
    }

    /// Mutable access to the underlying block store.
    pub fn doc_mut(&mut self) -> &mut CrdtBlockStore {
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
    use kaijutsu_crdt::block_store::BlockStore as CrdtBlockStore;
    use kaijutsu_types::{BlockKind, Role};
    use std::collections::HashMap;

    fn test_context_id() -> ContextId {
        ContextId::new()
    }

    fn test_agent_id() -> PrincipalId {
        PrincipalId::new()
    }

    fn create_server_store(context_id: ContextId) -> CrdtBlockStore {
        let mut store = CrdtBlockStore::new(context_id, test_agent_id());
        store
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "Hello from server",
            )
            .expect("insert block");
        store
    }

    /// Helper: serialize a StoreSnapshot to postcard bytes.
    fn snapshot_bytes(store: &CrdtBlockStore) -> Vec<u8> {
        postcard::to_allocvec(&store.snapshot()).expect("serialize snapshot")
    }

    /// Helper: serialize a SyncPayload to postcard bytes.
    fn sync_payload_bytes(
        store: &CrdtBlockStore,
        frontiers: &HashMap<BlockId, kaijutsu_crdt::Frontier>,
    ) -> Vec<u8> {
        postcard::to_allocvec(&store.ops_since(frontiers)).expect("serialize sync payload")
    }

    #[test]
    fn test_new_and_from_sync_state() {
        let ctx = test_context_id();
        let server = create_server_store(ctx);
        let snap = snapshot_bytes(&server);

        let state = SyncState {
            context_id: ctx,

            version: 1,
            ops: snap,
        };

        let sd = SyncedDocument::from_sync_state(&state, test_agent_id()).unwrap();
        assert_eq!(sd.context_id(), ctx);
        assert_eq!(sd.block_count(), 1);
        assert!(sd.is_synced());
    }

    #[test]
    fn test_apply_event_block_inserted() {
        let ctx = test_context_id();
        let mut server = create_server_store(ctx);
        let initial_snap = snapshot_bytes(&server);

        let state = SyncState {
            context_id: ctx,

            version: 1,
            ops: initial_snap,
        };
        let mut sd = SyncedDocument::from_sync_state(&state, test_agent_id()).unwrap();

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
        let ops = sync_payload_bytes(&server, &frontier_before);

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
        let server = create_server_store(ctx);
        let snap = snapshot_bytes(&server);
        let state = SyncState {
            context_id: ctx,

            version: 1,
            ops: snap,
        };
        let mut sd = SyncedDocument::from_sync_state(&state, test_agent_id()).unwrap();
        assert!(sd.is_synced());

        let effect = sd.apply_event(&ServerEvent::SyncReset {
            context_id: ctx,
            generation: 1,
        });

        assert_eq!(effect, SyncEffect::NeedsResync);
        assert!(!sd.is_synced());
    }

    #[test]
    fn test_apply_event_text_via_snapshot() {
        // Per-block DTE: after merge_ops creates a block from snapshot,
        // incremental DTE text ops fail (DataMissing) because the client
        // has a fresh DTE document. Text updates arrive as full snapshots
        // via apply_sync_state instead.
        let ctx = test_context_id();
        let mut server = create_server_store(ctx);
        let initial_snap = snapshot_bytes(&server);
        let state = SyncState {
            context_id: ctx,

            version: 1,
            ops: initial_snap,
        };
        let mut sd = SyncedDocument::from_sync_state(&state, test_agent_id()).unwrap();

        // Create a streaming block and add text on the server
        let block_id = server
            .insert_block(None, None, Role::Model, BlockKind::Text, "Hello!")
            .unwrap();

        // Client gets updated snapshot
        let updated_snap = snapshot_bytes(&server);
        let updated_state = SyncState {
            context_id: ctx,

            version: 2,
            ops: updated_snap,
        };
        let effect = sd.apply_sync_state(&updated_state).unwrap();

        assert!(matches!(effect, SyncEffect::FullSync { block_count: 2 }));
        let b = sd.get_block(&block_id).unwrap();
        assert_eq!(b.content, "Hello!");
    }

    #[test]
    fn test_apply_sync_state() {
        let ctx = test_context_id();
        let mut sd = SyncedDocument::new(ctx, test_agent_id());
        assert!(!sd.is_synced());

        let server = create_server_store(ctx);
        let snap = snapshot_bytes(&server);
        let state = SyncState {
            context_id: ctx,

            version: 1,
            ops: snap,
        };

        let effect = sd.apply_sync_state(&state).unwrap();
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

    /// End-to-end test: SyncedDocument → snapshot → BlockDocument.
    ///
    /// Simulates the rendering pipeline path: events arrive via apply_event,
    /// then sync_main_cell_to_conversation takes a snapshot and rebuilds
    /// a BlockDocument for display. Verifies block count, order, and content.
    #[test]
    fn test_synced_document_to_block_document_rendering_chain() {
        let ctx = test_context_id();
        let server_agent = test_agent_id();
        let client_agent = test_agent_id();

        // Server creates a realistic conversation with explicit ordering
        let mut server = CrdtBlockStore::new(ctx, server_agent);
        let b1 = server.insert_block(None, None, Role::User, BlockKind::Text, "Hello").unwrap();
        let b2 = server.insert_block(None, Some(&b1), Role::Model, BlockKind::Text, "Hi there").unwrap();
        let b3 = server.insert_block(Some(&b2), Some(&b2), Role::Model, BlockKind::ToolCall, "search").unwrap();

        // Client syncs initial state via SyncState
        let snap = snapshot_bytes(&server);
        let state = SyncState {
            context_id: ctx,

            version: 1,
            ops: snap,
        };
        let mut sd = SyncedDocument::from_sync_state(&state, client_agent).unwrap();
        assert_eq!(sd.block_count(), 3);

        // Server adds a new block (after b3)
        let frontier_before = server.frontier();
        let b4 = server.insert_block(Some(&b3), Some(&b3), Role::Model, BlockKind::ToolResult, "found it").unwrap();
        let _ = b4;
        let ops = sync_payload_bytes(&server, &frontier_before);
        // Get snapshot of the new block for the event
        let block = server.blocks_ordered().into_iter().last().unwrap();

        // Apply via ServerEvent::BlockInserted
        let effect = sd.apply_event(&ServerEvent::BlockInserted {
            context_id: ctx,
            block: Box::new(block),
            ops,
        });
        assert!(matches!(effect, SyncEffect::Updated { block_count: 4 }));

        // Simulate sync_main_cell_to_conversation: snapshot → BlockDocument
        let store_snap = sd.snapshot();
        let doc_snap = kaijutsu_crdt::DocumentSnapshot {
            context_id: store_snap.context_id,
            blocks: store_snap.blocks,
            version: sd.version(),
        };
        let doc = kaijutsu_crdt::BlockDocument::from_snapshot(doc_snap, client_agent);
        let blocks = doc.blocks_ordered();

        assert_eq!(blocks.len(), 4);
        assert_eq!(blocks[0].content, "Hello");
        assert_eq!(blocks[1].content, "Hi there");
        assert_eq!(blocks[2].kind, BlockKind::ToolCall);
        assert_eq!(blocks[3].content, "found it");
    }

    /// Verify SyncedDocument recovers from text ops failures.
    ///
    /// If a text ops merge fails (corrupt data), subsequent BlockInserted
    /// events should still succeed. This was the root cause of the
    /// "stuck scroll" bug when using the dual sync path.
    #[test]
    fn test_synced_document_resilient_to_text_ops_failure() {
        let ctx = test_context_id();
        let client_agent = test_agent_id();

        // Server creates initial block
        let server = create_server_store(ctx);

        // Client syncs initial state
        let snap = snapshot_bytes(&server);
        let state = SyncState {
            context_id: ctx,

            version: 1,
            ops: snap,
        };
        let mut sd = SyncedDocument::from_sync_state(&state, client_agent).unwrap();
        let v1 = sd.version();

        // Apply corrupt text ops — should fail gracefully
        let corrupt_event = ServerEvent::BlockTextOps {
            context_id: ctx,
            block_id: BlockId::new(ctx, PrincipalId::new(), 999),
            ops: vec![0xFF, 0xFE, 0xFD],
        };
        let effect = sd.apply_event(&corrupt_event);
        // Should still return Updated (error is logged but doesn't crash)
        assert!(matches!(effect, SyncEffect::Updated { .. }));

        // Now add a new block on server and sync it
        let mut server2 = server; // move
        let frontier_before = server2.frontier();
        let b2 = server2.insert_block(None, None, Role::Model, BlockKind::Text, "Response").unwrap();
        let ops = sync_payload_bytes(&server2, &frontier_before);
        let block = server2.get_block_snapshot(&b2).unwrap();

        let effect = sd.apply_event(&ServerEvent::BlockInserted {
            context_id: ctx,
            block: Box::new(block),
            ops,
        });

        // Should succeed — the corrupt text ops didn't break the sync pipeline
        assert!(matches!(effect, SyncEffect::Updated { block_count: 2 }));
        assert!(sd.version() > v1, "version should have advanced");
    }
}
