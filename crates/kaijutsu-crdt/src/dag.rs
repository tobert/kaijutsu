//! Computed DAG index from CRDT data.
//!
//! The ConversationDAG provides efficient tree traversal operations
//! computed from the flat block list in BlockDocument.

use std::collections::{HashMap, HashSet};

use crate::{BlockDocument, BlockId, BlockSnapshot, MAX_DAG_DEPTH};

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
            if let Some(parent_id) = snap.parent_id {
                children.entry(parent_id).or_default().push(snap.id);
            } else {
                roots.push(snap.id);
            }
            blocks.insert(snap.id, snap);
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
    ///
    /// Circuit-breaks at `MAX_DAG_DEPTH` to prevent runaway traversal
    /// in the presence of cycles or extremely deep trees.
    pub fn subtree(&self, root: &BlockId) -> Vec<&BlockSnapshot> {
        let mut result = Vec::new();
        let mut stack = vec![*root];
        let mut visited = HashSet::new();

        while let Some(id) = stack.pop() {
            if !visited.insert(id) {
                continue; // cycle detected — skip
            }
            if visited.len() > MAX_DAG_DEPTH {
                tracing::warn!("subtree traversal hit MAX_DAG_DEPTH ({MAX_DAG_DEPTH}), truncating");
                break;
            }
            if let Some(block) = self.blocks.get(&id) {
                result.push(block);
                if let Some(children) = self.children.get(&id) {
                    // Push children in reverse to maintain order
                    for child in children.iter().rev() {
                        stack.push(*child);
                    }
                }
            }
        }

        result
    }

    /// Get the depth of a block (0 for roots).
    ///
    /// Circuit-breaks at `MAX_DAG_DEPTH`.
    pub fn depth(&self, id: &BlockId) -> usize {
        let mut depth = 0;
        let mut current = self.blocks.get(id);

        while let Some(block) = current {
            if depth >= MAX_DAG_DEPTH {
                tracing::warn!("depth() hit MAX_DAG_DEPTH ({MAX_DAG_DEPTH}), returning capped depth");
                break;
            }
            if let Some(parent_id) = block.parent_id {
                depth += 1;
                current = self.blocks.get(&parent_id);
            } else {
                break;
            }
        }

        depth
    }

    /// Get ancestors of a block (from immediate parent to root).
    ///
    /// Circuit-breaks at `MAX_DAG_DEPTH`.
    pub fn ancestors(&self, id: &BlockId) -> Vec<&BlockSnapshot> {
        let mut result = Vec::new();
        let mut current = self.blocks.get(id);

        while let Some(block) = current {
            if result.len() >= MAX_DAG_DEPTH {
                tracing::warn!("ancestors() hit MAX_DAG_DEPTH ({MAX_DAG_DEPTH}), truncating");
                break;
            }
            if let Some(parent_id) = block.parent_id {
                if let Some(parent) = self.blocks.get(&parent_id) {
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
///
/// Tracks visited nodes to protect against cycles. Circuit-breaks at
/// `MAX_DAG_DEPTH` total visited nodes.
struct DfsIterator<'a> {
    dag: &'a ConversationDAG,
    stack: Vec<(usize, BlockId)>,
    visited: HashSet<BlockId>,
}

impl<'a> DfsIterator<'a> {
    fn new(dag: &'a ConversationDAG) -> Self {
        // Push roots in reverse order to process first root first
        let stack: Vec<_> = dag.roots.iter().rev().map(|id| (0, *id)).collect();
        Self { dag, stack, visited: HashSet::new() }
    }
}

impl<'a> Iterator for DfsIterator<'a> {
    type Item = (usize, &'a BlockSnapshot);

    fn next(&mut self) -> Option<Self::Item> {
        while let Some((depth, id)) = self.stack.pop() {
            if !self.visited.insert(id) {
                continue; // already visited (cycle)
            }
            if self.visited.len() > MAX_DAG_DEPTH {
                tracing::warn!("DFS iterator hit MAX_DAG_DEPTH ({MAX_DAG_DEPTH}), stopping");
                return None;
            }
            if let Some(block) = self.dag.blocks.get(&id) {
                // Push children in reverse order
                if let Some(children) = self.dag.children.get(&id) {
                    for child in children.iter().rev() {
                        self.stack.push((depth + 1, *child));
                    }
                }
                return Some((depth, block));
            }
        }
        None
    }
}

/// Breadth-first iterator over DAG blocks.
///
/// Tracks visited nodes to protect against cycles. Circuit-breaks at
/// `MAX_DAG_DEPTH` total visited nodes.
struct BfsIterator<'a> {
    dag: &'a ConversationDAG,
    queue: std::collections::VecDeque<(usize, BlockId)>,
    visited: HashSet<BlockId>,
}

impl<'a> BfsIterator<'a> {
    fn new(dag: &'a ConversationDAG) -> Self {
        let queue: std::collections::VecDeque<_> = dag.roots.iter().map(|id| (0, *id)).collect();
        Self { dag, queue, visited: HashSet::new() }
    }
}

impl<'a> Iterator for BfsIterator<'a> {
    type Item = (usize, &'a BlockSnapshot);

    fn next(&mut self) -> Option<Self::Item> {
        while let Some((depth, id)) = self.queue.pop_front() {
            if !self.visited.insert(id) {
                continue; // already visited (cycle)
            }
            if self.visited.len() > MAX_DAG_DEPTH {
                tracing::warn!("BFS iterator hit MAX_DAG_DEPTH ({MAX_DAG_DEPTH}), stopping");
                return None;
            }
            if let Some(block) = self.dag.blocks.get(&id) {
                // Queue children
                if let Some(children) = self.dag.children.get(&id) {
                    for child in children {
                        self.queue.push_back((depth + 1, *child));
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
    use crate::{BlockKind, ContextId, PrincipalId, Role};

    fn test_doc() -> BlockDocument {
        BlockDocument::new(ContextId::new(), PrincipalId::new())
    }

    #[test]
    fn test_dag_from_flat_document() {
        let mut doc = test_doc();

        let id1 = doc.insert_block(None, None, Role::User, BlockKind::Text, "First").unwrap();
        let id2 = doc.insert_block(None, Some(&id1), Role::Model, BlockKind::Text, "Second").unwrap();

        let dag = ConversationDAG::from_document(&doc);

        assert_eq!(dag.roots.len(), 2);
        assert!(dag.get(&id1).is_some());
        assert!(dag.get(&id2).is_some());
    }

    #[test]
    fn test_dag_with_parent_child() {
        let mut doc = test_doc();

        let parent = doc.insert_block(None, None, Role::User, BlockKind::Text, "Question").unwrap();
        let child1 = doc.insert_block(Some(&parent), Some(&parent), Role::Model, BlockKind::Thinking, "Thinking...").unwrap();
        let child2 = doc.insert_block(Some(&parent), Some(&child1), Role::Model, BlockKind::Text, "Answer").unwrap();

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
        let mut doc = test_doc();

        let root = doc.insert_block(None, None, Role::User, BlockKind::Text, "Root").unwrap();
        let child1 = doc.insert_block(Some(&root), Some(&root), Role::Model, BlockKind::Text, "Child1").unwrap();
        let grandchild = doc.insert_block(Some(&child1), Some(&child1), Role::Model, BlockKind::Text, "Grandchild").unwrap();

        let dag = ConversationDAG::from_document(&doc);

        let dfs: Vec<_> = dag.iter_dfs().collect();
        assert_eq!(dfs.len(), 3);
        assert_eq!(dfs[0].0, 0);
        assert_eq!(dfs[0].1.id, root);
        assert_eq!(dfs[1].0, 1);
        assert_eq!(dfs[1].1.id, child1);
        assert_eq!(dfs[2].0, 2);
        assert_eq!(dfs[2].1.id, grandchild);
    }

    #[test]
    fn test_dag_cycle_terminates() {
        // Manually construct a DAG with a cycle to verify circuit breakers
        let ctx = ContextId::new();
        let agent = PrincipalId::new();

        let id_a = BlockId::new(ctx, agent, 0);
        let id_b = BlockId::new(ctx, agent, 1);

        // A → B → A (cycle via parent_id)
        let snap_a = BlockSnapshot {
            id: id_a,
            parent_id: Some(id_b), // cycle!
            role: Role::User,
            status: crate::Status::Done,
            kind: BlockKind::Text,
            content: "A".to_string(),
            collapsed: false,
            compacted: false,
            created_at: 0,
            tool_kind: None,
            tool_name: None,
            tool_input: None,
            tool_call_id: None,
            exit_code: None,
            is_error: false,
            display_hint: None,
            source_context: None,
            source_model: None,
            drift_kind: None,
        };
        let snap_b = BlockSnapshot {
            id: id_b,
            parent_id: Some(id_a), // cycle!
            role: Role::Model,
            status: crate::Status::Done,
            kind: BlockKind::Text,
            content: "B".to_string(),
            collapsed: false,
            compacted: false,
            created_at: 0,
            tool_kind: None,
            tool_name: None,
            tool_input: None,
            tool_call_id: None,
            exit_code: None,
            is_error: false,
            display_hint: None,
            source_context: None,
            source_model: None,
            drift_kind: None,
        };

        // Build DAG manually (from_document would not create cycles)
        let mut blocks = HashMap::new();
        let mut children: HashMap<BlockId, Vec<BlockId>> = HashMap::new();
        blocks.insert(id_a, snap_a);
        blocks.insert(id_b, snap_b);
        children.entry(id_b).or_default().push(id_a);
        children.entry(id_a).or_default().push(id_b);

        let dag = ConversationDAG {
            roots: vec![id_a],
            children,
            blocks,
        };

        // These should all terminate, not loop forever
        let depth = dag.depth(&id_a);
        assert!(depth <= MAX_DAG_DEPTH, "depth should be bounded");

        let ancestors = dag.ancestors(&id_a);
        assert!(ancestors.len() <= MAX_DAG_DEPTH, "ancestors should be bounded");

        let subtree = dag.subtree(&id_a);
        assert!(subtree.len() <= MAX_DAG_DEPTH + 1, "subtree should be bounded");

        let dfs: Vec<_> = dag.iter_dfs().collect();
        assert!(dfs.len() <= MAX_DAG_DEPTH + 1, "DFS should be bounded");

        let bfs: Vec<_> = dag.iter_bfs().collect();
        assert!(bfs.len() <= MAX_DAG_DEPTH + 1, "BFS should be bounded");
    }

    #[test]
    fn test_subtree() {
        let mut doc = test_doc();

        let root = doc.insert_block(None, None, Role::User, BlockKind::Text, "Root").unwrap();
        let child = doc.insert_block(Some(&root), Some(&root), Role::Model, BlockKind::Text, "Child").unwrap();
        let _other_root = doc.insert_block(None, Some(&child), Role::User, BlockKind::Text, "Other").unwrap();

        let dag = ConversationDAG::from_document(&doc);

        let subtree = dag.subtree(&root);
        assert_eq!(subtree.len(), 2);
        assert!(subtree.iter().any(|b| b.id == root));
        assert!(subtree.iter().any(|b| b.id == child));
    }
}
