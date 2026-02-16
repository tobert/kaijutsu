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
    #[allow(dead_code)] // Phase 4: vertical nihongo compose pane
    VerticalRl,
}

/// What a pane displays. Every node in the tree has a content variant.
#[derive(Debug, Clone, PartialEq, Reflect)]
pub enum PaneContent {
    // ── Views ──────────────────────────────────────────────────────────
    /// A CRDT conversation view bound to a document.
    Conversation { document_id: String },
    // Dashboard variant removed — app starts directly in conversation view.
    /// A text file editor.
    #[allow(dead_code)] // Phase 4: tab containers for stacking views
    Editor { path: String },

    // ── Inputs ─────────────────────────────────────────────────────────
    /// Compose input linked to a conversation pane.
    Compose {
        /// Which pane this input submits to.
        target_pane: PaneId,
        writing_direction: WritingDirection,
    },
    /// Kaish shell input.
    #[allow(dead_code)] // Phase 4: shell pane content type
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
    Text { template: String },
    /// Flexible spacer — grows to fill remaining space.
    Spacer,
}

impl PaneContent {
    /// Whether this content type is a conversation view.
    #[allow(dead_code)] // used in tests + future per-pane dispatch
    pub fn is_conversation(&self) -> bool {
        matches!(self, PaneContent::Conversation { .. })
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

/// Spatial direction for focus navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusDirection {
    Left,
    Right,
    Up,
    Down,
}

impl FocusDirection {
    /// Which split direction this focus movement aligns with.
    fn split_axis(self) -> SplitDirection {
        match self {
            FocusDirection::Left | FocusDirection::Right => SplitDirection::Row,
            FocusDirection::Up | FocusDirection::Down => SplitDirection::Column,
        }
    }

    /// Whether this direction is forward (+1) or backward (-1) along its axis.
    fn is_forward(self) -> bool {
        matches!(self, FocusDirection::Right | FocusDirection::Down)
    }
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
    East,
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
    #[allow(dead_code)] // used in tests
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

    /// Check if this subtree contains a pane with the given ID.
    #[allow(dead_code)] // used in tests
    pub fn contains_pane(&self, target: PaneId) -> bool {
        if self.id() == target {
            return true;
        }
        match self {
            TileNode::Split { children, .. } | TileNode::Dock { children, .. } => {
                children.iter().any(|c| c.contains_pane(target))
            }
            TileNode::Leaf { .. } => false,
        }
    }

    /// Find the first conversation PaneId in this subtree (depth-first).
    pub fn first_conversation(&self) -> Option<PaneId> {
        match self {
            TileNode::Leaf {
                id,
                content: PaneContent::Conversation { .. },
            } => Some(*id),
            TileNode::Split { children, .. } | TileNode::Dock { children, .. } => {
                children.iter().find_map(|c| c.first_conversation())
            }
            TileNode::Leaf { .. } => None,
        }
    }

    /// Find the last conversation PaneId in this subtree (depth-first, reverse).
    pub fn last_conversation(&self) -> Option<PaneId> {
        match self {
            TileNode::Leaf {
                id,
                content: PaneContent::Conversation { .. },
            } => Some(*id),
            TileNode::Split { children, .. } | TileNode::Dock { children, .. } => {
                children.iter().rev().find_map(|c| c.last_conversation())
            }
            TileNode::Leaf { .. } => None,
        }
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
///
/// ## Two Generation Counters
///
/// - `structural_gen` — bumped on split/close (entity tree changes).
///   The reconciler rebuilds entities only when this changes.
/// - `visual_gen` — bumped on focus/resize (style-only changes).
///   The focus/visual system updates borders + flex weights without rebuilding.
#[derive(Resource, Reflect)]
#[reflect(Resource)]
pub struct TilingTree {
    /// Root node of the layout tree.
    pub root: TileNode,
    /// Next PaneId to assign (monotonically increasing).
    next_id: u64,
    /// Currently focused pane.
    pub focused: PaneId,
    /// Previously focused pane (for Ctrl-^ toggle).
    pub previous_focused: Option<PaneId>,
    /// Structural generation — bumped on split/close (entity tree changes).
    pub structural_gen: u64,
    /// Visual generation — bumped on focus/resize (style-only changes).
    pub visual_gen: u64,
}

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
            structural_gen: 0,
            visual_gen: 0,
        }
    }

