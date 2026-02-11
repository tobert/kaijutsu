//! Tiling Window Manager — the core layout data structure.
//!
//! Everything on screen is a **Pane** with a `PaneContent`. The `TilingTree`
//! resource owns the layout tree and the reconciler turns it into Bevy entities.
//!
//! ## Three Screen Primitives
//!
//! | Primitive | PaneContent variants               | Examples                     |
//! |-----------|------------------------------------|------------------------------|
//! | Views     | Conversation, Dashboard, Editor    | DagView, code viewer         |
//! | Inputs    | Compose, Shell                     | Chat prompt, kaish input     |
//! | Widgets   | Title, Mode, Connection, Contexts… | Status bar items             |

use bevy::prelude::*;

// ============================================================================
// PANE IDENTITY
// ============================================================================

/// Unique, stable identifier for a pane in the tiling tree.
///
/// PaneIds persist across frames so the reconciler can diff by identity,
/// not by tree position. Monotonically increasing from `TilingTree.next_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Component, Reflect)]
#[reflect(Component)]
pub struct PaneId(pub u64);

impl std::fmt::Display for PaneId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Pane({})", self.0)
    }
}

// ============================================================================
// PANE CONTENT — What a pane displays
// ============================================================================

/// Writing direction for compose panes (cosmic-text supports both).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
pub enum WritingDirection {
    #[default]
    Horizontal,
    /// Vertical right-to-left (日本語 tategaki)
    #[allow(dead_code)]
    VerticalRl,
}

/// What a pane displays. Every node in the tree has a content variant.
#[derive(Debug, Clone, PartialEq, Reflect)]
pub enum PaneContent {
    // ── Views ──────────────────────────────────────────────────────────
    /// A CRDT conversation view bound to a document.
    Conversation { document_id: String },
    /// The kernel/context/seat dashboard.
    Dashboard,
    /// A text file editor (future).
    #[allow(dead_code)]
    Editor { path: String },

    // ── Inputs ─────────────────────────────────────────────────────────
    /// Compose input linked to a conversation pane.
    Compose {
        /// Which pane this input submits to.
        target_pane: PaneId,
        writing_direction: WritingDirection,
    },
    /// Kaish shell input (future).
    #[allow(dead_code)]
    Shell,

    // ── Widgets ────────────────────────────────────────────────────────
    /// Application title (e.g. "会術 Kaijutsu").
    Title,
    /// Vim-style mode indicator (NORMAL / CHAT / SHELL / VISUAL).
    Mode,
    /// Connection status (reactive to RpcConnectionState).
    Connection,
    /// Drift context badge strip.
    Contexts,
    /// Context-sensitive key hints.
    Hints,
    /// Static or templated text.
    #[allow(dead_code)]
    Text { template: String },
    /// Flexible spacer — grows to fill remaining space.
    Spacer,
}

#[allow(dead_code)]
impl PaneContent {
    /// Whether this content type is a widget (lives in a dock, auto-sized).
    pub fn is_widget(&self) -> bool {
        matches!(
            self,
            PaneContent::Title
                | PaneContent::Mode
                | PaneContent::Connection
                | PaneContent::Contexts
                | PaneContent::Hints
                | PaneContent::Text { .. }
                | PaneContent::Spacer
        )
    }
}

// ============================================================================
// SPLIT DIRECTION
// ============================================================================

/// Direction for Split nodes (same semantics as flex-direction).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
pub enum SplitDirection {
    /// Children laid out left to right.
    Row,
    /// Children laid out top to bottom.
    #[default]
    Column,
}

// ============================================================================
// DOCK EDGE
// ============================================================================

/// Which edge a Dock node attaches to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Reflect)]
pub enum Edge {
    North,
    #[default]
    South,
    #[allow(dead_code)]
    East,
    #[allow(dead_code)]
    West,
}

// ============================================================================
// TILE NODE — The recursive tree
// ============================================================================

