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
    /// Chat/shell conversation view.
    #[default]
    Conversation,
    /// Time-well context browser — the radial 3D well of context cards.
    /// Full-viewport; conversation chrome is hidden while it is active.
    TimeWell,
    /// In-app vi editor — an MSDF panel rendering a kernel editor session.
    /// Full-viewport; conversation chrome is hidden while it is active. Entered
    /// by the `open_editor` peer signal (see `view::editor`).
    Editor,
    /// The room level above the well — the shell's station carousel
    /// (`docs/scenes/shell.md`). Reached by Up-Up at the well's mouth ring;
    /// Left/Right cycle stations, Enter/Down dives, Esc drops to Conversation.
    Room,
    /// Patch bay station — the observed ALSA-graph circle scene
    /// (`docs/scenes/patchbay.md`, slice 0). Reached from the room.
    PatchBay,
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

        // ── TimeWell ──
        // Hide the conversation chrome so the 3D well owns the viewport. The
        // well's own camera + card entities are managed by the time_well plugin's
        // OnEnter/OnExit systems. Returning to Conversation re-shows the chrome
        // via the OnEnter(Conversation) systems above.
        app.add_systems(
            OnEnter(Screen::TimeWell),
            (hide_conversation_root, hide_cell_text),
        );

        // ── Editor ──
        // Like the time well, the editor owns the viewport: hide the conversation
        // chrome on enter. Its MSDF panel + camera are managed by the editor
        // plugin (docs/vi.md step 4). Returning to Conversation re-shows the
        // chrome via the OnEnter(Conversation) systems above.
        //
        // Move focus off Compose: the editor forwards raw keystrokes itself
        // (`view::editor::editor_dispatch_keys`), and `vim_dispatch_compose` is
        // gated purely on `FocusArea::Compose` — leaving it there would
        // double-apply every keystroke to the hidden chat overlay.
        app.add_systems(
            OnEnter(Screen::Editor),
            (hide_conversation_root, hide_cell_text, set_focus_conversation),
        );

        // ── Room + PatchBay ──
        // The scenes-charter views (docs/scenes/): full-viewport 3D like the
        // well, reading raw keys — hide the chrome and park focus off Compose
        // (same reasoning as the editor above).
        app.add_systems(
            OnEnter(Screen::Room),
            (hide_conversation_root, hide_cell_text, set_focus_conversation),
        );
        app.add_systems(
            OnEnter(Screen::PatchBay),
            (hide_conversation_root, hide_cell_text, set_focus_conversation),
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

/// Hide the conversation root when leaving for the time well.
fn hide_conversation_root(mut roots: Query<&mut Visibility, With<ConversationRoot>>) {
    for mut vis in roots.iter_mut() {
        *vis = Visibility::Hidden;
    }
}

/// Hide block cells and role headers while the time well owns the viewport.
fn hide_cell_text(
    mut block_cells: Query<&mut Visibility, (With<BlockCell>, Without<RoleGroupBorder>)>,
    mut role_headers: Query<&mut Visibility, (With<RoleGroupBorder>, Without<BlockCell>)>,
) {
    for mut vis in block_cells.iter_mut() {
        *vis = Visibility::Hidden;
    }
    for mut vis in role_headers.iter_mut() {
        *vis = Visibility::Hidden;
    }
}

/// Set focus to Conversation (navigation mode) when entering conversation view.
///
/// Input is now an ephemeral overlay summoned with i/:, not a permanent fixture.
/// Entering conversation view = navigation mode by default.
fn set_focus_conversation(mut focus: ResMut<crate::input::focus::FocusArea>) {
    *focus = crate::input::focus::FocusArea::Conversation;
}
