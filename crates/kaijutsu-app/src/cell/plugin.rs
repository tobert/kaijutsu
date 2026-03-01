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
    DocumentCache, FocusTarget, LayoutGeneration, MainCell, SessionAgent,
    PendingContextSwitch, PromptSubmitted, RoleGroupBorderLayout, SubmitFailed,
    ViewingConversation, WorkspaceLayout,
};
use super::block_border;
use super::systems;

// Phase 3+4: Systems from view/ module replace cell/systems equivalents.
use crate::view::lifecycle as view_lifecycle;
use crate::view::overlay as view_overlay;
use crate::view::render as view_render;
use crate::view::submit as view_submit;
use crate::view::sync as view_sync;

/// Plugin that enables cell-based editing in the workspace.
pub struct CellPlugin;

impl Plugin for CellPlugin {
    fn build(&self, app: &mut App) {
        // Register messages
        app.add_message::<PromptSubmitted>()
            .add_message::<SubmitFailed>()
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
            .register_type::<RoleGroupBorderLayout>()
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
            .init_resource::<SessionAgent>()
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
        // Phase 4: sync + submit systems from view/
        // ====================================================================
        app.add_systems(
            Update,
            (
                // Block event handling (view/sync)
                view_sync::handle_block_events,
                // Input document events (view/sync)
                view_sync::handle_input_doc_events.after(view_sync::handle_block_events),
                // Context switching (view/sync)
                view_sync::handle_context_switch.after(view_sync::handle_block_events),
                // Handle prompt submission (view/submit)
                view_submit::handle_prompt_submitted
                    .after(view_sync::handle_context_switch),
                // Restore text + flash border on submit failure (view/submit)
                view_submit::handle_submit_failed
                    .after(view_submit::handle_prompt_submitted),
                // Sync main cell to conversation (view/sync)
                view_sync::sync_main_cell_to_conversation
                    .after(view_sync::handle_block_events)
                    .after(view_sync::handle_context_switch)
                    .after(view_submit::handle_prompt_submitted),
                // Staleness detection (view/sync)
                view_sync::check_cache_staleness
                    .after(view_sync::handle_block_events)
                    .after(view_sync::handle_context_switch),
            )
                .in_set(CellPhase::Sync),
        );

        // ====================================================================
        // CellPhase::Spawn - Entity spawning + ApplyDeferred
        // Phase 3: spawn systems from view/lifecycle (TopLeft anchor, no UiTransform)
        // ====================================================================
        app.add_systems(
            Update,
            (
                // Main cell spawning (view/)
                view_lifecycle::spawn_main_cell,
                // Input overlay spawning (view/overlay)
                view_overlay::spawn_input_overlay,
                // Track focused pane and re-parent block cells after split (view/)
                view_lifecycle::track_conversation_container.after(view_lifecycle::spawn_main_cell),
                // Block cell spawning — NO UiTransform (view/)
                view_lifecycle::spawn_block_cells,
                // Role group border sync (view/)
                view_lifecycle::sync_role_headers.after(view_lifecycle::spawn_block_cells),
                // Cursor spawning
                systems::spawn_cursor,
                // ApplyDeferred to flush spawn commands
                ApplyDeferred.after(view_lifecycle::sync_role_headers),
            )
                .in_set(CellPhase::Spawn),
        );

        // ====================================================================
        // CellPhase::Buffer - Text buffer init/sync, highlighting
        // Phase 3: buffer systems from view/render (TopLeft anchor)
        // ====================================================================
        app.add_systems(
            Update,
            (
                // Cell buffer init and sync (stays in cell/ — non-block cells)
                systems::init_cell_buffers,
                systems::sync_cell_buffers.after(systems::init_cell_buffers),
                systems::compute_cell_heights,
                // Block cell buffer init and sync (view/ — TopLeft anchor)
                // ApplyDeferred flushes init's UiVelloText insert so sync can
                // set real text on the same frame.
                view_render::init_block_cell_buffers,
                ApplyDeferred.after(view_render::init_block_cell_buffers),
                view_render::sync_block_cell_buffers
                    .after(view_render::init_block_cell_buffers),
                // Input overlay visibility + buffer sync (view/overlay)
                view_overlay::sync_overlay_visibility,
                view_overlay::sync_input_overlay_buffer,
                // Highlighting (after buffer sync)
                systems::highlight_focused_cell.after(systems::sync_cell_buffers),
                view_render::highlight_focused_block.after(view_render::sync_block_cell_buffers),
                // Block border style determination (after buffer sync)
                block_border::determine_block_border_style
                    .after(view_render::sync_block_cell_buffers),
                // Flush BlockBorderStyle inserts
                ApplyDeferred
                    .after(block_border::determine_block_border_style),
            )
                .in_set(CellPhase::Buffer),
        );

        // ====================================================================
        // CellPhase::Layout - Measure heights, scroll, position entities
        // ====================================================================
        // Layout phase part 1: measure, scroll, position
        // Phase 3: layout systems from view/render
        app.add_systems(
            Update,
            (
                // Block cell layout — indentation levels (view/)
                view_render::layout_block_cells,
                // Sync indentation to Node for flex layout (view/)
                view_render::update_block_cell_nodes.after(view_render::layout_block_cells),
                // Reorder children to match document order (view/)
                view_render::reorder_conversation_children.after(view_render::update_block_cell_nodes),
                // Smooth scroll (stays in cell/ — Phase 5)
                systems::smooth_scroll.after(view_render::layout_block_cells),
                // Cursor positioning (stays in cell/ — Phase 5)
                systems::update_cursor,
                systems::update_block_edit_cursor.after(systems::update_cursor),
            )
                .in_set(CellPhase::Layout),
        );

        // Layout phase part 2: borders + frames (non-position-dependent)
        app.add_systems(
            Update,
            (
                // Compose error border animation (view/submit)
                view_submit::animate_compose_error,
                block_border::cleanup_block_borders,
            )
                .in_set(CellPhase::Layout),
        );

        // ====================================================================
        // PostUpdate — Systems that read UiGlobalTransform (set by UiSystems::Layout)
        // Must run in PostUpdate so layout values are fresh, not one frame behind.
        // ====================================================================
        // Phase 3: PostUpdate systems from view/render
        app.add_systems(
            PostUpdate,
            (
                // Input overlay cursor (reads UiGlobalTransform — stays in cell/)
                systems::update_input_overlay_cursor,
                // Read back actual block heights from Taffy layout (view/)
                view_render::readback_block_heights,
                // Vello border systems (spawn → update → animate)
                block_border::spawn_vello_borders
                    .after(view_render::readback_block_heights),
                block_border::update_vello_borders
                    .after(block_border::spawn_vello_borders),
                block_border::animate_vello_borders
                    .after(block_border::update_vello_borders),
                // Role group border scene updates (view/)
                view_render::update_role_group_scenes,
            )
                .after(bevy::ui::UiSystems::Layout),
        );
    }
}