    /// Bump structural generation (triggers entity rebuild in reconciler).
    fn bump_structural(&mut self) {
        self.structural_gen = self.structural_gen.wrapping_add(1);
        // Visual also bumps — structural changes imply visual changes
        self.visual_gen = self.visual_gen.wrapping_add(1);
    }

    /// Bump visual generation only (focus/resize — style updates, no rebuild).
    fn bump_visual(&mut self) {
        self.visual_gen = self.visual_gen.wrapping_add(1);
    }

    // ════════════════════════════════════════════════════════════════════
    // TREE OPERATIONS
    // ════════════════════════════════════════════════════════════════════

    /// Split a conversation pane, inserting a new conversation beside it.
    ///
    /// Finds the conversation group (Column Split containing the target
    /// conversation + its compose), then inserts a new conversation group
    /// as a sibling. If the parent's direction matches, the new group is
    /// inserted inline; otherwise a wrapper Split is created.
    ///
    /// Returns the PaneId of the new conversation pane.
    pub fn split(
        &mut self,
        target: PaneId,
        direction: SplitDirection,
    ) -> Option<PaneId> {
        // Verify target is a conversation pane
        if let Some(TileNode::Leaf { content, .. }) = self.root.find(target) {
            if !matches!(content, PaneContent::Conversation { .. }) {
                return None;
            }
        } else {
            return None;
        }

        // Find the conversation group: the Split(Column) parent of the target
        let (conv_group_id, _conv_idx) = self.root.find_parent(target)?;

        // Find the grandparent that contains the conversation group
        let (grandparent_id, group_idx) = self.root.find_parent(conv_group_id)?;

        // Allocate all IDs upfront (before any mutable tree borrows)
        let new_conv_id = self.next_pane_id();
        let new_compose_id = self.next_pane_id();
        let new_group_id = self.next_pane_id();
        let wrapper_id = self.next_pane_id();

        let new_group = TileNode::Split {
            id: new_group_id,
            direction: SplitDirection::Column,
            children: vec![
                TileNode::Leaf {
                    id: new_conv_id,
                    content: PaneContent::Conversation {
                        document_id: String::new(),
                    },
                },
                TileNode::Leaf {
                    id: new_compose_id,
                    content: PaneContent::Compose {
                        target_pane: new_conv_id,
                        writing_direction: WritingDirection::Horizontal,
                    },
                },
            ],
            ratios: vec![1.0, 0.0],
        };

        // Now mutate the grandparent
        let grandparent = self.root.find_mut(grandparent_id)?;
        if let TileNode::Split {
            direction: gp_dir,
            children,
            ratios,
            ..
        } = grandparent
        {
            if *gp_dir == direction {
                // Same direction: insert new group after the conversation group.
                // Halve the target's ratio — preserves other panes' custom sizing.
                let target_ratio = ratios.get(group_idx).copied().unwrap_or(0.5);
                let half = target_ratio / 2.0;
                if group_idx < ratios.len() {
                    ratios[group_idx] = half;
                }
                children.insert(group_idx + 1, new_group);
                ratios.insert(group_idx + 1, half);
            } else {
                // Different direction: wrap conv_group + new_group in a new Split
                let old_child = children.remove(group_idx);
                let old_ratio = if group_idx < ratios.len() {
                    ratios.remove(group_idx)
                } else {
                    1.0
                };
                let wrapper = TileNode::Split {
                    id: wrapper_id,
                    direction,
                    children: vec![old_child, new_group],
                    ratios: vec![0.5, 0.5],
                };
                children.insert(group_idx, wrapper);
                ratios.insert(group_idx, old_ratio);
            }
        } else {
            return None;
        }

        // Normalize ratios for the inline case (ensures content ratios sum to 1.0).
        // Safe to call unconditionally — the wrapper case already has [0.5, 0.5].
        if let Some(TileNode::Split { ratios, .. }) = self.root.find_mut(grandparent_id) {
            normalize_ratios(ratios);
        }

        self.bump_structural();
        Some(new_conv_id)
    }

