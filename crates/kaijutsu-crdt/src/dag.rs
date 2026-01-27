//! Computed DAG index from CRDT data.
//!
//! The ConversationDAG provides efficient tree traversal operations
//! computed from the flat block list in BlockDocument.

use std::collections::HashMap;

use crate::{BlockDocument, BlockId, BlockSnapshot};

/// Computed DAG index from CRDT data.
///
/// This is an ephemeral structure computed from BlockDocument data.
/// It provides efficient traversal without modifying the underlying CRDT.
#[derive(Debug, Clone)]
pub struct ConversationDAG {
    /// Root blocks (no parent).
    pub roots: Vec<BlockId>,
    /// Children indexed by parent ID.
    pub children: HashMap<BlockId, Vec<BlockId>>,
    /// All blocks indexed by ID.
    pub blocks: HashMap<BlockId, BlockSnapshot>,
}

impl ConversationDAG {
    /// Build a DAG from a BlockDocument.
    pub fn from_document(doc: &BlockDocument) -> Self {
        let snapshots = doc.blocks_ordered();

        let mut roots = Vec::new();
        let mut children: HashMap<BlockId, Vec<BlockId>> = HashMap::new();
        let mut blocks = HashMap::new();

        for snap in snapshots {
            if let Some(ref parent_id) = snap.parent_id {
                children.entry(parent_id.clone()).or_default().push(snap.id.clone());
            } else {
                roots.push(snap.id.clone());
            }
            blocks.insert(snap.id.clone(), snap);
        }

        Self { roots, children, blocks }
    }

    /// Get a block by ID.
    pub fn get(&self, id: &BlockId) -> Option<&BlockSnapshot> {
        self.blocks.get(id)
    }

    /// Get children of a block.
    pub fn get_children(&self, id: &BlockId) -> &[BlockId] {
        self.children.get(id).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Iterate blocks in depth-first order.
    ///
    /// Returns (depth, block) pairs where depth is 0 for roots.
    pub fn iter_dfs(&self) -> impl Iterator<Item = (usize, &BlockSnapshot)> {
        DfsIterator::new(self)
    }

    /// Iterate blocks in breadth-first order.
    ///
    /// Returns (depth, block) pairs where depth is 0 for roots.
    pub fn iter_bfs(&self) -> impl Iterator<Item = (usize, &BlockSnapshot)> {
        BfsIterator::new(self)
    }

    /// Get all blocks in a subtree rooted at the given block.
    pub fn subtree(&self, root: &BlockId) -> Vec<&BlockSnapshot> {
        let mut result = Vec::new();
        let mut stack = vec![root.clone()];

        while let Some(id) = stack.pop() {
            if let Some(block) = self.blocks.get(&id) {
                result.push(block);
                if let Some(children) = self.children.get(&id) {
                    // Push children in reverse to maintain order
                    for child in children.iter().rev() {
                        stack.push(child.clone());
                    }
                }
            }
        }

        result
    }

    /// Get the depth of a block (0 for roots).
    pub fn depth(&self, id: &BlockId) -> usize {
        let mut depth = 0;
        let mut current = self.blocks.get(id);

        while let Some(block) = current {
            if let Some(ref parent_id) = block.parent_id {
                depth += 1;
                current = self.blocks.get(parent_id);
            } else {
                break;
            }
        }

        depth
    }

    /// Get ancestors of a block (from immediate parent to root).
    pub fn ancestors(&self, id: &BlockId) -> Vec<&BlockSnapshot> {
        let mut result = Vec::new();
        let mut current = self.blocks.get(id);

        while let Some(block) = current {
            if let Some(ref parent_id) = block.parent_id {
                if let Some(parent) = self.blocks.get(parent_id) {
                    result.push(parent);
                    current = Some(parent);
                } else {
                    break;
                }
            } else {
                break;
            }
        }

        result
    }

    /// Check if the DAG is empty.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Get the total number of blocks.
    pub fn len(&self) -> usize {
        self.blocks.len()
    }
}

/// Depth-first iterator over DAG blocks.
struct DfsIterator<'a> {
    dag: &'a ConversationDAG,
    stack: Vec<(usize, BlockId)>,
}

impl<'a> DfsIterator<'a> {
    fn new(dag: &'a ConversationDAG) -> Self {
        // Push roots in reverse order to process first root first
        let stack: Vec<_> = dag.roots.iter().rev().map(|id| (0, id.clone())).collect();
        Self { dag, stack }
    }
}