/// A node in the tiling tree. The full UI layout is a single `TileNode`.
#[derive(Debug, Clone, Reflect)]
pub enum TileNode {
    /// A container that splits space among children.
    Split {
        id: PaneId,
        direction: SplitDirection,
        children: Vec<TileNode>,
        /// Flex ratios for children (must be same length as children).
        ratios: Vec<f32>,
    },
    /// A leaf pane with content.
    Leaf {
        id: PaneId,
        content: PaneContent,
    },
    /// A dock attached to a screen edge, children auto-sized.
    Dock {
        id: PaneId,
        edge: Edge,
        children: Vec<TileNode>,
    },
}

#[allow(dead_code)]
impl TileNode {
    /// Get the PaneId of this node.
    pub fn id(&self) -> PaneId {
        match self {
            TileNode::Split { id, .. } => *id,
            TileNode::Leaf { id, .. } => *id,
            TileNode::Dock { id, .. } => *id,
        }
    }

    /// Recursively collect all PaneIds in this subtree.
    pub fn collect_ids(&self, out: &mut Vec<PaneId>) {
        out.push(self.id());
        match self {
            TileNode::Split { children, .. } | TileNode::Dock { children, .. } => {
                for child in children {
                    child.collect_ids(out);
                }
            }
            TileNode::Leaf { .. } => {}
        }
    }

    /// Find a node by PaneId (immutable).
    pub fn find(&self, target: PaneId) -> Option<&TileNode> {
        if self.id() == target {
            return Some(self);
        }
        match self {
            TileNode::Split { children, .. } | TileNode::Dock { children, .. } => {
                for child in children {
                    if let Some(found) = child.find(target) {
                        return Some(found);
                    }
                }
                None
            }
            TileNode::Leaf { .. } => None,
        }
    }

    /// Find a node by PaneId (mutable).
    pub fn find_mut(&mut self, target: PaneId) -> Option<&mut TileNode> {
        if self.id() == target {
            return Some(self);
        }
        match self {
            TileNode::Split { children, .. } | TileNode::Dock { children, .. } => {
                for child in children {
                    if let Some(found) = child.find_mut(target) {
                        return Some(found);
                    }
                }
                None
            }
            TileNode::Leaf { .. } => None,
        }
    }

    /// Find the parent of a node with the given PaneId.
    /// Returns (parent_node, index_of_child).
    pub fn find_parent(&self, target: PaneId) -> Option<(PaneId, usize)> {
        match self {
            TileNode::Split { id, children, .. } | TileNode::Dock { id, children, .. } => {
                for (i, child) in children.iter().enumerate() {
                    if child.id() == target {
                        return Some((*id, i));
                    }
                    if let Some(found) = child.find_parent(target) {
                        return Some(found);
                    }
                }
                None
            }
            TileNode::Leaf { .. } => None,
        }
    }

    /// Get all leaf panes with `PaneContent::Conversation`.
    pub fn conversation_panes(&self) -> Vec<(PaneId, &str)> {
        let mut out = Vec::new();
        self.collect_conversations(&mut out);
        out
    }

    fn collect_conversations<'a>(&'a self, out: &mut Vec<(PaneId, &'a str)>) {
        match self {
            TileNode::Leaf {
                id,
                content: PaneContent::Conversation { document_id },
            } => {
                out.push((*id, document_id.as_str()));
            }
            TileNode::Split { children, .. } | TileNode::Dock { children, .. } => {
                for child in children {
                    child.collect_conversations(out);
                }
            }
            TileNode::Leaf { .. } => {}
        }
    }
}

// ============================================================================
// TILING TREE — The top-level resource
// ============================================================================

/// The tiling tree resource — owns the entire UI layout.
///
/// The reconciler reads this every frame and diffs against the Bevy entity tree.
/// Tree operations (split, close, swap, etc.) mutate this resource and the
/// reconciler handles the entity-level changes.
#[derive(Resource, Reflect)]
pub struct TilingTree {
    /// Root node of the layout tree.
    pub root: TileNode,
    /// Next PaneId to assign (monotonically increasing).
    next_id: u64,
    /// Currently focused pane.
    pub focused: PaneId,
    /// Previously focused pane (for Ctrl-^ toggle).
    pub previous_focused: Option<PaneId>,
    /// Generation counter — bumped on every mutation for change detection.
    pub generation: u64,
}