    /// Close a conversation pane, removing its entire conversation group.
    ///
    /// If the group's parent becomes a single-child split, it collapses
    /// (replaces the split with the lone child). Focus moves to a neighbor.
    /// Returns false if the pane can't be closed (e.g. it's the last one).
    pub fn close(&mut self, target: PaneId) -> bool {
        // Don't close the last conversation
        let convs = self.root.conversation_panes();
        if convs.len() <= 1 {
            return false;
        }

        // Find the conversation group
        let Some((conv_group_id, _)) = self.root.find_parent(target) else {
            return false;
        };

        // Find the grandparent
        let Some((grandparent_id, group_idx)) = self.root.find_parent(conv_group_id) else {
            return false;
        };

        // Determine new focus before mutating
        let new_focus = self.find_close_target(target);

        // Remove the conversation group from grandparent
        let grandparent = match self.root.find_mut(grandparent_id) {
            Some(gp) => gp,
            None => return false,
        };

        if let TileNode::Split {
            children, ratios, ..
        } = grandparent
        {
            if group_idx < children.len() {
                children.remove(group_idx);
                if group_idx < ratios.len() {
                    ratios.remove(group_idx);
                }
                // Redistribute space proportionally to remaining panes
                normalize_ratios(ratios);
            } else {
                return false;
            }
        } else {
            return false;
        }

        // Collapse single-child splits
        self.collapse_single_child_splits();

        // Update focus inline (avoids double visual_gen bump from self.focus())
        if let Some(new_target) = new_focus
            && self.focused != new_target
        {
            self.previous_focused = Some(self.focused);
            self.focused = new_target;
        }
        self.bump_structural();
        true
    }

    /// Swap two conversation groups in the tree.
    ///
    /// PaneIds travel with content — the two groups exchange positions
    /// in the tree while keeping their identities. Works for siblings
    /// in the same split or groups in different splits.
    #[allow(dead_code)] // Phase 2: Mod+Shift+hjkl will wire this up
    pub fn swap(&mut self, a: PaneId, b: PaneId) -> bool {
        // Find both nodes' parents
        let Some((parent_a, idx_a)) = self.root.find_parent(a) else {
            return false;
        };
        let Some((parent_b, idx_b)) = self.root.find_parent(b) else {
            return false;
        };

        if parent_a == parent_b {
            // Siblings: swap in place within the same children vec
            let parent = match self.root.find_mut(parent_a) {
                Some(p) => p,
                None => return false,
            };
            if let TileNode::Split { children, ratios, .. } = parent {
                if idx_a < children.len() && idx_b < children.len() {
                    children.swap(idx_a, idx_b);
                    ratios.swap(idx_a, idx_b);
                } else {
                    return false;
                }
            } else {
                return false;
            }
        } else {
            // Different parents: clone both nodes, then replace via parents.
            // We must not use find_mut(b) after mutating a's slot, because
            // find_mut does DFS and could match the node we just placed.
            let node_a = match self.root.find(a) {
                Some(n) => n.clone(),
                None => return false,
            };
            let node_b = match self.root.find(b) {
                Some(n) => n.clone(),
                None => return false,
            };

            // Replace a's slot with b's clone via parent_a
            if let Some(TileNode::Split { children, .. }) = self.root.find_mut(parent_a) {
                if idx_a < children.len() {
                    children[idx_a] = node_b;
                } else {
                    return false;
                }
            } else {
                return false;
            }

            // Replace b's slot with a's clone via parent_b
            // Safe: parent_a != parent_b, so the first mutation didn't affect parent_b's children
            if let Some(TileNode::Split { children, .. }) = self.root.find_mut(parent_b) {
                if idx_b < children.len() {
                    children[idx_b] = node_a;
                } else {
                    return false;
                }
            } else {
                return false;
            }
        }

        self.bump_structural();
        true
    }

