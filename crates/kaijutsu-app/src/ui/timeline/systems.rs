//! Timeline systems for temporal navigation.

use bevy::prelude::*;
use bevy::input::keyboard::{Key, KeyboardInput};

use super::components::*;
use crate::cell::{CellEditor, MainCell, ViewingConversation};
use crate::input::FocusArea;
use crate::connection::{RpcActor, RpcResultChannel, RpcResultMessage};

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
    focus_area: Res<FocusArea>,
    mut timeline: ResMut<TimelineState>,
    mut fork_writer: MessageWriter<ForkRequest>,
) {
    // Only when not typing text
    if focus_area.is_text_input() {
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
/// Sends the fork request to the server via ActorHandle async task.
/// The actual forking happens server-side; results arrive via RpcResultChannel.
pub fn process_fork_requests(
    mut fork_reader: MessageReader<ForkRequest>,
    actor: Option<Res<RpcActor>>,
    channel: Res<RpcResultChannel>,
    conversation_query: Query<&ViewingConversation, With<MainCell>>,
) {
    let Some(actor) = actor else { return };

    for request in fork_reader.read() {
        info!(
            "Fork requested from version {} with name {:?}",
            request.from_version, request.name
        );

        let document_id = if let Ok(viewing) = conversation_query.single() {
            viewing.conversation_id.clone()
        } else {
            warn!("No active conversation for fork");
            continue;
        };

        let context_name = request.name.clone().unwrap_or_else(|| {
            format!("fork-v{}", request.from_version)
        });

        let handle = actor.handle.clone();
        let tx = channel.sender();
        let version = request.from_version;
        let doc_id = document_id.clone();
        let ctx_name = context_name.clone();
        bevy::tasks::IoTaskPool::get()
            .spawn(async move {
                match handle.fork_from_version(&doc_id, version, &ctx_name).await {
                    Ok(context) => {
                        let new_doc_id = format!(
                            "{}@{}",
                            doc_id.split('@').next().unwrap_or(&doc_id),
                            context.name
                        );
                        let _ = tx.send(RpcResultMessage::Forked {
                            success: true,
                            context_name: Some(context.name),
                            document_id: Some(new_doc_id),
                            error: None,
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(RpcResultMessage::Forked {
                            success: false,
                            context_name: None,
                            document_id: None,
                            error: Some(e.to_string()),
                        });
                    }
                }
            })
            .detach();
    }
}

/// Handle fork completion results from async RPC tasks.
pub fn handle_fork_complete(
    mut events: MessageReader<RpcResultMessage>,
    mut result_writer: MessageWriter<ForkResult>,
) {
    for event in events.read() {
        if let RpcResultMessage::Forked { success, context_name, error, .. } = event {
            result_writer.write(ForkResult {
                success: *success,
                context_id: context_name.clone(),
                error: error.clone(),
            });
        }
    }
}

/// Process cherry-pick requests via ActorHandle async task.
pub fn process_cherry_pick_requests(
    mut pick_reader: MessageReader<CherryPickRequest>,
    actor: Option<Res<RpcActor>>,
    channel: Res<RpcResultChannel>,
) {
    let Some(actor) = actor else { return };

    for request in pick_reader.read() {
        info!(
            "Cherry-pick requested for block {:?} to context {}",
            request.block_id, request.target_context
        );

        let handle = actor.handle.clone();
        let tx = channel.sender();
        let block_id = request.block_id.clone();
        let target = request.target_context.clone();
        bevy::tasks::IoTaskPool::get()
            .spawn(async move {
                match handle.cherry_pick_block(&block_id, &target).await {
                    Ok(new_id) => {
                        let _ = tx.send(RpcResultMessage::CherryPicked {
                            success: true,
                            new_block_id: Some(new_id),
                            error: None,
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(RpcResultMessage::CherryPicked {
                            success: false,
                            new_block_id: None,
                            error: Some(e.to_string()),
                        });
                    }
                }
            })
            .detach();
    }
}

/// Handle cherry-pick completion results from async RPC tasks.
pub fn handle_cherry_pick_complete(
    mut events: MessageReader<RpcResultMessage>,
    mut result_writer: MessageWriter<CherryPickResult>,
) {
    for event in events.read() {
        if let RpcResultMessage::CherryPicked { success, new_block_id, error } = event {
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
    focus_area: Res<FocusArea>,
    mut timeline: ResMut<TimelineState>,
) {
    // Only when not typing text
    if focus_area.is_text_input() {
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
