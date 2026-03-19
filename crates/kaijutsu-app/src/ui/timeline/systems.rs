//! Timeline systems for temporal navigation.

use bevy::prelude::*;

use super::components::*;
use crate::cell::{CellEditor, MainCell};
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
// CHERRY-PICK PROCESSING
// ============================================================================

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
        let block_id = request.block_id;
        let target = request.target_context.clone();
        bevy::tasks::IoTaskPool::get()
            .spawn(async move {
                let target_ctx_id = match kaijutsu_crdt::ContextId::parse(&target) {
                    Ok(id) => id,
                    Err(e) => {
                        let _ = tx.send(RpcResultMessage::CherryPicked {
                            success: false,
                            new_block_id: None,
                            error: Some(format!("Invalid target context ID '{}': {}", target, e)),
                        });
                        return;
                    }
                };
                match handle.cherry_pick_block(&block_id, target_ctx_id).await {
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
        if let RpcResultMessage::CherryPicked {
            success,
            new_block_id,
            error,
        } = event
        {
            result_writer.write(CherryPickResult {
                success: *success,
                new_block_id: *new_block_id,
                error: error.clone(),
            });
        }
    }
}

// Timeline keyboard navigation has moved to input::systems::handle_timeline.
// Toggle visibility is now via the TimelineToggle action (bound to 't').