    /// Move focus in a spatial direction.
    ///
    /// Walks up from the focused pane to find a Split with the matching
    /// axis (Row for Left/Right, Column for Up/Down), then moves to the
    /// adjacent sibling's first/last conversation pane.
    pub fn focus_direction(&mut self, direction: FocusDirection) -> bool {
        let axis = direction.split_axis();
        let forward = direction.is_forward();

        // Walk ancestors from the focused pane upward
        let mut current = self.focused;
        loop {
            let Some((parent_id, child_idx)) = self.root.find_parent(current) else {
                break;
            };

            // Check if parent is a Split with the right direction
            if let Some(TileNode::Split {
                direction: split_dir,
                children,
                ..
            }) = self.root.find(parent_id)
            {
                if *split_dir == axis && children.len() > 1 {
                    let target_idx = if forward {
                        if child_idx + 1 < children.len() {
                            Some(child_idx + 1)
                        } else {
                            None // Already at end
                        }
                    } else if child_idx > 0 {
                        Some(child_idx - 1)
                    } else {
                        None // Already at start
                    };

                    if let Some(idx) = target_idx {
                        // Find a conversation in the target child
                        let target_conv = if forward {
                            children[idx].first_conversation()
                        } else {
                            children[idx].last_conversation()
                        };

                        if let Some(conv_id) = target_conv {
                            self.focus(conv_id);
                            return true;
                        }
                    }
                }
            }

            current = parent_id;
        }
        false
    }

    /// Resize the split containing the focused pane.
    ///
    /// `delta` is a fraction to shift (positive = grow focused pane,
    /// negative = shrink). Applies to the split that directly contains
    /// the focused pane's conversation group.
    pub fn resize(&mut self, target: PaneId, delta: f32) -> bool {
        // Need at least 2 conversations for resize to make sense
        if self.root.conversation_panes().len() < 2 {
            return false;
        }

        // Find the conversation group
        let Some((conv_group_id, _)) = self.root.find_parent(target) else {
            return false;
        };
        // Find the parent split
        let Some((parent_id, group_idx)) = self.root.find_parent(conv_group_id) else {
            return false;
        };

        let parent = match self.root.find_mut(parent_id) {
            Some(p) => p,
            None => return false,
        };

        if let TileNode::Split { ratios, .. } = parent {
            if ratios.len() < 2 {
                return false;
            }
            // Find a neighbor to take/give space
            let neighbor_idx = if group_idx + 1 < ratios.len() {
                group_idx + 1
            } else if group_idx > 0 {
                group_idx - 1
            } else {
                return false;
            };

            let min_ratio = 0.1;

            // Apply delta first, then normalize, then clamp, then re-normalize.
            // Clamping before normalization can distort other ratios.
            ratios[group_idx] += delta;
            ratios[neighbor_idx] -= delta;

            // First normalize pass
            let total: f32 = ratios.iter().sum();
            if total > 0.0 {
                for r in ratios.iter_mut() {
                    *r /= total;
                }
            }

            // Clamp all ratios
            let max_ratio = 1.0 - min_ratio * (ratios.len() - 1) as f32;
            for r in ratios.iter_mut() {
                *r = r.clamp(min_ratio, max_ratio);
            }

            // Re-normalize after clamping
            let total: f32 = ratios.iter().sum();
            if total > 0.0 {
                for r in ratios.iter_mut() {
                    *r /= total;
                }
            }
            self.bump_visual();
            true
        } else {
            false
        }
    }

    /// Focus a specific pane.
    pub fn focus(&mut self, target: PaneId) {
        if self.focused != target {
            self.previous_focused = Some(self.focused);
            self.focused = target;
            self.bump_visual();
        }
    }

    /// Toggle focus between current and previous pane (Ctrl-^).
    pub fn toggle_focus(&mut self) {
        if let Some(prev) = self.previous_focused {
            let current = self.focused;
            self.focused = prev;
            self.previous_focused = Some(current);
            self.bump_visual();
        }
    }

