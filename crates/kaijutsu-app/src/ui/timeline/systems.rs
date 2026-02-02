//! Timeline systems for temporal navigation.

use bevy::prelude::*;
use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::input::mouse::MouseButton;
use bevy::ui::ComputedNode;

use super::components::*;
use crate::cell::{CellEditor, CurrentMode, MainCell};
use crate::ui::theme::Theme;

// ============================================================================
// VERSION SYNC
// ============================================================================

/// Sync timeline state with document version.
///
/// When the document changes (new blocks, edits), update the timeline's
/// understanding of the current version.
pub fn sync_timeline_version(
    mut timeline: ResMut<TimelineState>,
    editor_query: Query<&CellEditor, With<MainCell>>,
) {
    // Get the current document version
    if let Ok(editor) = editor_query.single() {
        let doc_version = editor.version();
        if doc_version != timeline.current_version {
            timeline.sync_version(doc_version);
        }
    }
}

// ============================================================================
// KEYBOARD NAVIGATION
// ============================================================================

/// Handle keyboard shortcuts for timeline navigation.
///
/// - `[` - Step back in history
/// - `]` - Step forward in history
/// - `\` - Jump to live/now
/// - `Ctrl+F` - Fork from current position (when viewing history)
pub fn handle_timeline_keys(
    mut keyboard: MessageReader<KeyboardInput>,
    mode: Res<CurrentMode>,
    mut timeline: ResMut<TimelineState>,
    mut fork_writer: MessageWriter<ForkRequest>,
) {
    // Only in Normal mode
    if mode.0.accepts_input() {
        return;
    }

    for event in keyboard.read() {
        if !event.state.is_pressed() {
            continue;
        }

        match &event.logical_key {
            Key::Character(c) if c == "[" => {
                // Step back
                let step = 1.0 / (timeline.snapshot_count.max(1) as f32);
                let new_pos = (timeline.target_position - step).max(0.0);
                timeline.begin_scrub(new_pos);
                timeline.end_scrub();
            }
            Key::Character(c) if c == "]" => {
                // Step forward
                let step = 1.0 / (timeline.snapshot_count.max(1) as f32);
                let new_pos = (timeline.target_position + step).min(1.0);
                timeline.begin_scrub(new_pos);
                timeline.end_scrub();
            }
            Key::Character(c) if c == "\\" => {
                // Jump to now
                timeline.jump_to_live();
            }
            Key::Character(c) if c == "f" && timeline.is_historical() => {
                // Fork from current viewing position
                // TODO: Check for Ctrl modifier when we have proper modifier tracking
                fork_writer.write(ForkRequest {
                    from_version: timeline.viewing_version,
                    name: None,
                });
            }
            _ => {}
        }
    }
}

// ============================================================================
// MOUSE SCRUBBING
// ============================================================================

/// Handle mouse interaction with the timeline scrubber.
pub fn handle_timeline_mouse(
    mouse_buttons: Res<ButtonInput<MouseButton>>,
    mut timeline: ResMut<TimelineState>,
    track_query: Query<(&ComputedNode, &GlobalTransform), With<TimelineTrack>>,
    window_query: Query<&Window>,
) {
    // Get cursor position
    let Ok(window) = window_query.single() else {
        return;
    };
    let Some(cursor_pos) = window.cursor_position() else {
        return;
    };

    // Get track bounds
    let Ok((computed_node, transform)) = track_query.single() else {
        return;
    };

    let track_pos = transform.translation().truncate();
    let track_size = computed_node.size();

    // Check if cursor is within track bounds
    let track_left = track_pos.x - track_size.x / 2.0;
    let track_right = track_pos.x + track_size.x / 2.0;
    let track_top = track_pos.y - track_size.y / 2.0;
    let track_bottom = track_pos.y + track_size.y / 2.0;

    let in_track = cursor_pos.x >= track_left
        && cursor_pos.x <= track_right
        && cursor_pos.y >= track_top
        && cursor_pos.y <= track_bottom;

    // Handle click/drag
    if mouse_buttons.just_pressed(MouseButton::Left) && in_track {
        let relative_x = (cursor_pos.x - track_left) / track_size.x;
        timeline.begin_scrub(relative_x);
    } else if mouse_buttons.pressed(MouseButton::Left) && timeline.scrubbing {
        let relative_x = (cursor_pos.x - track_left) / track_size.x;
        timeline.update_scrub(relative_x);
    } else if mouse_buttons.just_released(MouseButton::Left) && timeline.scrubbing {
        timeline.end_scrub();
    }
}

// ============================================================================
// UI UPDATES
// ============================================================================

