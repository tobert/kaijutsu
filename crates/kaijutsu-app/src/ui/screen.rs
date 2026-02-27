//! Screen state machine — the single authority for which full-viewport view is active.
//!
//! Replaces the ad-hoc `ConstellationVisible` resource + `FocusArea::Constellation`
//! coupling with Bevy's `States` system. Each screen gets `OnEnter`/`OnExit` schedules
//! for visibility management and cleanup.
//!
//! ## Screens
//!
//! - `Constellation` — 3D context navigation graph (default)
//! - `Conversation` — chat/shell conversation view
//! - `ForkForm` — full-viewport fork configuration form
//!
//! ## Design
//!
//! Long-lived entities (3D scene, containers, block cells) persist across transitions
//! and are shown/hidden via Display + Visibility + camera `is_active`. Screen-scoped
//! entities (fork form UI) use `DespawnOnExit` for automatic cleanup.

use bevy::prelude::*;

use super::constellation::{ConstellationContainer, viewport::ConstellationCamera3d};
use super::state::ConversationRoot;
use crate::cell::{BlockCell, RoleHeader};
use crate::text::KjText;

/// Which full-viewport view is currently active.
///
/// This is the single source of truth for screen-level visibility.
/// Systems use `run_if(in_state(Screen::...))` for input gating.
/// `OnEnter`/`OnExit` schedules handle visibility transitions.
#[derive(States, Clone, Copy, Default, Eq, PartialEq, Hash, Debug, Reflect)]
pub enum Screen {
    /// 3D context navigation graph (default — app starts here).
    #[default]
    Constellation,
    /// Chat/shell conversation view.
    Conversation,
    /// Full-viewport fork configuration form.
    ForkForm,
}

/// Plugin that registers the Screen state and its transition systems.
pub struct ScreenPlugin;

impl Plugin for ScreenPlugin {
    fn build(&self, app: &mut App) {
        app.init_state::<Screen>()
            .register_type::<Screen>();

        // ── Constellation ──
        app.add_systems(OnEnter(Screen::Constellation), (
            activate_constellation_camera,
            show_constellation_container,
            hide_conversation_root,
            hide_cell_text,
        ));
        app.add_systems(OnExit(Screen::Constellation), (
            deactivate_constellation_camera,
        ));

        // ── Conversation ──
        app.add_systems(OnEnter(Screen::Conversation), (
            show_conversation_root,
            hide_constellation_container,
            show_cell_text,
            set_focus_conversation,
        ));

        // ── ForkForm ──
        // Form UI is spawned by the message handler that triggers the transition.
        // DespawnOnExit(Screen::ForkForm) handles cleanup automatically.
        // OnExit(Constellation) already deactivated the camera.

        // ── Continuous ──
        // Hide newly-added text entities that appear while not in conversation
        // (e.g., block cells created by background sync while constellation is showing).
        app.add_systems(
            Update,
            hide_new_cell_text_outside_conversation
                .run_if(not(in_state(Screen::Conversation))),
        );
    }
}

// ============================================================================
// OnEnter/OnExit SYSTEMS
// ============================================================================

/// Activate the 3D constellation camera.
fn activate_constellation_camera(
    mut cameras: Query<&mut Camera, With<ConstellationCamera3d>>,
) {
    for mut camera in cameras.iter_mut() {
        camera.is_active = true;
    }
}

/// Deactivate the 3D constellation camera (saves GPU work).
fn deactivate_constellation_camera(
    mut cameras: Query<&mut Camera, With<ConstellationCamera3d>>,
) {
    for mut camera in cameras.iter_mut() {
        camera.is_active = false;
    }
}

/// Show the constellation container (Display::Flex + Visibility::Inherited).
fn show_constellation_container(
    mut containers: Query<
        (&mut Node, &mut Visibility),
        (With<ConstellationContainer>, Without<ConversationRoot>),
    >,
) {
    for (mut node, mut vis) in containers.iter_mut() {
        node.display = Display::Flex;
        *vis = Visibility::Inherited;
    }
}

/// Hide the constellation container (Display::None + Visibility::Hidden).
fn hide_constellation_container(
    mut containers: Query<
        (&mut Node, &mut Visibility),
        (With<ConstellationContainer>, Without<ConversationRoot>),
    >,
) {
    for (mut node, mut vis) in containers.iter_mut() {
        node.display = Display::None;
        *vis = Visibility::Hidden;
    }
}

/// Show the conversation root (Display::Flex + Visibility::Inherited).
fn show_conversation_root(
    mut roots: Query<
        (&mut Node, &mut Visibility),
        (With<ConversationRoot>, Without<ConstellationContainer>),
    >,
) {
    for (mut node, mut vis) in roots.iter_mut() {
        node.display = Display::Flex;
        *vis = Visibility::Inherited;
    }
}

/// Hide the conversation root (Display::None + Visibility::Hidden).
fn hide_conversation_root(
    mut roots: Query<
        (&mut Node, &mut Visibility),
        (With<ConversationRoot>, Without<ConstellationContainer>),
    >,
) {
    for (mut node, mut vis) in roots.iter_mut() {
        node.display = Display::None;
        *vis = Visibility::Hidden;
    }
}

/// Hide block cells and role headers when leaving conversation.
///
/// With Vello, text is rendered ON the Node entity (UiVelloText component
/// lives on the same entity as Node). Display::None on ConversationRoot
/// suppresses layout but does not prevent Vello from rendering at the
/// entity's stale UiGlobalTransform. We must explicitly set Visibility.
fn hide_cell_text(
    mut block_cells: Query<&mut Visibility, (With<BlockCell>, Without<RoleHeader>)>,
    mut role_headers: Query<&mut Visibility, (With<RoleHeader>, Without<BlockCell>)>,
) {
    for mut vis in block_cells.iter_mut() {
        *vis = Visibility::Hidden;
    }
    for mut vis in role_headers.iter_mut() {
        *vis = Visibility::Hidden;
    }
}

/// Show block cells and role headers when entering conversation.
fn show_cell_text(
    mut block_cells: Query<&mut Visibility, (With<BlockCell>, Without<RoleHeader>)>,
    mut role_headers: Query<&mut Visibility, (With<RoleHeader>, Without<BlockCell>)>,
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
fn set_focus_conversation(
    mut focus: ResMut<crate::input::focus::FocusArea>,
) {
    *focus = crate::input::focus::FocusArea::Conversation;
}

/// Hide newly-added block cells and role headers when not in conversation view.
///
/// Block cells may be created by background sync while the constellation or
/// fork form is showing. Without this, they'd bleed through until the next
/// screen transition.
fn hide_new_cell_text_outside_conversation(
    mut new_blocks: Query<&mut Visibility, (Added<KjText>, Or<(With<BlockCell>, With<RoleHeader>)>)>,
) {
    for mut vis in new_blocks.iter_mut() {
        *vis = Visibility::Hidden;
    }
}