    // ════════════════════════════════════════════════════════════════════
    // HELPERS
    // ════════════════════════════════════════════════════════════════════

    /// Find the best conversation pane to focus after closing `target`.
    ///
    /// Prefers a spatially adjacent pane (sibling in the same parent split),
    /// trying the right neighbor first, then left. Falls back to the first
    /// conversation in the tree if no sibling is found.
    fn find_close_target(&self, target: PaneId) -> Option<PaneId> {
        // Try neighbor in same parent split
        if let Some((conv_group_id, _)) = self.root.find_parent(target)
            && let Some((parent_id, group_idx)) = self.root.find_parent(conv_group_id)
            && let Some(TileNode::Split { children, .. }) = self.root.find(parent_id)
        {
            // Try right neighbor, then left
            let candidates = [group_idx.wrapping_add(1), group_idx.wrapping_sub(1)];
            for idx in candidates {
                if let Some(conv) = children.get(idx).and_then(|c| c.first_conversation()) {
                    if conv != target {
                        return Some(conv);
                    }
                }
            }
        }
        // Fallback: any conversation that isn't the target
        self.root
            .conversation_panes()
            .iter()
            .find(|(id, _)| *id != target)
            .map(|(id, _)| *id)
    }

    /// Collapse Split nodes that have exactly one child.
    ///
    /// After closing a pane, a split might have only one child left.
    /// Replace the split with its lone child to keep the tree clean.
    fn collapse_single_child_splits(&mut self) {
        fn collapse(node: &mut TileNode) {
            // First recurse into children
            if let TileNode::Split { children, .. } = node {
                for child in children.iter_mut() {
                    collapse(child);
                }
                // After recursion, check if we should collapse
                if children.len() == 1 {
                    let child = children.remove(0);
                    *node = child;
                }
            }
        }
        collapse(&mut self.root);
    }

    // ════════════════════════════════════════════════════════════════════
    // QUERIES
    // ════════════════════════════════════════════════════════════════════

    /// Find the first conversation pane (by tree order).
    #[allow(dead_code)] // used in tests + Phase 3 preset loading
    pub fn first_conversation_pane(&self) -> Option<PaneId> {
        self.root
            .conversation_panes()
            .first()
            .map(|(id, _)| *id)
    }

    /// Update the document_id for a conversation pane.
    #[allow(dead_code)] // used in tests + context join wiring
    pub fn set_conversation_document(&mut self, pane: PaneId, document_id: &str) {
        if let Some(node) = self.root.find_mut(pane) {
            if let TileNode::Leaf {
                content: PaneContent::Conversation { document_id: doc_id },
                ..
            } = node
            {
                *doc_id = document_id.to_string();
                self.bump_visual();
            }
        }
    }

