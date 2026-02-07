//! Cell plugin for Bevy.

use bevy::prelude::*;

// ============================================================================
// SYSTEM SETS - Execution Phases
// ============================================================================

/// SystemSets for organizing cell systems into execution phases.
///
/// These replace the fragile 40+ `.after()` chains with proper set-based ordering.
/// Systems within a set can still use internal `.after()` for fine-grained ordering.
///
/// Execution order:
/// 1. **Input** - Mode switching, key handling, click-to-focus
/// 2. **Sync** - Server events (BlockInserted, etc.), document sync
/// 3. **Spawn** - Entity spawning + ApplyDeferred command flush
/// 4. **Buffer** - Text buffer init/sync, highlighting
/// 5. **Layout** - Measure heights, scroll, position entities
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub enum CellPhase {
    /// Mode switching, key handling, click-to-focus
    Input,
    /// Server events, document sync
    Sync,
    /// Entity spawning + ApplyDeferred
    Spawn,
    /// Text buffer init/sync
    Buffer,
    /// Measure, scroll, position
    Layout,
}

use super::components::{
    BlockCellContainer, BlockCellLayout, BubbleConfig, BubblePosition,
    BubbleRegistry, BubbleSpawnContext, BubbleState, Cell, CellId, CellPosition, CellState,
    ContextSwitchRequested, ConversationContainer, ConversationScrollState, CurrentMode,
    DocumentCache, DocumentSyncState, EditorMode, FocusTarget, LayoutGeneration, MainCell,
    PromptSubmitted, RoleHeaderLayout, ViewingConversation, WorkspaceLayout,
};
use super::frame_assembly;
use super::systems;
use crate::dashboard::DashboardEventHandling;

/// Plugin that enables cell-based editing in the workspace.
pub struct CellPlugin;

