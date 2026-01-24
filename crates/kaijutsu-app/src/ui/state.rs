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

/// Plugin for app screen state management
pub struct AppScreenPlugin;

impl Plugin for AppScreenPlugin {
    fn build(&self, app: &mut App) {
        // Register types for BRP reflection
        app.register_type::<AppScreen>()
            .register_type::<InputPresenceKind>()
            .register_type::<InputPresence>();

        app.init_state::<AppScreen>()
            .add_systems(OnEnter(AppScreen::Dashboard), show_dashboard)
            .add_systems(OnExit(AppScreen::Dashboard), hide_dashboard)
            .add_systems(OnEnter(AppScreen::Conversation), show_conversation)
            .add_systems(OnExit(AppScreen::Conversation), hide_conversation);
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
fn show_conversation(mut query: Query<(&mut Node, &mut Visibility), With<ConversationRoot>>) {
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
