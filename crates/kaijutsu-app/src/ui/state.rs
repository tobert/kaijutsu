//! App screen state management
//!
//! Manages which screen is active (Dashboard vs Conversation) using Bevy's
//! state system. This replaces manual visibility toggling with proper
//! state-driven transitions.
//!
//! ## Input Architecture
//!
//! Input is handled by ComposeBlock - an inline editable block at the end of
//! the conversation. The legacy floating InputLayer has been removed.

use bevy::prelude::*;

/// The main application screen state.
///
/// Uses Bevy's state system for clean screen transitions with proper
/// system gating via `run_if(in_state(...))`.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Hash, States, Reflect)]
pub enum AppScreen {
    /// Dashboard - Kernel/Context/Seat selection view
    #[default]
    Dashboard,
    /// Conversation - Active conversation view (the "world")
    Conversation,
}

/// Marker for the main content area that contains both views
#[derive(Component)]
pub struct ContentArea;

/// Marker for the conversation view root
#[derive(Component)]
pub struct ConversationRoot;

// ============================================================================
// VIEW STACK (Phase 4)
// ============================================================================

use kaijutsu_crdt::BlockId;

/// A view in the application - what you're looking at.
///
/// Views are orthogonal to modes:
/// - **Modes** = how you interact (Normal, Input, Visual)
/// - **Views** = what you're looking at (Dashboard, Conversation, ExpandedBlock)
///
/// You can be in ExpandedBlock view in Normal mode or Input mode.
#[derive(Debug, Clone, PartialEq)]
pub enum View {
    /// Dashboard - kernel/conversation selection
    Dashboard,
    /// Conversation view - main chat/shell interface
    #[allow(dead_code)]
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
    /// Check if this is a root view (Dashboard or Conversation).
    pub fn is_root(&self) -> bool {
        matches!(self, View::Dashboard | View::Conversation { .. })
    }

    /// Check if this is an overlay view (pushed on top of root).
    #[allow(dead_code)]
    pub fn is_overlay(&self) -> bool {
        !self.is_root()
    }

    /// Get the base AppScreen this view corresponds to.
    #[allow(dead_code)]
    pub fn base_screen(&self) -> AppScreen {
        match self {
            View::Dashboard => AppScreen::Dashboard,
            View::Conversation { .. } | View::ExpandedBlock { .. } => AppScreen::Conversation,
        }
    }

    /// Get the root container type for this view.
    ///
    /// Used by the layout reconciler to find the correct parent entity.
    pub fn root_container(&self) -> ViewRootContainer {
        match self {
            View::Dashboard => ViewRootContainer::Dashboard,
            View::Conversation { .. } => ViewRootContainer::Conversation,
            View::ExpandedBlock { .. } => ViewRootContainer::Conversation, // Overlay on conversation
        }
    }
}

/// Identifies which root container a view uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewRootContainer {
    /// Dashboard root (DashboardRoot marker)
    Dashboard,
    /// Conversation root (ConversationRoot marker)
    Conversation,
}

/// Stack of views for navigation.
///
/// The view stack enables:
/// - Pushing overlay views (ExpandedBlock) on top of root views
/// - Popping back with Esc (in Normal mode)
/// - Tracking navigation history
///
/// The bottom of the stack is always a root view (Dashboard or Conversation).
#[derive(Resource, Debug)]
pub struct ViewStack {
    stack: Vec<View>,
}

impl Default for ViewStack {
    fn default() -> Self {
        Self {
            stack: vec![View::Dashboard],
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

/// Plugin for app screen state management
pub struct AppScreenPlugin;

impl Plugin for AppScreenPlugin {
    fn build(&self, app: &mut App) {
        // Register types for BRP reflection
        app.register_type::<AppScreen>();

        // Initialize ViewStack resource
        app.init_resource::<ViewStack>();

        app.init_state::<AppScreen>()
            .add_systems(OnEnter(AppScreen::Dashboard), show_dashboard)
            .add_systems(OnExit(AppScreen::Dashboard), hide_dashboard)
            .add_systems(OnEnter(AppScreen::Conversation), show_conversation)
            .add_systems(OnExit(AppScreen::Conversation), hide_conversation)
            // ViewStack visibility management
            .add_systems(Update, sync_expanded_block_visibility);
    }
}

// ============================================================================
// State Transition Systems
// ============================================================================

// Note: We set both Display and Visibility because glyphon text rendering
// doesn't respect Display::None - it only respects Visibility::Hidden.

/// Show the dashboard view when entering Dashboard state.
///
/// Focus is automatically set to FocusArea::Dashboard by sync_focus_from_screen.
fn show_dashboard(
    mut query: Query<(&mut Node, &mut Visibility), With<crate::dashboard::DashboardRoot>>,
) {
    for (mut node, mut vis) in query.iter_mut() {
        node.display = Display::Flex;
        *vis = Visibility::Inherited;
    }
}

/// Hide the dashboard view when leaving Dashboard state
fn hide_dashboard(
    mut query: Query<(&mut Node, &mut Visibility), With<crate::dashboard::DashboardRoot>>,
) {
    for (mut node, mut vis) in query.iter_mut() {
        node.display = Display::None;
        *vis = Visibility::Hidden;
    }
}

/// Show the conversation view when entering Conversation state
fn show_conversation(
    mut query: Query<(&mut Node, &mut Visibility), With<ConversationRoot>>,
) {
    for (mut node, mut vis) in query.iter_mut() {
        node.display = Display::Flex;
        *vis = Visibility::Inherited;
    }
}

/// Hide the conversation view when leaving Conversation state
fn hide_conversation(mut query: Query<(&mut Node, &mut Visibility), With<ConversationRoot>>) {
    for (mut node, mut vis) in query.iter_mut() {
        node.display = Display::None;
        *vis = Visibility::Hidden;
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