#[allow(dead_code)]
impl TilingTree {
    /// Allocate a new unique PaneId.
    pub fn next_pane_id(&mut self) -> PaneId {
        let id = PaneId(self.next_id);
        self.next_id += 1;
        id
    }

    /// Create the default layout: North dock, single conversation, South dock.
    pub fn default_layout() -> Self {
        let mut next_id = 0u64;
        let mut next = || {
            let id = PaneId(next_id);
            next_id += 1;
            id
        };

        // North dock widgets
        let north_dock = TileNode::Dock {
            id: next(),
            edge: Edge::North,
            children: vec![
                TileNode::Leaf {
                    id: next(),
                    content: PaneContent::Title,
                },
                TileNode::Leaf {
                    id: next(),
                    content: PaneContent::Spacer,
                },
                TileNode::Leaf {
                    id: next(),
                    content: PaneContent::Connection,
                },
            ],
        };

        // Content area — starts as a single conversation
        let conv_id = next();
        let compose_id = next();
        let content_split = TileNode::Split {
            id: next(),
            direction: SplitDirection::Column,
            children: vec![
                TileNode::Leaf {
                    id: conv_id,
                    content: PaneContent::Conversation {
                        document_id: String::new(), // Set when conversation joins
                    },
                },
                TileNode::Leaf {
                    id: compose_id,
                    content: PaneContent::Compose {
                        target_pane: conv_id,
                        writing_direction: WritingDirection::Horizontal,
                    },
                },
            ],
            ratios: vec![1.0, 0.0], // Conversation grows, compose auto-sizes
        };

        // South dock widgets
        let south_dock = TileNode::Dock {
            id: next(),
            edge: Edge::South,
            children: vec![
                TileNode::Leaf {
                    id: next(),
                    content: PaneContent::Mode,
                },
                TileNode::Leaf {
                    id: next(),
                    content: PaneContent::Spacer,
                },
                TileNode::Leaf {
                    id: next(),
                    content: PaneContent::Contexts,
                },
                TileNode::Leaf {
                    id: next(),
                    content: PaneContent::Spacer,
                },
                TileNode::Leaf {
                    id: next(),
                    content: PaneContent::Hints,
                },
            ],
        };

        // Root: Column with [NorthDock, ContentSplit, SouthDock]
        let root_id = next();
        let root = TileNode::Split {
            id: root_id,
            direction: SplitDirection::Column,
            children: vec![north_dock, content_split, south_dock],
            ratios: vec![0.0, 1.0, 0.0], // Docks auto-size, content grows
        };

        Self {
            root,
            next_id,
            focused: conv_id, // Start focused on conversation
            previous_focused: None,
            generation: 0,
        }
    }

