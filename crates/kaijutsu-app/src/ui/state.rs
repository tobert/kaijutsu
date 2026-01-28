//! App screen state management
//!
//! Manages which screen is active (Dashboard vs Conversation) using Bevy's
//! state system. This replaces manual visibility toggling with proper
//! state-driven transitions.
//!
//! ## Input Area Architecture
//!
//! The input area uses an overlay-first architecture where positioning is computed
//! from `InputPresence` and `InputDock`, then applied via `InputPosition`.
//!
//! ```text
//! ConversationRoot (flex column)
//! ├── ConversationContainer (flex_grow: 1, scrollable)
//! └── InputShadow (flex child, reserves space)
//!         └── ChasingLineDecoration (always visible neon floor)
//!
//! InputLayer (world-level, ZIndex(100))  ← FLOATS OVER SHADOW
//! ├── Backdrop (optional, when presence=Overlay)
//! ├── InputFrame (9-slice, when Docked or Overlay)
//! └── PromptCell (absolute, from InputPosition)
//! ```

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

/// Marker for the status bar (chrome, always visible)
#[derive(Component)]
pub struct StatusBar;

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
        app.register_type::<AppScreen>()
            .register_type::<InputPresenceKind>()
            .register_type::<InputPresence>();

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

/// Show the dashboard view when entering Dashboard state
fn show_dashboard(
    mut query: Query<(&mut Node, &mut Visibility), With<crate::dashboard::DashboardRoot>>,
    mut presence: ResMut<InputPresence>,
    mut mode: ResMut<crate::cell::CurrentMode>,
) {
    for (mut node, mut vis) in query.iter_mut() {
        node.display = Display::Flex;
        *vis = Visibility::Inherited;
    }
    // Hide input area when on dashboard
    presence.0 = InputPresenceKind::Hidden;
    // Reset mode to Normal (Chat/Shell make no sense on Dashboard)
    mode.0 = crate::cell::EditorMode::Normal;
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
    mut presence: ResMut<InputPresence>,
) {
    for (mut node, mut vis) in query.iter_mut() {
        node.display = Display::Flex;
        *vis = Visibility::Inherited;
    }
    // Restore input area when entering conversation
    presence.0 = InputPresenceKind::Docked;
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

// ============================================================================
// INPUT AREA STATE MANAGEMENT
// ============================================================================

/// Input area presence state - determines visibility and positioning mode.
///
/// Design principle: Overlay is the base case, Docked and Minimized are special cases.
/// - **Overlay**: Floating centered with backdrop (Space to summon)
/// - **Docked**: Pinned to dock position, no backdrop (i to enter insert)
/// - **Minimized**: Only chasing line visible (reading mode)
/// - **Hidden**: Dashboard state, input completely hidden
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Reflect)]
pub enum InputPresenceKind {
    /// Floating centered overlay with backdrop
    Overlay,
    /// Pinned to dock position (bottom, bottom-right, etc.)
    #[default]
    Docked,
    /// Collapsed to thin chasing line at bottom
    Minimized,
    /// Completely hidden (Dashboard state)
    Hidden,
}

/// Resource tracking current input presence state.
#[derive(Resource, Default, Reflect)]
#[reflect(Resource)]
pub struct InputPresence(pub InputPresenceKind);

impl InputPresence {
    pub fn is_visible(&self) -> bool {
        !matches!(self.0, InputPresenceKind::Hidden | InputPresenceKind::Minimized)
    }

    pub fn shows_backdrop(&self) -> bool {
        matches!(self.0, InputPresenceKind::Overlay)
    }
}

/// Dock position for the input area when in Docked presence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum InputDockKind {
    /// Full-width at bottom (default)
    #[default]
    Bottom,
}

/// Resource tracking dock position preference.
#[derive(Resource, Default)]
pub struct InputDock(pub InputDockKind);

/// Computed input position, updated when presence/dock/window changes.
///
/// This is the single source of truth for input area positioning.
/// Systems read this to position the InputLayer and its contents.
#[derive(Resource, Default, Debug, Clone)]
pub struct InputPosition {
    /// Left edge X coordinate
    pub x: f32,
    /// Top edge Y coordinate
    pub y: f32,
    /// Width of input area
    pub width: f32,
    /// Height of input area
    pub height: f32,
    /// Whether to show the backdrop (dim overlay behind input)
    pub show_backdrop: bool,
    /// Whether to show the 9-slice frame
    pub show_frame: bool,
}

/// Height of the InputShadow flex element.
///
/// This determines how much space the shadow reserves in the flex layout.
/// The actual input floats over this space.
#[derive(Resource)]
pub struct InputShadowHeight(pub f32);

impl Default for InputShadowHeight {
    fn default() -> Self {
        // Default: enough space for the chasing line decoration
        Self(6.0)
    }
}

// ============================================================================
// INPUT AREA MARKERS
// ============================================================================

/// Marker for the InputShadow - flex child that reserves space at bottom.
///
/// Contains the ChasingLineDecoration (always visible neon floor).
/// The actual input floats over this shadow when active.
#[derive(Component)]
pub struct InputShadow;

/// Marker for the InputLayer - world-level floating container.
///
/// Contains backdrop, frame, and PromptCell. Floats over InputShadow
/// and is positioned according to InputPosition.
#[derive(Component)]
pub struct InputLayer;

/// Marker for the backdrop within InputLayer.
///
/// Visibility toggles based on InputPresence (shown in Overlay mode).
#[derive(Component)]
pub struct InputBackdrop;

/// Marker for the frame container within InputLayer.
///
/// The 9-slice frame pieces are children of this entity.
#[derive(Component)]
pub struct InputFrame;
