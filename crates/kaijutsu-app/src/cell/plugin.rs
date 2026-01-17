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
            .init_resource::<systems::DragState>()
            .init_resource::<systems::CollapsedParentsCache>()
            .init_resource::<RecentlyDeletedByServer>()
            .init_resource::<PendingCellRegistrations>()
            .init_resource::<systems::CursorEntity>()
            .init_resource::<systems::ConsumedModeKeys>()
            .init_resource::<systems::PromptCellEntity>()
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
            // Prompt cell and conversation management
            .add_systems(
                Update,
                (
                    systems::spawn_prompt_cell,
                    systems::handle_prompt_submitted,
                    systems::scroll_to_bottom,
                    systems::handle_scroll_input,
                ),
            )
            // Layout and rendering
            // sync_cell_buffers must run after:
            // - init_cell_buffers (TextBuffer exists)
            // - handle_cell_input (CellEditor updated with typed text)
            .add_systems(
                Update,
                (
                    systems::init_cell_buffers,
                    systems::compute_cell_heights,
                    systems::layout_cells,
                    systems::layout_prompt_cell_position,
                    systems::sync_cell_buffers
                        .after(systems::init_cell_buffers)
                        .after(systems::handle_cell_input),
                    systems::highlight_focused_cell,
                ),
            )
            // Collapse/expand and drag
            .add_systems(
                Update,
                (
                    systems::handle_collapse_toggle,
                    systems::update_collapsed_cache,
                    systems::apply_collapse_visibility,
                    systems::start_cell_drag,
                    systems::update_cell_drag,
                ),
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
            // Remote sync
            .add_systems(
                Update,
                (
                    sync::trigger_sync_on_attach,
                    sync::handle_cell_sync_result,
                    sync::send_cell_operations,
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
