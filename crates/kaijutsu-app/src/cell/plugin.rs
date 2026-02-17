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

use crate::ui::tiling_reconciler::TilingPhase;
use super::components::{
    BlockCellContainer, BlockCellLayout, Cell, CellId, CellPosition, CellState,
    ContextSwitchRequested, ConversationContainer, ConversationScrollState,
    DocumentCache, DocumentSyncState, FocusTarget, LayoutGeneration, MainCell,
    PendingContextSwitch, PromptSubmitted, RoleHeaderLayout, ViewingConversation, WorkspaceLayout,
};
use super::block_border;
use super::frame_assembly;
use super::systems;

/// Plugin that enables cell-based editing in the workspace.
pub struct CellPlugin;

impl Plugin for CellPlugin {
    fn build(&self, app: &mut App) {
        // Register messages
        app.add_message::<PromptSubmitted>()
            .add_message::<ContextSwitchRequested>();

        // Register types for BRP reflection
        app.register_type::<ConversationScrollState>()
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
            .register_type::<block_border::BlockBorderStyle>();

        // Configure SystemSet execution order
        app.configure_sets(
            Update,
            (
                // Cell systems run after tiling reconciler has spawned/updated entities
                CellPhase::Input.after(TilingPhase::PostReconcile),
                CellPhase::Sync.after(CellPhase::Input),
                CellPhase::Spawn.after(CellPhase::Sync),
                CellPhase::Buffer.after(CellPhase::Spawn),
                CellPhase::Layout.after(CellPhase::Buffer),
            ),
        );

        app.init_resource::<FocusTarget>()
            .init_resource::<WorkspaceLayout>()
            .init_resource::<ConversationScrollState>()
            .init_resource::<LayoutGeneration>()
            .init_resource::<DocumentSyncState>()
            .init_resource::<DocumentCache>()
            .init_resource::<PendingContextSwitch>()
            .init_resource::<systems::EditorEntities>();

        // ====================================================================
        // CellPhase::Input — click-to-focus only
        // All keyboard input now handled by InputPlugin (input/ module).
        // ====================================================================
        app.add_systems(
            Update,
            (
                systems::click_to_focus,
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
                systems::handle_block_events,
                // Context switching (reads ContextSwitchRequested, swaps active cache entry)
                systems::handle_context_switch.after(systems::handle_block_events),
                // Handle prompt submission
                systems::handle_prompt_submitted,
                // Sync main cell to conversation (after block events, context switch, and prompt submission)
                systems::sync_main_cell_to_conversation
                    .after(systems::handle_block_events)
                    .after(systems::handle_context_switch)
                    .after(systems::handle_prompt_submitted),
                // Staleness detection (after block events and context switch)
                systems::check_cache_staleness
                    .after(systems::handle_block_events)
                    .after(systems::handle_context_switch),
                // Block navigation, expand, scroll handled by InputPlugin
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
                // Track focused pane and re-parent block cells after split
                systems::track_conversation_container.after(systems::spawn_main_cell),
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
                // Highlighting (after buffer sync)
                systems::highlight_focused_cell.after(systems::sync_cell_buffers),
                systems::highlight_focused_block.after(systems::sync_block_cell_buffers),
                // Block border style determination (after buffer sync)
                block_border::determine_block_border_style
                    .after(systems::sync_block_cell_buffers),
            )
                .in_set(CellPhase::Buffer),
        );

        // ====================================================================
        // CellPhase::Layout - Measure heights, scroll, position entities
        // ====================================================================
        // Layout phase part 1: measure, scroll, position
        app.add_systems(
            Update,
            (
                // Block cell layout (computes heights, updates content_height)
                systems::layout_block_cells,
                // Sync heights to Node for flex layout
                systems::update_block_cell_nodes.after(systems::layout_block_cells),
                // Reorder children to match document order
                systems::reorder_conversation_children.after(systems::update_block_cell_nodes),
                // Smooth scroll (uses content_height from layout)
                systems::smooth_scroll.after(systems::layout_block_cells),
                // (Legacy apply_block_cell_positions removed — flex positioning takes over)
                // Flex-based positioning (after Bevy's UI layout)
                systems::position_block_cells_from_flex.after(bevy::ui::UiSystems::Prepare),
                systems::position_role_headers_from_flex.after(bevy::ui::UiSystems::Prepare),
                // Compose block positioning (after Bevy's UI layout)
                systems::position_compose_block.after(bevy::ui::UiSystems::Prepare),
                // Cursor positioning
                systems::update_cursor,
                systems::update_block_edit_cursor.after(systems::update_cursor),
                // Compose block cursor (Input mode, no inline edit active)
                systems::update_compose_cursor
                    .after(systems::update_cursor)
                    .after(systems::position_compose_block),
            )
                .in_set(CellPhase::Layout),
        );

        // Layout phase part 2: borders + frames
        app.add_systems(
            Update,
            (
                // Block border systems
                block_border::spawn_block_borders
                    .after(systems::position_block_cells_from_flex),
                // (Legacy layout_block_borders removed — flex positioning takes over)
                block_border::layout_block_borders_from_flex
                    .after(block_border::spawn_block_borders)
                    .after(bevy::ui::UiSystems::Prepare),
                block_border::update_block_border_state
                    .after(block_border::spawn_block_borders),
                block_border::cleanup_block_borders,
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