    /// Bump generation counter (signals reconciler to re-diff).
    fn bump(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    // ════════════════════════════════════════════════════════════════════
    // TREE OPERATIONS
    // ════════════════════════════════════════════════════════════════════

    /// Split a pane, inserting a new pane beside it.
    ///
    /// Phase 2 — requires careful borrow-checker-safe tree mutation.
    #[allow(dead_code)]
    pub fn split(
        &mut self,
        _target: PaneId,
        _direction: SplitDirection,
        _content: PaneContent,
    ) -> Option<PaneId> {
        // Phase 2 implementation
        None
    }

    /// Close a pane, removing it from the tree.
    ///
    /// Phase 2 — requires careful borrow-checker-safe tree mutation.
    #[allow(dead_code)]
    pub fn close(&mut self, _target: PaneId) -> bool {
        // Phase 2 implementation
        false
    }

    /// Swap two panes in the tree.
    #[allow(dead_code)]
    pub fn swap(&mut self, _a: PaneId, _b: PaneId) -> bool {
        // Phase 2 implementation
        false
    }

    /// Move focus in a direction (spatial neighbor finding).
    #[allow(dead_code)]
    pub fn focus_direction(&mut self, _direction: SplitDirection) -> bool {
        // Phase 2 implementation
        false
    }

    /// Resize a split ratio.
    #[allow(dead_code)]
    pub fn resize(&mut self, _target: PaneId, _delta: f32) -> bool {
        // Phase 2 implementation
        false
    }

    /// Focus a specific pane.
    pub fn focus(&mut self, target: PaneId) {
        if self.focused != target {
            self.previous_focused = Some(self.focused);
            self.focused = target;
            self.bump();
        }
    }

    /// Toggle focus between current and previous pane (Ctrl-^).
    #[allow(dead_code)]
    pub fn toggle_focus(&mut self) {
        if let Some(prev) = self.previous_focused {
            let current = self.focused;
            self.focused = prev;
            self.previous_focused = Some(current);
            self.bump();
        }
    }

    // ════════════════════════════════════════════════════════════════════
    // QUERIES
    // ════════════════════════════════════════════════════════════════════

    /// Find the first conversation pane (by tree order).
    pub fn first_conversation_pane(&self) -> Option<PaneId> {
        self.root
            .conversation_panes()
            .first()
            .map(|(id, _)| *id)
    }

    /// Update the document_id for a conversation pane.
    pub fn set_conversation_document(&mut self, pane: PaneId, document_id: &str) {
        if let Some(node) = self.root.find_mut(pane) {
            if let TileNode::Leaf {
                content: PaneContent::Conversation { document_id: doc_id },
                ..
            } = node
            {
                *doc_id = document_id.to_string();
                self.bump();
            }
        }
    }

    /// Get the document_id for the focused pane (if it's a Conversation).
    pub fn focused_conversation_document(&self) -> Option<&str> {
        let node = self.root.find(self.focused)?;
        match node {
            TileNode::Leaf {
                content: PaneContent::Conversation { document_id },
                ..
            } => {
                if document_id.is_empty() {
                    None
                } else {
                    Some(document_id.as_str())
                }
            }
            _ => None,
        }
    }
}

// ============================================================================
// PANE MARKER — Links Bevy entities to PaneIds
// ============================================================================

/// Component linking a Bevy entity to a pane in the TilingTree.
///
/// Spawned by the reconciler on every entity that corresponds to a TileNode.
/// Systems query for this to find "which pane entity am I?"
#[derive(Component, Debug, Clone, Reflect)]
#[reflect(Component)]
pub struct PaneMarker {
    pub pane_id: PaneId,
    pub content: PaneContent,
}

/// Marker for the focused pane entity.
///
/// The reconciler adds/removes this marker based on `TilingTree.focused`.
/// Systems use `Query<..., With<PaneFocus>>` to find the focused pane.
#[derive(Component, Debug, Clone, Copy, Reflect)]
#[reflect(Component)]
pub struct PaneFocus;

// ============================================================================
// PLUGIN
// ============================================================================

/// Plugin for the tiling window manager system.
pub struct TilingPlugin;

impl Plugin for TilingPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(TilingTree::default_layout())
            .register_type::<PaneId>()
            .register_type::<PaneContent>()
            .register_type::<SplitDirection>()
            .register_type::<Edge>()
            .register_type::<WritingDirection>()
            .register_type::<PaneMarker>()
            .register_type::<PaneFocus>();
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_layout_has_conversation() {
        let tree = TilingTree::default_layout();
        let convs = tree.root.conversation_panes();
        assert_eq!(convs.len(), 1, "Default layout should have one conversation pane");
    }

    #[test]
    fn find_by_id() {
        let tree = TilingTree::default_layout();
        let found = tree.root.find(tree.focused);
        assert!(found.is_some(), "Should find the focused pane");
    }

    #[test]
    fn collect_all_ids() {
        let tree = TilingTree::default_layout();
        let mut ids = Vec::new();
        tree.root.collect_ids(&mut ids);
        assert!(ids.len() >= 10, "Default layout should have many nodes");
        // All IDs should be unique
        let deduped: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(ids.len(), deduped.len(), "All PaneIds should be unique");
    }

    #[test]
    fn set_conversation_document() {
        let mut tree = TilingTree::default_layout();
        let conv_id = tree.first_conversation_pane().unwrap();
        tree.set_conversation_document(conv_id, "doc_123");
        assert_eq!(tree.focused_conversation_document(), Some("doc_123"));
    }
}