/// Update the playhead position based on timeline state.
pub fn update_playhead_position(
    timeline: Res<TimelineState>,
    mut playhead_query: Query<&mut Node, With<TimelinePlayhead>>,
    track_query: Query<&ComputedNode, (With<TimelineTrack>, Without<TimelinePlayhead>)>,
) {
    let Ok(track_computed) = track_query.single() else {
        return;
    };
    let track_width = track_computed.size().x;

    for mut node in playhead_query.iter_mut() {
        // Position playhead as percentage of track width
        node.left = Val::Px(timeline.position * track_width);
    }
}

/// Update the fill bar width based on timeline position.
pub fn update_fill_width(
    timeline: Res<TimelineState>,
    mut fill_query: Query<&mut Node, With<TimelineFill>>,
) {
    for mut node in fill_query.iter_mut() {
        node.width = Val::Percent(timeline.position * 100.0);
    }
}

/// Update block visibility based on timeline position.
///
/// Blocks created after the viewing position are hidden or dimmed.
/// This creates the visual "time travel" effect.
pub fn update_block_visibility(
    timeline: Res<TimelineState>,
    mut block_query: Query<(&mut TimelineVisibility, &mut BackgroundColor)>,
    theme: Res<Theme>,
) {
    if !timeline.is_changed() {
        return;
    }

    for (mut vis, mut bg) in block_query.iter_mut() {
        let is_past = vis.created_at_version > timeline.viewing_version;
        vis.is_past = is_past;

        // Calculate opacity based on distance from viewing position
        if timeline.is_live() {
            vis.opacity = 1.0;
        } else if is_past {
            // Future blocks (relative to viewing position) are dimmed
            vis.opacity = 0.3;
        } else {
            // Past and current blocks are fully visible
            vis.opacity = 1.0;
        }

        // Apply opacity to background
        let base_alpha = theme.panel_bg.alpha();
        bg.0 = bg.0.with_alpha(base_alpha * vis.opacity);
    }
}

// ============================================================================
// BUTTON HANDLERS
// ============================================================================

/// Handle Fork button clicks.
pub fn handle_fork_button(
    interactions: Query<&Interaction, (Changed<Interaction>, With<ForkButton>)>,
    timeline: Res<TimelineState>,
    mut fork_writer: MessageWriter<ForkRequest>,
) {
    for interaction in interactions.iter() {
        if *interaction == Interaction::Pressed {
            fork_writer.write(ForkRequest {
                from_version: timeline.viewing_version,
                name: None,
            });
        }
    }
}

/// Handle Jump to Now button clicks.
pub fn handle_jump_button(
    interactions: Query<&Interaction, (Changed<Interaction>, With<JumpToNowButton>)>,
    mut timeline: ResMut<TimelineState>,
) {
    for interaction in interactions.iter() {
        if *interaction == Interaction::Pressed {
            timeline.jump_to_live();
        }
    }
}

// ============================================================================
// FORK/CHERRY-PICK PROCESSING
// ============================================================================

/// Process fork requests.
///
/// This sends the fork request to the server via RPC.
/// The actual forking happens server-side.
pub fn process_fork_requests(
    mut fork_reader: MessageReader<ForkRequest>,
    mut result_writer: MessageWriter<ForkResult>,
    // TODO: Add RPC client connection
) {
    for request in fork_reader.read() {
        info!(
            "Fork requested from version {} with name {:?}",
            request.from_version, request.name
        );

        // TODO: Send to server via RPC
        // For now, emit a placeholder result
        result_writer.write(ForkResult {
            success: false,
            context_id: None,
            error: Some("Fork not yet implemented in RPC".to_string()),
        });
    }
}

/// Process cherry-pick requests.
pub fn process_cherry_pick_requests(
    mut pick_reader: MessageReader<CherryPickRequest>,
    mut result_writer: MessageWriter<CherryPickResult>,
    // TODO: Add RPC client connection
) {
    for request in pick_reader.read() {
        info!(
            "Cherry-pick requested for block {:?} to context {}",
            request.block_id, request.target_context
        );

        // TODO: Send to server via RPC
        result_writer.write(CherryPickResult {
            success: false,
            new_block_id: None,
            error: Some("Cherry-pick not yet implemented in RPC".to_string()),
        });
    }
}

// ============================================================================
// TOGGLE VISIBILITY
// ============================================================================

/// Toggle timeline visibility with `t` key.
pub fn toggle_timeline_visibility(
    mut keyboard: MessageReader<KeyboardInput>,
    mode: Res<CurrentMode>,
    mut timeline: ResMut<TimelineState>,
) {
    // Only in Normal mode
    if mode.0.accepts_input() {
        return;
    }

    for event in keyboard.read() {
        if !event.state.is_pressed() {
            continue;
        }

        if let Key::Character(c) = &event.logical_key {
            if c == "t" {
                timeline.expanded = !timeline.expanded;
            }
        }
    }
}
