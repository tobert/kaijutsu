//! Cell plugin for Bevy.

use bevy::prelude::*;

use super::components::{
    ConversationScrollState, CurrentMode, FocusedCell, PromptSubmitted, WorkspaceLayout,
};
use super::frame_assembly;
use super::frame_style::{FrameStyle, FrameStyleLoader, FrameStyleMapping};
use super::systems;

/// Plugin that enables cell-based editing in the workspace.
pub struct CellPlugin;

impl Plugin for CellPlugin {
    fn build(&self, app: &mut App) {
        // Register FrameStyle asset type and loader
        app.init_asset::<FrameStyle>()
            .init_asset_loader::<FrameStyleLoader>();

        // Register messages
        app.add_message::<PromptSubmitted>();

        app.init_resource::<FocusedCell>()
            .init_resource::<CurrentMode>()
            .init_resource::<WorkspaceLayout>()
            .init_resource::<ConversationScrollState>()
            .init_resource::<systems::CursorEntity>()
            .init_resource::<systems::ConsumedModeKeys>()
            .init_resource::<systems::PromptCellEntity>()
            .init_resource::<systems::MainCellEntity>()
            // Input and mode handling (mode_switch must run before cell_input)
            .add_systems(
                Update,
                (
                    systems::handle_mode_switch,
                    // Auto-focus prompt when entering INSERT mode (after mode switch, before input)
                    systems::auto_focus_prompt.after(systems::handle_mode_switch),
                    // Prompt submit must run before cell_input to intercept Enter in prompt
                    systems::handle_prompt_submit.after(systems::auto_focus_prompt),
                    systems::handle_cell_input.after(systems::handle_prompt_submit),
                    systems::click_to_focus,
                    systems::debug_spawn_cell,
                ),
            )
            // Main cell and prompt cell management
            .add_systems(
                Update,
                (
                    systems::spawn_main_cell,
                    systems::spawn_prompt_cell,
                    systems::handle_prompt_submitted,
                    systems::sync_main_cell_to_conversation
                        .after(systems::handle_prompt_submitted),
                    systems::scroll_to_bottom,
                    systems::handle_scroll_input,
                ),
            )
            // Layout and rendering for PromptCell
            // NOTE: MainCell no longer uses legacy rendering - BlockCell system handles it
            // These systems now only affect PromptCell (the input area at bottom)
            .add_systems(
                Update,
                (
                    systems::init_cell_buffers,
                    systems::compute_cell_heights,
                    // layout_main_cell is dead code - MainCell has no TextAreaConfig
                    // Kept for now in case we need it for something else
                    systems::layout_main_cell,
                    systems::layout_prompt_cell_position,
                    systems::sync_cell_buffers
                        .after(systems::init_cell_buffers)
                        .after(systems::handle_cell_input)
                        .after(systems::sync_main_cell_to_conversation),
                    systems::highlight_focused_cell,
                ),
            )
            // Block event handling (server → client sync)
            // Receives block events from the server and updates the MainCell's document
            .add_systems(
                Update,
                systems::handle_block_events
                    .after(systems::sync_main_cell_to_conversation),
            )
            // Block cell systems (per-block UI rendering for conversation)
            // Each block gets its own entity with independent TextBuffer
            .add_systems(
                Update,
                (
                    systems::spawn_block_cells
                        .after(systems::handle_block_events),
                    systems::init_block_cell_buffers
                        .after(systems::spawn_block_cells),
                    systems::sync_block_cell_buffers
                        .after(systems::init_block_cell_buffers)
                        .after(systems::handle_cell_input),
                    systems::layout_block_cells
                        .after(systems::sync_block_cell_buffers),
                    systems::apply_block_cell_positions
                        .after(systems::layout_block_cells),
                ),
            )
            // NOTE: Turn header systems removed in DAG migration.
            // Role transitions are now computed inline in layout_block_cells.
            // See systems.rs for details on the new role-based layout.
            // Collapse/expand for thinking blocks
            .add_systems(
                Update,
                systems::handle_collapse_toggle,
            )
            // 9-slice frame system
            .add_systems(
                Update,
                (
                    frame_assembly::spawn_nine_slice_frames,
                    frame_assembly::layout_nine_slice_frames,
                    frame_assembly::update_nine_slice_state,
                    frame_assembly::cleanup_nine_slice_frames,
                ),
            )
            // Cursor
            .add_systems(
                Update,
                (
                    systems::spawn_cursor,
                    systems::update_cursor,
                ),
            );
    }
}

/// System to load frame styles and initialize FrameStyleMapping resource.
pub fn setup_frame_styles(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
) {
    // Load frame styles
    let cyberpunk: Handle<FrameStyle> = asset_server.load("frames/cyberpunk.frame.ron");
    let minimal: Handle<FrameStyle> = asset_server.load("frames/minimal.frame.ron");

    // Set up style mapping - different cell types get different styles
    commands.insert_resource(FrameStyleMapping {
        code: cyberpunk.clone(),
        output: minimal.clone(),
        markdown: cyberpunk.clone(),
        system: minimal.clone(),
        user_message: cyberpunk.clone(),
        agent_message: cyberpunk.clone(),
        default: cyberpunk,
    });
}
