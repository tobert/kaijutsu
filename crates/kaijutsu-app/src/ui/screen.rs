//! Screen state machine — the single authority for which full-viewport view is active.
//!
//! After the app reset there is a single screen, `Conversation`. The state
//! machine is retained (rather than removed) so that `OnEnter(Screen::Conversation)`
//! still drives the initial visibility + focus setup, and so future screens can
//! be reintroduced without rewiring `run_if(in_state(...))` call sites.
//!
//! ## Design
//!
//! Long-lived entities (containers, block cells) persist and are shown via
//! Visibility on screen enter.

use bevy::prelude::*;

use super::state::ConversationRoot;
use crate::cell::{BlockCell, RoleGroupBorder};

/// Which full-viewport view is currently active.
///
/// This is the single source of truth for screen-level visibility.
/// Systems use `run_if(in_state(Screen::...))` for input gating.
/// `OnEnter`/`OnExit` schedules handle visibility transitions.
#[derive(States, Clone, Copy, Default, Eq, PartialEq, Hash, Debug, Reflect)]
pub enum Screen {
    /// Chat/shell conversation view (the only screen).
    #[default]
    Conversation,
}

/// Plugin that registers the Screen state and its transition systems.
pub struct ScreenPlugin;

impl Plugin for ScreenPlugin {
    fn build(&self, app: &mut App) {
        app.init_state::<Screen>().register_type::<Screen>();

        // ── Conversation ──
        app.add_systems(
            OnEnter(Screen::Conversation),
            (show_conversation_root, show_cell_text, set_focus_conversation),
        );
    }
}

// ============================================================================
// OnEnter/OnExit SYSTEMS
// ============================================================================

/// Show the conversation root.
fn show_conversation_root(mut roots: Query<&mut Visibility, With<ConversationRoot>>) {
    for mut vis in roots.iter_mut() {
        *vis = Visibility::Inherited;
    }
}

/// Show block cells and role headers when entering conversation.
fn show_cell_text(
    mut block_cells: Query<&mut Visibility, (With<BlockCell>, Without<RoleGroupBorder>)>,
    mut role_headers: Query<&mut Visibility, (With<RoleGroupBorder>, Without<BlockCell>)>,
) {
    for mut vis in block_cells.iter_mut() {
        *vis = Visibility::Inherited;
    }
    for mut vis in role_headers.iter_mut() {
        *vis = Visibility::Inherited;
    }
}

/// Set focus to Conversation (navigation mode) when entering conversation view.
///
/// Input is now an ephemeral overlay summoned with i/:, not a permanent fixture.
/// Entering conversation view = navigation mode by default.
fn set_focus_conversation(mut focus: ResMut<crate::input::focus::FocusArea>) {
    *focus = crate::input::focus::FocusArea::Conversation;
}
