//! Screen state machine — the single authority for which full-viewport view is active.
//!
//! Screen transitions control conversation visibility and focus. UI screens
//! use the theme background; Room and Heroic own their scene backgrounds.
//!
//! ## Design
//!
//! The conversation's content is a single composited surface
//! (`view::surface`) hanging off `ConversationRoot` — hiding/showing that one
//! root is enough to hide/show the whole conversation, since Bevy UI
//! `Visibility::Hidden` propagates to every descendant.

use bevy::prelude::*;

use super::state::ConversationRoot;

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
    /// (`view::room`). Reached by Up-Up at the well's mouth ring;
    /// Left/Right cycle stations, Enter/Down dives, Esc drops to Conversation.
    /// A bounded station is reached and left WITHOUT a further screen
    /// transition — "diving" into it is a camera pose plus a
    /// `view::room::RoomState::zoomed` write, all still `Screen::Room`
    /// ("diving IS fullscreening a panel").
    Room,
    /// The full diff viewer — an MSDF panel over a **frozen** `DiffCore`
    /// (`view::diff_view`). Full-viewport, chrome hidden, exactly like
    /// `Editor`; entered by `Action::OpenDiffViewer` on a focused diff block
    /// in the conversation, left by `q`/`ZQ`/`:q` (never by Esc — the vi
    /// surface owns Esc while it is live).
    Diff,
    /// The heroic view — full-window 3D anticipation & commitment road
    /// (`docs/newui.md`, `view::heroic`). Reached by `Ctrl+A k`, left by `Esc`.
    Heroic,
    /// The boards view.
    Boards,
}

/// Plugin that registers the Screen state and its transition systems.
pub struct ScreenPlugin;

impl Plugin for ScreenPlugin {
    fn build(&self, app: &mut App) {
        // `State`/`NextState` are registered so BRP can read the screen and
        // request a transition (`world.insert_resources` on `NextState<Screen>`
        // with `{"Pending": "Room"}`), the only way into a screen no key
        // reaches.
        app.init_state::<Screen>()
            .register_type::<Screen>()
            .register_type::<State<Screen>>()
            .register_type::<NextState<Screen>>()
            .add_systems(PostUpdate, sync_ui_camera_background);

        // ── Conversation ──
        app.add_systems(
            OnEnter(Screen::Conversation),
            (show_conversation_root, set_focus_conversation),
        );

        // ── Editor ──
        // Like the time well, the editor owns the viewport: hide the conversation
        // chrome on enter. Its MSDF panel + camera are managed by the editor
        // plugin (docs/vi.md step 4). Returning to Conversation re-shows the
        // chrome via the OnEnter(Conversation) systems above.
        //
        // Move focus off Compose: the editor forwards raw keystrokes itself
        // (`view::editor::editor_dispatch_keys`). The editor screen holds its
        // own grab, but a focus left on Compose would bring the chat overlay
        // back the moment the editor closes.
        app.add_systems(
            OnEnter(Screen::Editor),
            (hide_conversation_root, set_focus_conversation),
        );

        // ── Diff ──
        // The full diff viewer (`view::diff_view`): full-viewport MSDF panel
        // over a frozen model, owning the keyboard as an explicit grab —
        // same chrome/focus treatment as the editor above, and for the same
        // reasons (the grab forwards every key itself; leaving focus on
        // Compose would double-apply them to the hidden chat overlay).
        app.add_systems(
            OnEnter(Screen::Diff),
            (hide_conversation_root, set_focus_conversation),
        );

        // ── Room ──
        // The scenes-charter view (docs/scenes/): full-viewport 3D like the
        // well, reading raw keys — hide the chrome and park focus off Compose
        // (same reasoning as the editor above). Covers the whole shell,
        // zoomed into a station or not (`view::room::RoomState::zoomed`) —
        // there is no second screen for a station dive to enter any more.
        app.add_systems(
            OnEnter(Screen::Room),
            (hide_conversation_root, set_focus_conversation),
        );

        app.add_systems(
            OnEnter(Screen::Boards),
            (hide_conversation_root, set_focus_conversation),
        );

        // ── Heroic ──
        // Full-viewport 3D view (`view::heroic`): hide conversation chrome and
        // park focus off Compose so keys route to `InputContext::Heroic`.
        app.add_systems(
            OnEnter(Screen::Heroic),
            (hide_conversation_root, set_focus_conversation),
        );
    }
}

fn sync_ui_camera_background(
    theme: Res<super::theme::Theme>,
    screen: Res<State<Screen>>,
    mut cameras: Query<&mut Camera, With<IsDefaultUiCamera>>,
) {
    if !matches!(screen.get(), Screen::Conversation | Screen::Editor | Screen::Diff)
        || (!theme.is_changed() && !screen.is_changed())
    {
        return;
    }
    for mut camera in &mut cameras {
        camera.clear_color = ClearColorConfig::Custom(theme.bg);
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

/// Hide the conversation root when leaving for a full-viewport screen.
fn hide_conversation_root(mut roots: Query<&mut Visibility, With<ConversationRoot>>) {
    for mut vis in roots.iter_mut() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::Theme;

    fn app(screen: Screen) -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins(bevy::state::app::StatesPlugin);
        app.init_resource::<Theme>();
        app.init_resource::<crate::input::focus::FocusArea>();
        app.add_plugins(ScreenPlugin);
        app.insert_state(screen);
        let camera = app.world_mut().spawn((Camera3d::default(), IsDefaultUiCamera, Camera {
            clear_color: ClearColorConfig::Custom(Color::BLACK),
            ..default()
        })).id();
        app.update();
        (app, camera)
    }

    fn background(app: &App, camera: Entity) -> Color {
        match app.world().get::<Camera>(camera).unwrap().clear_color {
            ClearColorConfig::Custom(color) => color,
            other => panic!("expected a custom background, got {other:?}"),
        }
    }

    #[test]
    fn ui_camera_background_follows_theme_reload() {
        for screen in [Screen::Conversation, Screen::Editor, Screen::Diff] {
            let (mut app, camera) = app(screen);
            let color = Color::srgb(0.1, 0.2, 0.3);
            app.world_mut().resource_mut::<Theme>().bg = color;
            app.update();
            assert_eq!(background(&app, camera), color);
        }
    }

    #[test]
    fn scene_background_is_preserved_until_returning_to_ui() {
        for screen in [Screen::Room, Screen::Heroic] {
            let (mut app, camera) = app(screen);
            let color = Color::srgb(0.1, 0.2, 0.3);
            app.world_mut().resource_mut::<Theme>().bg = color;
            app.update();
            assert_eq!(background(&app, camera), Color::BLACK);
            app.world_mut().resource_mut::<NextState<Screen>>().set(Screen::Conversation);
            app.update();
            assert_eq!(background(&app, camera), color);
        }
    }
}
