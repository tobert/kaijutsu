//! Timeline systems for temporal navigation.

use bevy::prelude::*;
use bevy::input::keyboard::{Key, KeyboardInput};

use super::components::*;
use crate::cell::{CellEditor, CurrentMode, MainCell, ViewingConversation};
use crate::connection::bridge::{ConnectionCommands, ConnectionCommand, ConnectionEvent};

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
// BLOCK VISIBILITY
// ============================================================================

/// Update block visibility based on timeline position.
///
/// Blocks created after the viewing position are hidden or dimmed.
/// This creates the visual "time travel" effect.
///
/// Note: This system only updates the TimelineVisibility component.
/// The actual opacity application happens in sync_block_cell_buffers
/// which reads the opacity and applies it to the text color.
pub fn update_block_visibility(
    timeline: Res<TimelineState>,
    mut block_query: Query<&mut TimelineVisibility>,
) {
    if !timeline.is_changed() {
        return;
    }

    for mut vis in block_query.iter_mut() {
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
    cmds: Res<ConnectionCommands>,
    conversation_query: Query<&ViewingConversation, With<MainCell>>,
) {
    for request in fork_reader.read() {
        info!(
            "Fork requested from version {} with name {:?}",
            request.from_version, request.name
        );

        // Get the current document ID from the conversation
        let document_id = if let Ok(viewing) = conversation_query.single() {
            viewing.conversation_id.clone()
        } else {
            warn!("No active conversation for fork");
            continue;
        };

        // Generate context name if not provided
        let context_name = request.name.clone().unwrap_or_else(|| {
            format!("fork-v{}", request.from_version)
        });

        // Send to server via connection bridge
        cmds.send(ConnectionCommand::ForkDocument {
            document_id,
            version: request.from_version,
            context_name,
        });
    }
}

/// Handle fork completion events from the connection bridge.
pub fn handle_fork_complete(
    mut events: MessageReader<ConnectionEvent>,
    mut result_writer: MessageWriter<ForkResult>,
) {
    for event in events.read() {
        if let ConnectionEvent::ForkComplete { success, context_name, error, .. } = event {
            result_writer.write(ForkResult {
                success: *success,
                context_id: context_name.clone(),
                error: error.clone(),
            });
        }
    }
}

/// Process cherry-pick requests.
pub fn process_cherry_pick_requests(
    mut pick_reader: MessageReader<CherryPickRequest>,
    cmds: Res<ConnectionCommands>,
) {
    for request in pick_reader.read() {
        info!(
            "Cherry-pick requested for block {:?} to context {}",
            request.block_id, request.target_context
        );

        // Send to server via connection bridge
        cmds.send(ConnectionCommand::CherryPickBlock {
            block_id: request.block_id.clone(),
            target_context: request.target_context.clone(),
        });
    }
}

/// Handle cherry-pick completion events from the connection bridge.
pub fn handle_cherry_pick_complete(
    mut events: MessageReader<ConnectionEvent>,
    mut result_writer: MessageWriter<CherryPickResult>,
) {
    for event in events.read() {
        if let ConnectionEvent::CherryPickComplete { success, new_block_id, error } = event {
            result_writer.write(CherryPickResult {
                success: *success,
                new_block_id: new_block_id.clone(),
                error: error.clone(),
            });
        }
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