impl Plugin for CellPlugin {
    fn build(&self, app: &mut App) {
        // Register messages
        app.add_message::<PromptSubmitted>()
            .add_message::<ContextSwitchRequested>();

        // Register types for BRP reflection
        app.register_type::<EditorMode>()
            .register_type::<CurrentMode>()
            .register_type::<ConversationScrollState>()
            .register_type::<ConversationContainer>()
            .register_type::<MainCell>()
            .register_type::<PromptSubmitted>()
            // Additional types for debugging
            .register_type::<CellId>()
            .register_type::<Cell>()
            .register_type::<ViewingConversation>()
            .register_type::<CellPosition>()
            .register_type::<CellState>()
            .register_type::<FocusTarget>()
            .register_type::<WorkspaceLayout>()
            .register_type::<BlockCellContainer>()
            .register_type::<BlockCellLayout>()
            .register_type::<RoleHeaderLayout>()
            // Bubble types
            .register_type::<BubbleState>()
            .register_type::<BubblePosition>()
            .register_type::<BubbleSpawnContext>()
            .register_type::<BubbleConfig>();

        // Configure SystemSet execution order
        app.configure_sets(
            Update,
            (
                CellPhase::Input,
                CellPhase::Sync.after(CellPhase::Input),
                CellPhase::Spawn.after(CellPhase::Sync),
                CellPhase::Buffer.after(CellPhase::Spawn),
                CellPhase::Layout.after(CellPhase::Buffer),
            ),
        );

        app.init_resource::<FocusTarget>()
            .init_resource::<CurrentMode>()
            .init_resource::<WorkspaceLayout>()
            .init_resource::<ConversationScrollState>()
            .init_resource::<LayoutGeneration>()
            .init_resource::<DocumentSyncState>()
            .init_resource::<DocumentCache>()
            .init_resource::<systems::EditorEntities>()
            .init_resource::<systems::ConsumedModeKeys>()
            // Bubble system resources
            .init_resource::<BubbleRegistry>()
            .init_resource::<BubbleConfig>();

        // ====================================================================
        // CellPhase::Input - Mode switching, key handling, click-to-focus
        // ====================================================================
        app.add_systems(
            Update,
            (
                // Clear consumed keys at start of frame before any input handling
                systems::clear_consumed_keys,
                // Block edit mode must run BEFORE mode_switch to intercept `i`
                // when a BlockCell is focused
                systems::handle_block_edit_mode
                    .after(systems::clear_consumed_keys)
                    .before(systems::handle_mode_switch),
                systems::handle_mode_switch.after(systems::clear_consumed_keys),
                // Block cell input runs after block edit mode
                systems::handle_block_cell_input.after(systems::handle_block_edit_mode),
                // Compose block input runs after mode switch
                systems::handle_compose_block_input.after(systems::handle_mode_switch),
                // Click to focus
                systems::click_to_focus,
                // Collapse/expand for thinking blocks (Tab in Normal mode)
                systems::handle_collapse_toggle,
                // Pop ViewStack with Esc (before mode switch handles Esc)
                systems::handle_view_pop.before(systems::handle_mode_switch),
                // Mobile bubble input systems
                systems::handle_bubble_spawn
                    .after(systems::clear_consumed_keys)
                    .before(systems::handle_mode_switch),
                systems::handle_bubble_navigation
                    .after(systems::handle_bubble_spawn)
                    .before(systems::handle_mode_switch),
                systems::handle_bubble_input
                    .after(systems::handle_mode_switch)
                    .before(systems::handle_cell_input),
                systems::handle_bubble_submit.after(systems::handle_bubble_input),
            )
                .in_set(CellPhase::Input),
        );

        // ====================================================================
        // CellPhase::Sync - Server events, document sync
        // ====================================================================
        app.add_systems(
            Update,
            (
                // Block event handling (server → client sync, routes through DocumentCache)
                // Must run AFTER DashboardEventHandling so SeatTaken creates conversation first
                systems::handle_block_events.after(DashboardEventHandling),
                // Context switching (reads ContextSwitchRequested, swaps active cache entry)
                systems::handle_context_switch.after(systems::handle_block_events),
                // Handle prompt submission
                systems::handle_prompt_submitted,
                // Sync main cell to conversation (after block events, context switch, and prompt submission)
                systems::sync_main_cell_to_conversation
                    .after(systems::handle_block_events)
                    .after(systems::handle_context_switch)
                    .after(systems::handle_prompt_submitted),
                // Block navigation (j/k) after sync
                systems::navigate_blocks.after(systems::sync_main_cell_to_conversation),
                // Expand block with `f` key
                systems::handle_expand_block.after(systems::navigate_blocks),
                // Scroll input after navigation
                systems::handle_scroll_input.after(systems::navigate_blocks),
            )
                .in_set(CellPhase::Sync),
        );

        // ====================================================================
        // CellPhase::Spawn - Entity spawning + ApplyDeferred
        // ====================================================================
        app.add_systems(
            Update,
            (
                // Main cell spawning
                systems::spawn_main_cell,
                // Block cell spawning (after sync)
                systems::spawn_block_cells,
                // Role header sync (after block cells)
                systems::sync_role_headers.after(systems::spawn_block_cells),
                // Expanded block view spawning
                systems::spawn_expanded_block_view,
                // Cursor spawning
                systems::spawn_cursor,
                // ApplyDeferred to flush spawn commands
                ApplyDeferred.after(systems::sync_role_headers),
            )
                .in_set(CellPhase::Spawn),
        );

        // ====================================================================
        // CellPhase::Buffer - Text buffer init/sync, highlighting
        // ====================================================================
        app.add_systems(
            Update,
            (
                // Cell buffer init and sync
                systems::init_cell_buffers,
                systems::sync_cell_buffers.after(systems::init_cell_buffers),
                systems::compute_cell_heights,
                // Block cell buffer init and sync
                systems::init_block_cell_buffers,
                systems::init_role_header_buffers,
                systems::sync_block_cell_buffers
                    .after(systems::init_block_cell_buffers)
                    .after(systems::init_role_header_buffers),
                // Compose block buffer init and sync
                systems::init_compose_block_buffer,
                systems::sync_compose_block_buffer.after(systems::init_compose_block_buffer),
                // Expanded block content sync
                systems::sync_expanded_block_content,
                // Bubble buffer init and sync
                systems::init_bubble_buffers,
                systems::sync_bubble_buffers.after(systems::init_bubble_buffers),
                // Highlighting (after buffer sync)
                systems::highlight_focused_cell.after(systems::sync_cell_buffers),
                systems::highlight_focused_block.after(systems::sync_block_cell_buffers),
            )
                .in_set(CellPhase::Buffer),
        );

        // ====================================================================
        // CellPhase::Layout - Measure heights, scroll, position entities
        // ====================================================================
        app.add_systems(
            Update,
            (
                // Block cell layout (computes heights, updates content_height)
                systems::layout_block_cells,
                // Smooth scroll (uses content_height from layout)
                systems::smooth_scroll.after(systems::layout_block_cells),
                // Apply positions (uses scroll offset)
                systems::apply_block_cell_positions.after(systems::smooth_scroll),
                // Compose block positioning (after Bevy's UI layout)
                systems::position_compose_block.after(bevy::ui::UiSystems::Prepare),
                // Cursor positioning
                systems::update_cursor,
                systems::update_block_edit_cursor.after(systems::update_cursor),
                // Bubble layout and cursor
                systems::layout_bubble_position,
                systems::update_bubble_cursor
                    .after(systems::layout_bubble_position)
                    .after(systems::update_cursor),
                systems::sync_bubble_visibility.after(systems::layout_bubble_position),
                // 9-slice frame layout
                frame_assembly::spawn_nine_slice_frames,
                frame_assembly::layout_nine_slice_frames,
                frame_assembly::update_nine_slice_state,
                frame_assembly::sync_frame_visibility,
                frame_assembly::cleanup_nine_slice_frames,
            )
                .in_set(CellPhase::Layout),
        );
    }
}
