//! Cell plugin for Bevy.

use bevy::prelude::*;

use super::components::{
    ConversationScrollState, CurrentMode, FocusedCell, PromptSubmitted, WorkspaceLayout,
};
use super::frame_assembly;
use super::frame_style::{FrameStyle, FrameStyleLoader, FrameStyleMapping};
use super::sync::{self, CellRegistry, PendingCellRegistrations, RecentlyDeletedByServer};
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
            .init_resource::<CellRegistry>()
            .init_resource::<CurrentMode>()
            .init_resource::<WorkspaceLayout>()
            .init_resource::<ConversationScrollState>()
            .init_resource::<RecentlyDeletedByServer>()
            .init_resource::<PendingCellRegistrations>()
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
            // Layout and rendering
            // sync_cell_buffers must run after:
            // - init_cell_buffers (TextBuffer exists)
            // - handle_cell_input (CellEditor updated with typed text)
            // - sync_main_cell_to_conversation (CellEditor updated from conversation)
            .add_systems(
                Update,
                (
                    systems::init_cell_buffers,
                    systems::compute_cell_heights,
                    systems::layout_main_cell,
                    systems::layout_prompt_cell_position,
                    systems::sync_cell_buffers
                        .after(systems::init_cell_buffers)
                        .after(systems::handle_cell_input)
                        .after(systems::sync_main_cell_to_conversation),
                    systems::highlight_focused_cell,
                ),
            )
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
            )
            // Remote sync (block-based)
            .add_systems(
                Update,
                (
                    sync::trigger_sync_on_attach,
                    sync::handle_cell_sync_result,
                    sync::handle_block_events,
                    sync::send_block_operations,
                    sync::create_remote_cell,
                    sync::delete_remote_cell,
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
