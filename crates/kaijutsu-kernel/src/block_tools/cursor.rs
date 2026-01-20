//! Cursor tracking for collaborative editing.
//!
//! Tracks cursor positions for each agent and transforms them when edits occur.
//! Cursors are ephemeral (not persisted) and broadcast to clients for display.

use std::collections::HashMap;
use std::sync::Arc;

use kaijutsu_crdt::BlockId;
use parking_lot::RwLock;
use tokio::sync::broadcast;

/// A cursor position within a block.
#[derive(Debug, Clone, PartialEq)]
pub struct CursorPosition {
    /// The block containing the cursor.
    pub block_id: BlockId,
    /// Byte offset within the block content.
    pub offset: usize,
    /// Optional selection end (for ranges).
    pub selection_end: Option<usize>,
}

impl CursorPosition {
    /// Create a new cursor at the given offset.
    pub fn new(block_id: BlockId, offset: usize) -> Self {
        Self {
            block_id,
            offset,
            selection_end: None,
        }
    }

    /// Create a cursor with a selection range.
    pub fn with_selection(block_id: BlockId, start: usize, end: usize) -> Self {
        Self {
            block_id,
            offset: start,
            selection_end: Some(end),
        }
    }

    /// Transform cursor position based on an edit operation.
    ///
    /// # Arguments
    /// * `edit_offset` - Where the edit occurred
    /// * `deleted` - Number of bytes deleted at edit_offset
    /// * `inserted` - Number of bytes inserted at edit_offset
    pub fn transform(&mut self, edit_offset: usize, deleted: usize, inserted: usize) {
        // Transform main cursor offset
        self.offset = transform_offset(self.offset, edit_offset, deleted, inserted);

        // Transform selection end if present
        if let Some(ref mut end) = self.selection_end {
            *end = transform_offset(*end, edit_offset, deleted, inserted);
        }
    }
}

/// Transform a single offset based on an edit operation.
fn transform_offset(offset: usize, edit_offset: usize, deleted: usize, inserted: usize) -> usize {
    if offset <= edit_offset {
        // Cursor is before the edit, unchanged
        offset
    } else if offset <= edit_offset + deleted {
        // Cursor is within the deleted region, move to edit point
        edit_offset + inserted
    } else {
        // Cursor is after the edit, shift by delta
        offset - deleted + inserted
    }
}

/// Event broadcast when a cursor moves.
#[derive(Debug, Clone)]
pub struct CursorEvent {
    /// The agent whose cursor moved.
    pub agent_id: String,
    /// The new cursor position (None if cursor removed).
    pub position: Option<CursorPosition>,
}

/// Tracks cursor positions for all agents.
pub struct CursorTracker {
    /// Current cursor positions by agent ID.
    cursors: Arc<RwLock<HashMap<String, CursorPosition>>>,
    /// Broadcast channel for cursor updates.
    sender: broadcast::Sender<CursorEvent>,
}