    /// Get the document_id for the focused pane (if it's a Conversation).
    #[allow(dead_code)] // used in tests + context join wiring
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
// FREE FUNCTIONS — Tree manipulation helpers
// ============================================================================

/// Normalize ratios so content children (ratio > 0) sum to 1.0.
///
/// Children with 0.0 ratios (auto-sized like compose blocks and docks)
/// are preserved. Remaining ratios are scaled proportionally, keeping
/// any user-customized sizing intact.
fn normalize_ratios(ratios: &mut [f32]) {
    let content_sum: f32 = ratios.iter().filter(|r| **r > 0.0).sum();
    if content_sum <= 0.0 {
        return;
    }
    let scale = 1.0 / content_sum;
    for r in ratios.iter_mut() {
        if *r > 0.0 {
            *r *= scale;
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

/// Per-pane saved state — attached to ConversationContainer entities.
///
/// When focus switches between panes, the outgoing pane's global state
/// (scroll, compose text) is saved here, and the incoming pane's state
/// is restored. This lets each pane remember where it was.
#[derive(Component, Debug, Clone, Default, Reflect)]
#[reflect(Component)]
pub struct PaneSavedState {
    /// Document this pane is viewing (empty = unassigned).
    pub document_id: String,
    /// Scroll offset (rendered position).
    pub scroll_offset: f32,
    /// Scroll target (smooth scroll destination).
    pub scroll_target: f32,
    /// Whether the pane was in follow (auto-scroll) mode.
    pub following: bool,
    /// Compose block text content.
    pub compose_text: String,
    /// Compose block cursor position.
    pub compose_cursor: usize,
}

// ============================================================================
// PLUGIN
// ============================================================================

/// Plugin for the tiling window manager system.
pub struct TilingPlugin;

impl Plugin for TilingPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(TilingTree::default_layout())
            .register_type::<TilingTree>()
            .register_type::<TileNode>()
            .register_type::<PaneId>()
            .register_type::<PaneContent>()
            .register_type::<SplitDirection>()
            .register_type::<Edge>()
            .register_type::<WritingDirection>()
            .register_type::<PaneMarker>()
            .register_type::<PaneFocus>()
            .register_type::<PaneSavedState>()
            // Tiling key handling in input::systems::handle_tiling
            ;
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

    // ── Split tests ─────────────────────────────────────────────────

    #[test]
    fn split_row_creates_two_conversations() {
        let mut tree = TilingTree::default_layout();
        let conv1 = tree.first_conversation_pane().unwrap();
        let gen_before = tree.structural_gen;

        let conv2 = tree.split(conv1, SplitDirection::Row);
        assert!(conv2.is_some(), "Split should return new conversation PaneId");

        let convs = tree.root.conversation_panes();
        assert_eq!(convs.len(), 2, "Should have two conversation panes after split");
        assert!(tree.structural_gen > gen_before, "Structural generation should bump");
    }

    #[test]
    fn split_column_creates_two_conversations() {
        let mut tree = TilingTree::default_layout();
        let conv1 = tree.first_conversation_pane().unwrap();

        let conv2 = tree.split(conv1, SplitDirection::Column);
        assert!(conv2.is_some());

        let convs = tree.root.conversation_panes();
        assert_eq!(convs.len(), 2);
    }

    #[test]
    fn split_same_direction_adds_sibling() {
        let mut tree = TilingTree::default_layout();
        let conv1 = tree.first_conversation_pane().unwrap();

        // First split creates a Row wrapper
        let conv2 = tree.split(conv1, SplitDirection::Row).unwrap();
        // Second split in same direction adds inline (no extra nesting)
        let conv3 = tree.split(conv1, SplitDirection::Row).unwrap();

        let convs = tree.root.conversation_panes();
        assert_eq!(convs.len(), 3, "Should have three conversation panes");

        // All three should be distinct
        assert_ne!(conv1, conv2);
        assert_ne!(conv2, conv3);
        assert_ne!(conv1, conv3);
    }

    #[test]
    fn split_invalid_target_returns_none() {
        let mut tree = TilingTree::default_layout();
        // Try to split a non-existent pane
        let result = tree.split(PaneId(9999), SplitDirection::Row);
        assert!(result.is_none());
    }

    // ── Close tests ─────────────────────────────────────────────────

    #[test]
    fn close_last_pane_returns_false() {
        let mut tree = TilingTree::default_layout();
        let conv = tree.first_conversation_pane().unwrap();
        assert!(!tree.close(conv), "Should not close the last conversation");
    }

    #[test]
    fn close_removes_pane() {
        let mut tree = TilingTree::default_layout();
        let conv1 = tree.first_conversation_pane().unwrap();
        let conv2 = tree.split(conv1, SplitDirection::Row).unwrap();

        assert!(tree.close(conv2), "Should close the second pane");
        let convs = tree.root.conversation_panes();
        assert_eq!(convs.len(), 1, "Should have one conversation after close");
    }

    #[test]
    fn close_updates_focus() {
        let mut tree = TilingTree::default_layout();
        let conv1 = tree.first_conversation_pane().unwrap();
        let conv2 = tree.split(conv1, SplitDirection::Row).unwrap();
        tree.focus(conv2);

        assert!(tree.close(conv2));
        // Focus should move to the remaining pane
        assert_eq!(tree.focused, conv1);
    }

    // ── Focus direction tests ───────────────────────────────────────

    #[test]
    fn focus_direction_row() {
        let mut tree = TilingTree::default_layout();
        let conv1 = tree.first_conversation_pane().unwrap();
        let conv2 = tree.split(conv1, SplitDirection::Row).unwrap();
        tree.focus(conv1);

        // Focus right should go to conv2
        assert!(tree.focus_direction(FocusDirection::Right));
        assert_eq!(tree.focused, conv2);

        // Focus left should go back to conv1
        assert!(tree.focus_direction(FocusDirection::Left));
        assert_eq!(tree.focused, conv1);

        // Focus left at edge should return false
        assert!(!tree.focus_direction(FocusDirection::Left));
    }

    #[test]
    fn focus_direction_column() {
        let mut tree = TilingTree::default_layout();
        let conv1 = tree.first_conversation_pane().unwrap();
        let conv2 = tree.split(conv1, SplitDirection::Column).unwrap();
        tree.focus(conv1);

        assert!(tree.focus_direction(FocusDirection::Down));
        assert_eq!(tree.focused, conv2);

        assert!(tree.focus_direction(FocusDirection::Up));
        assert_eq!(tree.focused, conv1);
    }

    // ── Resize tests ────────────────────────────────────────────────

    #[test]
    fn resize_adjusts_ratios() {
        let mut tree = TilingTree::default_layout();
        let conv1 = tree.first_conversation_pane().unwrap();
        let _conv2 = tree.split(conv1, SplitDirection::Row).unwrap();

        let gen_before = tree.visual_gen;
        assert!(tree.resize(conv1, 0.1));
        assert!(tree.visual_gen > gen_before);
    }

    #[test]
    fn resize_single_pane_returns_false() {
        let mut tree = TilingTree::default_layout();
        let conv = tree.first_conversation_pane().unwrap();
        assert!(!tree.resize(conv, 0.1), "Can't resize with only one pane");
    }

    // ── Toggle focus test ───────────────────────────────────────────

    #[test]
    fn toggle_focus_swaps_between_panes() {
        let mut tree = TilingTree::default_layout();
        let conv1 = tree.first_conversation_pane().unwrap();
        let conv2 = tree.split(conv1, SplitDirection::Row).unwrap();

        tree.focus(conv2);
        assert_eq!(tree.focused, conv2);
        assert_eq!(tree.previous_focused, Some(conv1));

        tree.toggle_focus();
        assert_eq!(tree.focused, conv1);
        assert_eq!(tree.previous_focused, Some(conv2));
    }

    // ── contains_pane tests ─────────────────────────────────────────

    #[test]
    fn contains_pane_finds_nested() {
        let tree = TilingTree::default_layout();
        let conv = tree.first_conversation_pane().unwrap();
        assert!(tree.root.contains_pane(conv));
        assert!(!tree.root.contains_pane(PaneId(9999)));
    }

    // ── Generation split tests ─────────────────────────────────────

    #[test]
    fn split_bumps_structural_gen() {
        let mut tree = TilingTree::default_layout();
        let conv1 = tree.first_conversation_pane().unwrap();
        let s_before = tree.structural_gen;
        let v_before = tree.visual_gen;

        tree.split(conv1, SplitDirection::Row);
        assert!(tree.structural_gen > s_before, "split should bump structural_gen");
        assert!(tree.visual_gen > v_before, "split should also bump visual_gen");
    }

    #[test]
    fn focus_bumps_only_visual_gen() {
        let mut tree = TilingTree::default_layout();
        let conv1 = tree.first_conversation_pane().unwrap();
        let conv2 = tree.split(conv1, SplitDirection::Row).unwrap();
        let s_before = tree.structural_gen;
        let v_before = tree.visual_gen;

        tree.focus(conv2);
        assert_eq!(tree.structural_gen, s_before, "focus should NOT bump structural_gen");
        assert!(tree.visual_gen > v_before, "focus should bump visual_gen");
    }

    #[test]
    fn resize_bumps_only_visual_gen() {
        let mut tree = TilingTree::default_layout();
        let conv1 = tree.first_conversation_pane().unwrap();
        let _conv2 = tree.split(conv1, SplitDirection::Row).unwrap();
        let s_before = tree.structural_gen;
        let v_before = tree.visual_gen;

        tree.resize(conv1, 0.1);
        assert_eq!(tree.structural_gen, s_before, "resize should NOT bump structural_gen");
        assert!(tree.visual_gen > v_before, "resize should bump visual_gen");
    }

    #[test]
    fn close_bumps_structural_gen() {
        let mut tree = TilingTree::default_layout();
        let conv1 = tree.first_conversation_pane().unwrap();
        let conv2 = tree.split(conv1, SplitDirection::Row).unwrap();
        let s_before = tree.structural_gen;

        tree.close(conv2);
        assert!(tree.structural_gen > s_before, "close should bump structural_gen");
    }

    // ── Swap tests ────────────────────────────────────────────────────

    #[test]
    fn swap_cross_parent() {
        // Create a layout with 3 panes: split conv1 horizontally, then split conv2 vertically
        // This gives us panes in different parent Splits.
        let mut tree = TilingTree::default_layout();
        let conv1 = tree.first_conversation_pane().unwrap();
        let conv2 = tree.split(conv1, SplitDirection::Row).unwrap();
        let conv3 = tree.split(conv2, SplitDirection::Column).unwrap();

        // conv2 and conv3 share a parent (Column split), conv1 is in a different parent (Row split)
        // Swap conv1 and conv3 (cross-parent)
        let gen_before = tree.structural_gen;
        assert!(tree.swap(conv1, conv3), "Cross-parent swap should succeed");
        assert!(tree.structural_gen > gen_before, "Swap should bump structural_gen");

        // After swap, both panes should still be findable in the tree
        assert!(tree.root.find(conv1).is_some(), "conv1 should still exist after swap");
        assert!(tree.root.find(conv3).is_some(), "conv3 should still exist after swap");
        assert!(tree.root.find(conv2).is_some(), "conv2 should be unaffected");

        // All three conversations should still exist
        let convs = tree.root.conversation_panes();
        assert_eq!(convs.len(), 3, "Should still have 3 conversations after swap");

        // All IDs should be unique (no corruption from DFS collision)
        let mut ids = Vec::new();
        tree.root.collect_ids(&mut ids);
        let deduped: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(ids.len(), deduped.len(), "All PaneIds should remain unique after swap");
    }

    #[test]
    fn swap_siblings() {
        let mut tree = TilingTree::default_layout();
        let conv1 = tree.first_conversation_pane().unwrap();
        let conv2 = tree.split(conv1, SplitDirection::Row).unwrap();

        assert!(tree.swap(conv1, conv2), "Sibling swap should succeed");

        let convs = tree.root.conversation_panes();
        assert_eq!(convs.len(), 2, "Should still have 2 conversations after swap");
    }

    // ── Close focuses neighbor test ───────────────────────────────────

    #[test]
    fn close_focuses_neighbor() {
        // Create 3 panes in a row: conv1 | conv2 | conv3
        let mut tree = TilingTree::default_layout();
        let conv1 = tree.first_conversation_pane().unwrap();
        let conv2 = tree.split(conv1, SplitDirection::Row).unwrap();
        let conv3 = tree.split(conv2, SplitDirection::Row).unwrap();

        // Focus conv2 (middle pane), then close it
        tree.focus(conv2);
        assert!(tree.close(conv2));

        // Focus should go to a spatial neighbor (conv3 right, or conv1 left),
        // not just the first conversation in tree order
        assert!(
            tree.focused == conv1 || tree.focused == conv3,
            "After closing middle pane, focus should go to a neighbor, got {}",
            tree.focused
        );

        // Specifically, our implementation tries right neighbor first (conv3)
        assert_eq!(
            tree.focused, conv3,
            "Should prefer right neighbor after close"
        );
    }
}
