//! App screen state management
//!
//! Manages which screen is active (Dashboard vs Conversation) using Bevy's
//! state system. This replaces manual visibility toggling with proper
//! state-driven transitions.

use bevy::prelude::*;

/// The main application screen state.
///
/// Uses Bevy's state system for clean screen transitions with proper
/// system gating via `run_if(in_state(...))`.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Hash, States)]
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