impl CursorTracker {
    /// Create a new cursor tracker.
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(256);
        Self {
            cursors: Arc::new(RwLock::new(HashMap::new())),
            sender,
        }
    }

    /// Subscribe to cursor events.
    pub fn subscribe(&self) -> broadcast::Receiver<CursorEvent> {
        self.sender.subscribe()
    }

    /// Set an agent's cursor position.
    pub fn set_cursor(&self, agent_id: impl Into<String>, position: CursorPosition) {
        let agent_id = agent_id.into();
        {
            let mut cursors = self.cursors.write();
            cursors.insert(agent_id.clone(), position.clone());
        }
        let _ = self.sender.send(CursorEvent {
            agent_id,
            position: Some(position),
        });
    }

    /// Remove an agent's cursor.
    pub fn remove_cursor(&self, agent_id: &str) {
        {
            let mut cursors = self.cursors.write();
            cursors.remove(agent_id);
        }
        let _ = self.sender.send(CursorEvent {
            agent_id: agent_id.to_string(),
            position: None,
        });
    }

    /// Get an agent's current cursor position.
    pub fn get_cursor(&self, agent_id: &str) -> Option<CursorPosition> {
        let cursors = self.cursors.read();
        cursors.get(agent_id).cloned()
    }

    /// Get all cursor positions.
    pub fn all_cursors(&self) -> HashMap<String, CursorPosition> {
        let cursors = self.cursors.read();
        cursors.clone()
    }

    /// Get cursors in a specific block.
    pub fn cursors_in_block(&self, block_id: &BlockId) -> Vec<(String, CursorPosition)> {
        let cursors = self.cursors.read();
        cursors
            .iter()
            .filter(|(_, pos)| &pos.block_id == block_id)
            .map(|(agent, pos)| (agent.clone(), pos.clone()))
            .collect()
    }

    /// Transform all cursors in a block after an edit.
    ///
    /// Call this after any edit operation to keep cursors in sync.
    pub fn transform_cursors(
        &self,
        block_id: &BlockId,
        edit_offset: usize,
        deleted: usize,
        inserted: usize,
    ) {
        let mut updated = Vec::new();

        {
            let mut cursors = self.cursors.write();
            for (agent_id, position) in cursors.iter_mut() {
                if &position.block_id == block_id {
                    position.transform(edit_offset, deleted, inserted);
                    updated.push((agent_id.clone(), position.clone()));
                }
            }
        }

        // Broadcast cursor updates
        for (agent_id, position) in updated {
            let _ = self.sender.send(CursorEvent {
                agent_id,
                position: Some(position),
            });
        }
    }

    /// Move a cursor by a relative delta.
    pub fn move_cursor(&self, agent_id: &str, delta: isize) -> Option<CursorPosition> {
        let mut result = None;

        {
            let mut cursors = self.cursors.write();
            if let Some(position) = cursors.get_mut(agent_id) {
                let new_offset = if delta < 0 {
                    position.offset.saturating_sub((-delta) as usize)
                } else {
                    position.offset.saturating_add(delta as usize)
                };
                position.offset = new_offset;
                position.selection_end = None; // Clear selection on move
                result = Some(position.clone());
            }
        }

        if let Some(ref pos) = result {
            let _ = self.sender.send(CursorEvent {
                agent_id: agent_id.to_string(),
                position: Some(pos.clone()),
            });
        }

        result
    }
}

