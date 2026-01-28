//! Cell plugin for Bevy.

use bevy::prelude::*;

use super::components::{
    ConversationContainer, ConversationFocus, ConversationScrollState, CurrentMode,
    DocumentSyncState, EditorMode, FocusedCell, LayoutGeneration, MainCell, PromptCell,
    PromptContainer, PromptSubmitted, WorkspaceLayout,
};
use super::frame_assembly;
use super::systems;
use crate::dashboard::DashboardEventHandling;
use crate::ui::state::{InputDock, InputPosition, InputPresence, InputShadowHeight};

/// Plugin that enables cell-based editing in the workspace.
pub struct CellPlugin;

impl Plugin for CellPlugin {
    fn build(&self, app: &mut App) {
        // Register messages
        app.add_message::<PromptSubmitted>();

        // Register types for BRP reflection
        app.register_type::<EditorMode>()
            .register_type::<CurrentMode>()
            .register_type::<ConversationScrollState>()
            .register_type::<ConversationContainer>()
            .register_type::<PromptContainer>()
            .register_type::<PromptCell>()
            .register_type::<MainCell>()
            .register_type::<PromptSubmitted>();

        app.init_resource::<FocusedCell>()
            .init_resource::<CurrentMode>()
            .init_resource::<WorkspaceLayout>()
            .init_resource::<ConversationScrollState>()
            .init_resource::<ConversationFocus>()
            .init_resource::<LayoutGeneration>()
            .init_resource::<DocumentSyncState>()
            .init_resource::<systems::CursorEntity>()
            .init_resource::<systems::ConsumedModeKeys>()
            .init_resource::<systems::PromptCellEntity>()
            .init_resource::<systems::MainCellEntity>()
            .init_resource::<systems::ExpandedBlockEntity>()
            // Input area state resources
            .init_resource::<InputPresence>()
            .init_resource::<InputDock>()
            .init_resource::<InputPosition>()
            .init_resource::<InputShadowHeight>()
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
                    // Block navigation (j/k) runs before scroll_input
                    systems::navigate_blocks
                        .after(systems::sync_main_cell_to_conversation),
                    // Expand block with `f` key
                    systems::handle_expand_block
                        .after(systems::navigate_blocks),
                    // Pop ViewStack with Esc (before mode switch handles Esc)
                    systems::handle_view_pop
                        .before(systems::handle_mode_switch),
                    // scroll_input sets target, but smooth_scroll must run AFTER layout
                    // (layout updates content_height, smooth_scroll depends on it)
                    systems::handle_scroll_input
                        .after(systems::navigate_blocks),
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
                    systems::layout_prompt_cell_position,
                    systems::sync_cell_buffers
                        .after(systems::init_cell_buffers)
                        .after(systems::handle_cell_input)
                        .after(systems::sync_main_cell_to_conversation),
                    systems::highlight_focused_cell,
                ),
            )
            // Block event handling (server → client sync)
            // Receives block events from the server and updates the Conversation registry.
            // Must run:
            // - AFTER DashboardEventHandling so SeatTaken creates conversation first
            // - BEFORE sync_main_cell_to_conversation so sync sees updated version
            .add_systems(
                Update,
                systems::handle_block_events
                    .after(DashboardEventHandling)
                    .before(systems::sync_main_cell_to_conversation),
            )
            // Block cell systems (per-block UI rendering for conversation)
            // Each block gets its own entity with independent GlyphonTextBuffer
            //
            // Critical ordering for smooth scroll:
            //   1. layout_block_cells → computes heights, updates content_height
            //   2. smooth_scroll → updates offset using new content_height
            //   3. apply_block_cell_positions → positions blocks using new offset
            // Block cell systems with explicit command flush between spawn and init.
            // spawn_block_cells uses deferred commands, so we need ApplyDeferred
            // before init_block_cell_buffers can see the new entities.
            .add_systems(
                Update,
                (
                    systems::spawn_block_cells
                        .after(systems::handle_block_events)
                        .after(systems::sync_main_cell_to_conversation),
                    systems::sync_role_headers
                        .after(systems::spawn_block_cells),
                    ApplyDeferred
                        .after(systems::sync_role_headers),
                    systems::init_block_cell_buffers
                        .after(ApplyDeferred),
                    systems::init_role_header_buffers
                        .after(ApplyDeferred),
                    systems::sync_block_cell_buffers
                        .after(systems::init_block_cell_buffers)
                        .after(systems::init_role_header_buffers)
                        .after(systems::handle_cell_input),
                    // Highlight focused block (overrides color set by sync_block_cell_buffers)
                    systems::highlight_focused_block
                        .after(systems::sync_block_cell_buffers),
                    systems::layout_block_cells
                        .after(systems::highlight_focused_block),
                    systems::smooth_scroll
                        .after(systems::layout_block_cells),
                    systems::apply_block_cell_positions
                        .after(systems::smooth_scroll),
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
            // Expanded block view (Phase 4)
            .add_systems(
                Update,
                (
                    systems::spawn_expanded_block_view,
                    systems::sync_expanded_block_content
                        .after(systems::spawn_expanded_block_view),
                ),
            )
            // 9-slice frame system
            .add_systems(
                Update,
                (
                    frame_assembly::spawn_nine_slice_frames,
                    frame_assembly::layout_nine_slice_frames,
                    frame_assembly::update_nine_slice_state,
                    frame_assembly::sync_frame_visibility,
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
            // Input area positioning and visibility
            .add_systems(
                Update,
                (
                    systems::sync_presence_with_screen,
                    systems::compute_input_position.after(systems::sync_presence_with_screen),
                    systems::sync_input_layer_visibility.after(systems::compute_input_position),
                    systems::sync_backdrop_visibility.after(systems::compute_input_position),
                    systems::apply_input_position.after(systems::compute_input_position),
                    systems::sync_input_shadow_height.after(systems::sync_presence_with_screen),
                    // PromptCell visibility syncs with presence to hide glyphon text on Dashboard
                    systems::sync_prompt_visibility.after(systems::sync_presence_with_screen),
                ),
            );
    }
}
