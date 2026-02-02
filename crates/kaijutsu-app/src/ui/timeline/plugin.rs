//! Timeline plugin - registers resources, systems, and UI.

use bevy::prelude::*;

use super::components::*;
use super::systems;
use crate::ui::state::AppScreen;
use crate::ui::theme::Theme;

/// Plugin that enables timeline navigation.
pub struct TimelinePlugin;

impl Plugin for TimelinePlugin {
    fn build(&self, app: &mut App) {
        // Register types for BRP reflection
        app.register_type::<TimelineState>()
            .register_type::<TimelineViewMode>()
            .register_type::<TimelineVisibility>();

        // Initialize resources
        app.init_resource::<TimelineState>();

        // Register messages
        app.add_message::<ForkRequest>()
            .add_message::<ForkResult>()
            .add_message::<CherryPickRequest>()
            .add_message::<CherryPickResult>();

        // Spawn UI on entering Conversation screen
        app.add_systems(OnEnter(AppScreen::Conversation), spawn_timeline_ui);

        // Core systems (only run in Conversation state)
        app.add_systems(
            Update,
            (
                // Version sync first
                systems::sync_timeline_version,
                // Input handling
                systems::handle_timeline_keys,
                systems::handle_timeline_mouse,
                systems::toggle_timeline_visibility,
                // UI updates
                systems::update_playhead_position,
                systems::update_fill_width,
                systems::update_block_visibility,
                // Button handlers
                systems::handle_fork_button,
                systems::handle_jump_button,
                // Request processing
                systems::process_fork_requests,
                systems::process_cherry_pick_requests,
            )
                .run_if(in_state(AppScreen::Conversation)),
        );

        // Visibility sync
        app.add_systems(Update, sync_timeline_ui_visibility);
    }
}

/// Spawn the timeline scrubber UI.
fn spawn_timeline_ui(mut commands: Commands, theme: Res<Theme>) {
    // Timeline container - horizontal bar at bottom of content area
    commands
        .spawn((
            TimelineScrubber,
            Name::new("TimelineScrubber"),
            Node {
                position_type: PositionType::Absolute,
                bottom: Val::Px(60.0), // Above the input area
                left: Val::Px(0.0),
                width: Val::Percent(100.0),
                height: Val::Px(32.0),
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::Center,
                padding: UiRect::horizontal(Val::Px(16.0)),
                ..default()
            },
            BackgroundColor(theme.panel_bg.with_alpha(0.9)),
            ZIndex(50), // Above content, below modals
        ))
        .with_children(|parent| {
            // Timeline track (draggable area)
            parent
                .spawn((
                    TimelineTrack,
                    Name::new("TimelineTrack"),
                    Node {
                        flex_grow: 1.0,
                        height: Val::Px(8.0),
                        margin: UiRect::horizontal(Val::Px(8.0)),
                        border_radius: BorderRadius::all(Val::Px(4.0)),
                        ..default()
                    },
                    BackgroundColor(theme.fg_dim.with_alpha(0.3)),
                ))
                .with_children(|track| {
                    // Fill bar (shows progress)
                    track.spawn((
                        TimelineFill,
                        Name::new("TimelineFill"),
                        Node {
                            height: Val::Percent(100.0),
                            width: Val::Percent(100.0), // Updated by system
                            border_radius: BorderRadius::all(Val::Px(4.0)),
                            ..default()
                        },
                        BackgroundColor(theme.accent.with_alpha(0.6)),
                    ));

                    // Playhead (current position indicator)
                    track.spawn((
                        TimelinePlayhead,
                        Name::new("TimelinePlayhead"),
                        Node {
                            position_type: PositionType::Absolute,
                            width: Val::Px(4.0),
                            height: Val::Px(16.0),
                            top: Val::Px(-4.0),
                            left: Val::Percent(100.0), // Updated by system
                            border_radius: BorderRadius::all(Val::Px(2.0)),
                            ..default()
                        },
                        BackgroundColor(theme.accent),
                    ));
                });

            // Fork button (visible when viewing history)
            parent.spawn((
                ForkButton,
                Name::new("ForkButton"),
                Button,
                Node {
                    padding: UiRect::axes(Val::Px(12.0), Val::Px(4.0)),
                    margin: UiRect::left(Val::Px(8.0)),
                    border_radius: BorderRadius::all(Val::Px(4.0)),
                    ..default()
                },
                BackgroundColor(theme.accent2.with_alpha(0.8)),
                Visibility::Hidden, // Shown when viewing history
            ));

            // Jump to Now button
            parent.spawn((
                JumpToNowButton,
                Name::new("JumpToNowButton"),
                Button,
                Node {
                    padding: UiRect::axes(Val::Px(12.0), Val::Px(4.0)),
                    margin: UiRect::left(Val::Px(8.0)),
                    border_radius: BorderRadius::all(Val::Px(4.0)),
                    ..default()
                },
                BackgroundColor(theme.row_result.with_alpha(0.8)),
                Visibility::Hidden, // Shown when viewing history
            ));
        });
}

/// Sync timeline UI visibility based on TimelineState.
fn sync_timeline_ui_visibility(
    timeline: Res<TimelineState>,
    mut scrubber_query: Query<&mut Visibility, With<TimelineScrubber>>,
    mut fork_btn_query: Query<
        &mut Visibility,
        (With<ForkButton>, Without<TimelineScrubber>, Without<JumpToNowButton>),
    >,
    mut jump_btn_query: Query<
        &mut Visibility,
        (With<JumpToNowButton>, Without<TimelineScrubber>, Without<ForkButton>),
    >,
) {
    // Show/hide entire scrubber based on expanded state
    for mut vis in scrubber_query.iter_mut() {
        *vis = if timeline.expanded {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
    }

    // Show/hide buttons based on whether we're viewing history
    let show_buttons = timeline.is_historical();
    for mut vis in fork_btn_query.iter_mut() {
        *vis = if show_buttons {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
    }
    for mut vis in jump_btn_query.iter_mut() {
        *vis = if show_buttons {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
    }
}
