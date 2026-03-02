//! Cell plugin for Bevy.

use bevy::prelude::*;

// ============================================================================
// SYSTEM SETS - Execution Phases
// ============================================================================

/// SystemSets for organizing cell systems into execution phases.
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
use crate::view::{
    BlockCellContainer, BlockCellLayout,
    ContextSwitchRequested, ConversationContainer, ConversationScrollState,
    DocumentCache, FocusTarget, LayoutGeneration, MainCell, SessionAgent,
    PendingContextSwitch, PromptSubmitted, RoleGroupBorderLayout, SubmitFailed,
    ViewingConversation, EditorEntities,
};
use super::block_border;

use crate::view::cursor as view_cursor;
use crate::view::lifecycle as view_lifecycle;
use crate::view::overlay as view_overlay;
use crate::view::render as view_render;
use crate::view::scroll as view_scroll;
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
            .register_type::<ViewingConversation>()
            .register_type::<FocusTarget>()
            .register_type::<BlockCellContainer>()
            .register_type::<BlockCellLayout>()
            .register_type::<RoleGroupBorderLayout>()
            .register_type::<block_border::BlockBorderStyle>();

        // Configure SystemSet execution order
        app.configure_sets(
            Update,
            (
                CellPhase::Input.after(TilingPhase::PostReconcile),
                CellPhase::Sync.after(CellPhase::Input),
                CellPhase::Spawn.after(CellPhase::Sync),
                CellPhase::Buffer.after(CellPhase::Spawn),
                CellPhase::Layout.after(CellPhase::Buffer),
            ),
        );

        app.init_resource::<FocusTarget>()
            .init_resource::<ConversationScrollState>()
            .init_resource::<LayoutGeneration>()
            .init_resource::<SessionAgent>()
            .init_resource::<DocumentCache>()
            .init_resource::<PendingContextSwitch>()
            .init_resource::<EditorEntities>();

        // ====================================================================
        // CellPhase::Sync — server events, document sync, prompt submission
        // ====================================================================
        app.add_systems(
            Update,
            (
                view_sync::handle_block_events,
                view_sync::handle_input_doc_events.after(view_sync::handle_block_events),
                view_sync::handle_context_switch.after(view_sync::handle_block_events),
                view_submit::handle_prompt_submitted
                    .after(view_sync::handle_context_switch),
                view_submit::handle_submit_failed
                    .after(view_submit::handle_prompt_submitted),
                view_sync::sync_main_cell_to_conversation
                    .after(view_sync::handle_block_events)
                    .after(view_sync::handle_context_switch)
                    .after(view_submit::handle_prompt_submitted),
                view_sync::check_cache_staleness
                    .after(view_sync::handle_block_events)
                    .after(view_sync::handle_context_switch),
            )
                .in_set(CellPhase::Sync),
        );

        // ====================================================================
        // CellPhase::Spawn — entity spawning + ApplyDeferred
        // ====================================================================
        app.add_systems(
            Update,
            (
                view_lifecycle::spawn_main_cell,
                view_overlay::spawn_input_overlay,
                view_lifecycle::track_conversation_container.after(view_lifecycle::spawn_main_cell),
                view_lifecycle::spawn_block_cells,
                view_lifecycle::sync_role_headers.after(view_lifecycle::spawn_block_cells),
                view_cursor::spawn_cursor,
                ApplyDeferred.after(view_lifecycle::sync_role_headers),
            )
                .in_set(CellPhase::Spawn),
        );

        // ====================================================================
        // CellPhase::Buffer — text buffer init/sync, highlighting
        // ====================================================================
        app.add_systems(
            Update,
            (
                // Block cell buffers (TopLeft anchor)
                view_render::init_block_cell_buffers,
                ApplyDeferred.after(view_render::init_block_cell_buffers),
                view_render::sync_block_cell_buffers
                    .after(view_render::init_block_cell_buffers),
                // Input overlay
                view_overlay::sync_overlay_visibility,
                view_overlay::sync_input_overlay_buffer,
                // Highlighting
                view_render::highlight_focused_block.after(view_render::sync_block_cell_buffers),
                // Block border style
                block_border::determine_block_border_style
                    .after(view_render::sync_block_cell_buffers),
                ApplyDeferred
                    .after(block_border::determine_block_border_style),
                // Rich content rendering — must run AFTER ApplyDeferred so the
                // RichContent component inserted by sync_block_cell_buffers is visible.
                crate::text::rich::render_rich_content
                    .after(block_border::determine_block_border_style),
            )
                .in_set(CellPhase::Buffer),
        );

        // ====================================================================
        // CellPhase::Layout — measure, scroll, position, animate
        // ====================================================================
        app.add_systems(
            Update,
            (
                view_render::layout_block_cells,
                view_render::update_block_cell_nodes.after(view_render::layout_block_cells),
                view_render::reorder_conversation_children.after(view_render::update_block_cell_nodes),
                view_scroll::smooth_scroll.after(view_render::layout_block_cells),
                view_cursor::update_cursor,
                view_cursor::update_block_edit_cursor.after(view_cursor::update_cursor),
            )
                .in_set(CellPhase::Layout),
        );

        app.add_systems(
            Update,
            (
                view_submit::animate_compose_error,
                block_border::cleanup_block_borders,
            )
                .in_set(CellPhase::Layout),
        );

        // ====================================================================
        // PostUpdate — Content sizing, Layout, then readback
        // ====================================================================
        app.add_systems(
            PostUpdate,
            (
                view_cursor::update_input_overlay_cursor.after(bevy::ui::UiSystems::Layout),
                view_render::readback_block_heights.after(bevy::ui::UiSystems::Layout),
                block_border::spawn_vello_borders
                    .after(view_render::readback_block_heights),
                block_border::update_vello_borders
                    .after(block_border::spawn_vello_borders),
                block_border::animate_vello_borders
                    .after(block_border::update_vello_borders),
                view_render::update_role_group_scenes.after(bevy::ui::UiSystems::Layout),
            ),
        );
    }
}
