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
use crate::text::MsdfText;

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
            set_focus_compose,
        ));

        // ── ForkForm ──
        // Form UI is spawned by the message handler that triggers the transition.
        // DespawnOnExit(Screen::ForkForm) handles cleanup automatically.
        // OnExit(Constellation) already deactivated the camera.

        // ── Continuous ──
        // Hide newly-added MSDF text entities that appear while not in conversation
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

/// Hide orphaned MSDF cell text when leaving conversation.
///
/// Block cells and role headers are spawned as root-level entities (no parent)
/// with screen-space coordinates. Since they're not descendants of
/// ConversationRoot, Visibility::Hidden doesn't propagate to them.
fn hide_cell_text(
    mut cell_texts: Query<&mut Visibility, (With<MsdfText>, Without<Node>)>,
) {
    for mut vis in cell_texts.iter_mut() {
        *vis = Visibility::Hidden;
    }
}

/// Show orphaned MSDF cell text when entering conversation.
fn show_cell_text(
    mut cell_texts: Query<&mut Visibility, (With<MsdfText>, Without<Node>)>,
) {
    for mut vis in cell_texts.iter_mut() {
        *vis = Visibility::Inherited;
    }
}

/// Set focus to Compose when entering conversation view.
fn set_focus_compose(
    mut focus: ResMut<crate::input::focus::FocusArea>,
) {
    *focus = crate::input::focus::FocusArea::Compose;
}

/// Hide newly-added MSDF cell text entities when not in conversation view.
///
/// Block cells may be created by background sync while the constellation or
/// fork form is showing. Without this, they'd bleed through until the next
/// screen transition.
fn hide_new_cell_text_outside_conversation(
    mut new_texts: Query<&mut Visibility, (Added<MsdfText>, Without<Node>)>,
) {
    for mut vis in new_texts.iter_mut() {
        *vis = Visibility::Hidden;
    }
}