impl<'a> Iterator for DfsIterator<'a> {
    type Item = (usize, &'a BlockSnapshot);

    fn next(&mut self) -> Option<Self::Item> {
        while let Some((depth, id)) = self.stack.pop() {
            if let Some(block) = self.dag.blocks.get(&id) {
                // Push children in reverse order
                if let Some(children) = self.dag.children.get(&id) {
                    for child in children.iter().rev() {
                        self.stack.push((depth + 1, child.clone()));
                    }
                }
                return Some((depth, block));
            }
        }
        None
    }
}

/// Breadth-first iterator over DAG blocks.
struct BfsIterator<'a> {
    dag: &'a ConversationDAG,
    queue: std::collections::VecDeque<(usize, BlockId)>,
}

impl<'a> BfsIterator<'a> {
    fn new(dag: &'a ConversationDAG) -> Self {
        let queue: std::collections::VecDeque<_> = dag.roots.iter().map(|id| (0, id.clone())).collect();
        Self { dag, queue }
    }
}

impl<'a> Iterator for BfsIterator<'a> {
    type Item = (usize, &'a BlockSnapshot);

    fn next(&mut self) -> Option<Self::Item> {
        while let Some((depth, id)) = self.queue.pop_front() {
            if let Some(block) = self.dag.blocks.get(&id) {
                // Queue children
                if let Some(children) = self.dag.children.get(&id) {
                    for child in children {
                        self.queue.push_back((depth + 1, child.clone()));
                    }
                }
                return Some((depth, block));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BlockKind, Role};

    #[test]
    fn test_dag_from_flat_document() {
        let mut doc = BlockDocument::new("doc-1", "alice");

        // Create flat structure
        let id1 = doc.insert_block(None, None, Role::User, BlockKind::Text, "First", "alice").unwrap();
        let id2 = doc.insert_block(None, Some(&id1), Role::Model, BlockKind::Text, "Second", "claude").unwrap();

        let dag = ConversationDAG::from_document(&doc);

        assert_eq!(dag.roots.len(), 2);
        assert!(dag.get(&id1).is_some());
        assert!(dag.get(&id2).is_some());
    }

    #[test]
    fn test_dag_with_parent_child() {
        let mut doc = BlockDocument::new("doc-1", "alice");

        // Create parent-child structure
        let parent = doc.insert_block(None, None, Role::User, BlockKind::Text, "Question", "alice").unwrap();
        let child1 = doc.insert_block(Some(&parent), Some(&parent), Role::Model, BlockKind::Thinking, "Thinking...", "claude").unwrap();
        let child2 = doc.insert_block(Some(&parent), Some(&child1), Role::Model, BlockKind::Text, "Answer", "claude").unwrap();

        let dag = ConversationDAG::from_document(&doc);

        assert_eq!(dag.roots.len(), 1);
        assert_eq!(dag.roots[0], parent);

        let children = dag.get_children(&parent);
        assert_eq!(children.len(), 2);
        assert!(children.contains(&child1));
        assert!(children.contains(&child2));

        assert_eq!(dag.depth(&parent), 0);
        assert_eq!(dag.depth(&child1), 1);
        assert_eq!(dag.depth(&child2), 1);
    }

    #[test]
    fn test_dfs_iteration() {
        let mut doc = BlockDocument::new("doc-1", "alice");

        let root = doc.insert_block(None, None, Role::User, BlockKind::Text, "Root", "alice").unwrap();
        let child1 = doc.insert_block(Some(&root), Some(&root), Role::Model, BlockKind::Text, "Child1", "claude").unwrap();
        let grandchild = doc.insert_block(Some(&child1), Some(&child1), Role::Model, BlockKind::Text, "Grandchild", "claude").unwrap();

        let dag = ConversationDAG::from_document(&doc);

        let dfs: Vec<_> = dag.iter_dfs().collect();
        assert_eq!(dfs.len(), 3);
        assert_eq!(dfs[0].0, 0); // Root depth
        assert_eq!(dfs[0].1.id, root);
        assert_eq!(dfs[1].0, 1); // Child depth
        assert_eq!(dfs[1].1.id, child1);
        assert_eq!(dfs[2].0, 2); // Grandchild depth
        assert_eq!(dfs[2].1.id, grandchild);
    }

    #[test]
    fn test_subtree() {
        let mut doc = BlockDocument::new("doc-1", "alice");

        let root = doc.insert_block(None, None, Role::User, BlockKind::Text, "Root", "alice").unwrap();
        let child = doc.insert_block(Some(&root), Some(&root), Role::Model, BlockKind::Text, "Child", "claude").unwrap();
        let _other_root = doc.insert_block(None, Some(&child), Role::User, BlockKind::Text, "Other", "alice").unwrap();

        let dag = ConversationDAG::from_document(&doc);

        let subtree = dag.subtree(&root);
        assert_eq!(subtree.len(), 2);
        assert!(subtree.iter().any(|b| b.id == root));
        assert!(subtree.iter().any(|b| b.id == child));
    }
}