impl Default for CursorTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_block_id() -> BlockId {
        BlockId::new("test-cell", "test-agent", 1)
    }

    #[test]
    fn test_cursor_position() {
        let block_id = test_block_id();
        let cursor = CursorPosition::new(block_id.clone(), 10);
        assert_eq!(cursor.offset, 10);
        assert_eq!(cursor.selection_end, None);

        let cursor = CursorPosition::with_selection(block_id, 5, 15);
        assert_eq!(cursor.offset, 5);
        assert_eq!(cursor.selection_end, Some(15));
    }

    #[test]
    fn test_transform_insert_before() {
        let block_id = test_block_id();
        let mut cursor = CursorPosition::new(block_id, 10);

        // Insert 5 chars at position 5 (before cursor)
        cursor.transform(5, 0, 5);
        assert_eq!(cursor.offset, 15, "cursor should shift right after insert before");
    }

    #[test]
    fn test_transform_insert_after() {
        let block_id = test_block_id();
        let mut cursor = CursorPosition::new(block_id, 10);

        // Insert 5 chars at position 15 (after cursor)
        cursor.transform(15, 0, 5);
        assert_eq!(cursor.offset, 10, "cursor should stay put after insert after");
    }

    #[test]
    fn test_transform_delete_before() {
        let block_id = test_block_id();
        let mut cursor = CursorPosition::new(block_id, 10);

        // Delete 3 chars at position 2 (before cursor)
        cursor.transform(2, 3, 0);
        assert_eq!(cursor.offset, 7, "cursor should shift left after delete before");
    }

    #[test]
    fn test_transform_delete_overlapping() {
        let block_id = test_block_id();
        let mut cursor = CursorPosition::new(block_id, 10);

        // Delete 10 chars at position 5 (overlaps cursor at 10)
        cursor.transform(5, 10, 0);
        assert_eq!(cursor.offset, 5, "cursor should move to delete point when overlapped");
    }

    #[test]
    fn test_transform_replace() {
        let block_id = test_block_id();
        let mut cursor = CursorPosition::new(block_id, 10);

        // Replace 3 chars with 5 chars at position 5 (delete 3, insert 5)
        cursor.transform(5, 3, 5);
        assert_eq!(cursor.offset, 12, "cursor should account for net change");
    }

    #[test]
    fn test_cursor_tracker_basic() {
        let tracker = CursorTracker::new();
        let block_id = test_block_id();

        // Set cursor
        tracker.set_cursor("agent1", CursorPosition::new(block_id.clone(), 10));

        // Get cursor
        let pos = tracker.get_cursor("agent1").unwrap();
        assert_eq!(pos.offset, 10);

        // Remove cursor
        tracker.remove_cursor("agent1");
        assert!(tracker.get_cursor("agent1").is_none());
    }

    #[test]
    fn test_cursor_tracker_transform() {
        let tracker = CursorTracker::new();
        let block_id = test_block_id();

        tracker.set_cursor("agent1", CursorPosition::new(block_id.clone(), 10));
        tracker.set_cursor("agent2", CursorPosition::new(block_id.clone(), 20));

        // Simulate insert at position 5
        tracker.transform_cursors(&block_id, 5, 0, 5);

        let pos1 = tracker.get_cursor("agent1").unwrap();
        let pos2 = tracker.get_cursor("agent2").unwrap();
        assert_eq!(pos1.offset, 15);
        assert_eq!(pos2.offset, 25);
    }

    #[test]
    fn test_cursor_tracker_different_blocks() {
        let tracker = CursorTracker::new();
        let block1 = test_block_id();
        let block2 = BlockId::new("other-cell", "test-agent", 2);

        tracker.set_cursor("agent1", CursorPosition::new(block1.clone(), 10));
        tracker.set_cursor("agent2", CursorPosition::new(block2.clone(), 10));

        // Transform only affects block1
        tracker.transform_cursors(&block1, 5, 0, 5);

        let pos1 = tracker.get_cursor("agent1").unwrap();
        let pos2 = tracker.get_cursor("agent2").unwrap();
        assert_eq!(pos1.offset, 15, "block1 cursor should transform");
        assert_eq!(pos2.offset, 10, "block2 cursor should not transform");
    }

    #[test]
    fn test_move_cursor() {
        let tracker = CursorTracker::new();
        let block_id = test_block_id();

        tracker.set_cursor("agent1", CursorPosition::new(block_id, 10));

        // Move forward
        let pos = tracker.move_cursor("agent1", 5).unwrap();
        assert_eq!(pos.offset, 15);

        // Move backward
        let pos = tracker.move_cursor("agent1", -3).unwrap();
        assert_eq!(pos.offset, 12);

        // Move backward past start
        let pos = tracker.move_cursor("agent1", -100).unwrap();
        assert_eq!(pos.offset, 0, "should saturate at 0");
    }

    #[test]
    fn test_cursors_in_block() {
        let tracker = CursorTracker::new();
        let block1 = test_block_id();
        let block2 = BlockId::new("other-cell", "test-agent", 2);

        tracker.set_cursor("agent1", CursorPosition::new(block1.clone(), 10));
        tracker.set_cursor("agent2", CursorPosition::new(block1.clone(), 20));
        tracker.set_cursor("agent3", CursorPosition::new(block2, 30));

        let cursors = tracker.cursors_in_block(&block1);
        assert_eq!(cursors.len(), 2);
    }

    #[tokio::test]
    async fn test_cursor_events() {
        let tracker = CursorTracker::new();
        let block_id = test_block_id();
        let mut rx = tracker.subscribe();

        tracker.set_cursor("agent1", CursorPosition::new(block_id, 10));

        let event = rx.recv().await.unwrap();
        assert_eq!(event.agent_id, "agent1");
        assert!(event.position.is_some());
        assert_eq!(event.position.unwrap().offset, 10);
    }
}
