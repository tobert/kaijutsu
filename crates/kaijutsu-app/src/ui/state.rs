//! App state management
//!
//! Manages view stack for overlay navigation (expanded blocks, etc.).
//! The dashboard has been removed — the app starts directly in the
//! conversation view. Connection + context join happens in background.
//!
//! ## Input Architecture
//!
//! Input is handled by ComposeBlock - an inline editable block at the end of
//! the conversation. The legacy floating InputLayer has been removed.

use bevy::prelude::*;

/// Marker for the main content area
#[derive(Component)]
pub struct ContentArea;

/// Marker for the conversation view root
#[derive(Component)]
pub struct ConversationRoot;

// ============================================================================
// VIEW STACK
// ============================================================================

use kaijutsu_crdt::BlockId;

/// A view in the application - what you're looking at.
///
/// Views are orthogonal to modes:
/// - **Modes** = how you interact (Normal, Input, Visual)
/// - **Views** = what you're looking at (Conversation, ExpandedBlock)
///
/// You can be in ExpandedBlock view in Normal mode or Input mode.
#[derive(Debug, Clone, PartialEq)]
pub enum View {
    /// Conversation view - main chat/shell interface
    Conversation {
        /// The conversation/kernel ID being viewed
        kernel_id: String,
    },
    /// Full-screen expanded block view
    ExpandedBlock {
        /// The block being viewed/edited
        block_id: BlockId,
    },
    // Future views:
    // Editor { path: String },
    // Diff { left: String, right: String },
    // Plugin { id: String },
}

impl View {
    /// Check if this is a root view (Conversation).
    pub fn is_root(&self) -> bool {
        matches!(self, View::Conversation { .. })
    }

    /// Check if this is an overlay view (pushed on top of root).
    #[allow(dead_code)]
    pub fn is_overlay(&self) -> bool {
        !self.is_root()
    }
}

/// Stack of views for navigation.
///
/// The view stack enables:
/// - Pushing overlay views (ExpandedBlock) on top of root views
/// - Popping back with Esc (in Normal mode)
/// - Tracking navigation history
///
/// The bottom of the stack is always a root view (Conversation).
#[derive(Resource, Debug)]
pub struct ViewStack {
    stack: Vec<View>,
}

impl Default for ViewStack {
    fn default() -> Self {
        Self {
            stack: vec![View::Conversation {
                kernel_id: String::new(),
            }],
        }
    }
}

impl ViewStack {
    /// Get the current (top) view.
    pub fn current(&self) -> &View {
        self.stack.last().expect("ViewStack should never be empty")
    }

    /// Check if we're at a root view (can't pop further).
    pub fn is_at_root(&self) -> bool {
        self.stack.len() == 1 || self.current().is_root()
    }

    /// Push a new view onto the stack.
    pub fn push(&mut self, view: View) {
        // If pushing a root view, replace the stack
        if view.is_root() {
            self.stack.clear();
        }
        self.stack.push(view);
        info!("ViewStack push: {:?} (depth={})", self.current(), self.stack.len());
    }

    /// Pop the current view, returning to the previous one.
    /// Returns None if already at root (can't pop).
    pub fn pop(&mut self) -> Option<View> {
        if self.is_at_root() {
            return None;
        }
        let popped = self.stack.pop();
        info!("ViewStack pop: {:?} → {:?}", popped, self.current());
        popped
    }

    /// Get the stack depth.
    #[allow(dead_code)]
    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    /// Check if an ExpandedBlock view is active.
    pub fn has_expanded_block(&self) -> bool {
        matches!(self.current(), View::ExpandedBlock { .. })
    }

    /// Get the expanded block ID if in ExpandedBlock view.
    pub fn expanded_block_id(&self) -> Option<&BlockId> {
        match self.current() {
            View::ExpandedBlock { block_id } => Some(block_id),
            _ => None,
        }
    }
}

/// Marker for the ExpandedBlock view container.
#[derive(Component)]
pub struct ExpandedBlockView;

/// Plugin for app state management
pub struct AppScreenPlugin;

impl Plugin for AppScreenPlugin {
    fn build(&self, app: &mut App) {
        // Initialize ViewStack resource
        app.init_resource::<ViewStack>();

        app
            // ViewStack visibility management
            .add_systems(Update, sync_expanded_block_visibility)
            // Handle ContextJoined — create conversation metadata (replaces dashboard handler)
            .add_systems(Update, handle_context_joined);
    }
}

// ============================================================================
// CONTEXT JOINED HANDLER
// ============================================================================

/// Handle ContextJoined events to create conversation metadata.
///
/// This replaces the logic that was in dashboard::handle_dashboard_events.
/// When a context is joined (from ActorPlugin bootstrap), creates the
/// conversation in the registry and sets it as current.
fn handle_context_joined(
    mut result_events: MessageReader<crate::connection::RpcResultMessage>,
    mut registry: ResMut<crate::conversation::ConversationRegistry>,
    mut current_conv: ResMut<crate::conversation::CurrentConversation>,
) {
    for result in result_events.read() {
        if let crate::connection::RpcResultMessage::ContextJoined { document_id, .. } = result {
            // Create conversation metadata if it doesn't exist (idempotent)
            if registry.get(document_id).is_none() {
                let conv = kaijutsu_kernel::Conversation::with_id(document_id, document_id);
                registry.add(conv);
                info!("Created conversation metadata for {}", document_id);
            }

            current_conv.0 = Some(document_id.clone());
        }
    }
}

// ============================================================================
// VIEW STACK VISIBILITY
// ============================================================================

/// Sync visibility of ExpandedBlockView based on ViewStack state.
///
/// Shows the expanded block view when ViewStack has an ExpandedBlock,
/// hides it otherwise. Also hides the conversation container when
/// expanded block is showing (overlay behavior).
fn sync_expanded_block_visibility(
    view_stack: Res<ViewStack>,
    mut expanded_views: Query<&mut Visibility, With<ExpandedBlockView>>,
    mut conversation_containers: Query<
        &mut Visibility,
        (With<crate::cell::ConversationContainer>, Without<ExpandedBlockView>),
    >,
) {
    if !view_stack.is_changed() {
        return;
    }

    let show_expanded = view_stack.has_expanded_block();

    // Show/hide expanded block view
    for mut vis in expanded_views.iter_mut() {
        *vis = if show_expanded {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
    }

    // Hide conversation container when expanded block is showing
    // (expanded block takes over the full screen)
    for mut vis in conversation_containers.iter_mut() {
        *vis = if show_expanded {
            Visibility::Hidden
        } else {
            Visibility::Inherited
        };
    }
}
