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
    /// In-app vi editor — an MSDF panel rendering a kernel editor session.
    /// Full-viewport; conversation chrome is hidden while it is active. Entered
    /// by the `open_editor` peer signal (see `view::editor`).
    Editor,
    /// The room level above the well — the shell's station carousel
    /// (`docs/scenes/shell.md`). Reached by Up-Up at the well's mouth ring;
    /// Left/Right cycle stations, Enter/Down dives, Esc drops to Conversation.
    /// A bounded station (the patch bay wheel) is reached and left WITHOUT a
    /// further screen transition — "diving" into it is a camera pose plus a
    /// `view::room::RoomState::zoomed` write, all still `Screen::Room`
    /// (2026-07-10 evening, the fullscreen-panel pivot: "diving IS
    /// fullscreening a panel," superseding the earlier `Screen::PatchBay`).
    Room,
    /// The FSN landscape (`docs/scenes/vfs.md` slice 0, `view::fsn`) — the
    /// VFS-as-terrain world behind the room's N archway ("DATA HORIZON").
    /// Unlike the room's bounded, furnished stations (patch bay, the well),
    /// the landscape is an **unbounded world**, too big to stand as room
    /// furniture (`docs/scenes/shell.md`: "N stays a dive-THROUGH door, not
    /// a panel to fill the frame with") — so N-diving is a genuine `Screen`
    /// transition, not a `view::room::RoomState::zoomed` write. Entered from
    /// `Screen::Room` (Enter/Down on the focused `Station::Vfs`); Esc returns
    /// to `Screen::Room`, not `Conversation` — the room is the level directly
    /// below, same as every other dive.
    Fsn,
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

        // ── Room ──
        // The scenes-charter view (docs/scenes/): full-viewport 3D like the
        // well, reading raw keys — hide the chrome and park focus off Compose
        // (same reasoning as the editor above). Covers the whole shell,
        // zoomed into a station or not (`view::room::RoomState::zoomed`) —
        // there is no second screen for a station dive to enter any more.
        app.add_systems(
            OnEnter(Screen::Room),
            (hide_conversation_root, hide_cell_text, set_focus_conversation),
        );

        // ── Fsn ──
        // The FSN landscape (`view::fsn`): full-viewport 3D like the room it
        // dives from, reading raw keys for camera fly + select — hide the
        // chrome and park focus off Compose (same reasoning as Room/Editor
        // above). The world's own spawn/despawn rides `OnEnter`/`OnExit(Screen::Fsn)`
        // in `view::fsn::scene`, not here — this only owns the chrome.
        app.add_systems(
            OnEnter(Screen::Fsn),
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

/// Hide the conversation root when leaving for a full-viewport screen.
fn hide_conversation_root(mut roots: Query<&mut Visibility, With<ConversationRoot>>) {
    for mut vis in roots.iter_mut() {
        *vis = Visibility::Hidden;
    }
}

/// Hide block cells and role headers while a full-viewport screen owns it.
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
