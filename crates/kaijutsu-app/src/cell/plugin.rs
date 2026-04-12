//! Cell plugin for Bevy.

use bevy::prelude::*;
use bevy_remote::{RemoteMethodSystemId, RemoteMethods};

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

use super::block_border;
use crate::ui::tiling_reconciler::TilingPhase;
use crate::view::overlay::{OverlayStyle, OverlaySummonState};
use crate::view::shell_dock::ShellDockSummonState;
use crate::view::{
    BlockCellContainer, BlockCellLayout, ContextSwitchRequested, ConversationContainer,
    ConversationScrollState, DocumentCache, EditorEntities, FocusTarget, LayoutGeneration,
    MainCell, PendingContextSwitch, RoleGroupBorderLayout, SessionAgent, SubmitFailed,
    ViewingConversation,
};

use crate::view::lifecycle as view_lifecycle;
use crate::view::overlay as view_overlay;
use crate::view::shell_dock as view_shell_dock;
use crate::view::render as view_render;
use crate::view::scroll as view_scroll;
use crate::view::submit as view_submit;
use crate::view::sync as view_sync;

/// Plugin that enables cell-based editing in the workspace.
pub struct CellPlugin;

impl Plugin for CellPlugin {
    fn build(&self, app: &mut App) {
        // Register messages
        app.add_message::<SubmitFailed>()
            .add_message::<ContextSwitchRequested>();

        // Register types for BRP reflection
        app.register_type::<ConversationScrollState>()
            .register_type::<ConversationContainer>()
            .register_type::<MainCell>()
            .register_type::<ViewingConversation>()
            .register_type::<FocusTarget>()
            .register_type::<BlockCellContainer>()
            .register_type::<BlockCellLayout>()
            .register_type::<RoleGroupBorderLayout>()
            .register_type::<block_border::BlockBorderStyle>()
            .register_type::<block_border::BorderLabelMetrics>()
            .register_type::<OverlayStyle>();

        // Register custom BRP methods for context navigation
        use crate::view::brp_methods;
        let switch_id = app.register_system(brp_methods::handle_switch_context);
        let active_id = app.register_system(brp_methods::handle_active_context);
        app.world_mut()
            .resource_mut::<RemoteMethods>()
            .insert(
                brp_methods::SWITCH_CONTEXT_METHOD,
                RemoteMethodSystemId::Instant(switch_id),
            );
        app.world_mut()
            .resource_mut::<RemoteMethods>()
            .insert(
                brp_methods::ACTIVE_CONTEXT_METHOD,
                RemoteMethodSystemId::Instant(active_id),
            );

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
            .init_resource::<EditorEntities>()
            .init_resource::<OverlaySummonState>()
            .init_resource::<ShellDockSummonState>()
            .init_resource::<crate::view::components::ErrorChildIndex>()
            .init_resource::<crate::view::components::ExpandedErrorParents>();

        // ====================================================================
        // CellPhase::Sync — server events, document sync, prompt submission
        // ====================================================================
        app.add_systems(
            Update,
            (
                view_sync::handle_block_events,
                view_sync::handle_input_doc_events.after(view_sync::handle_block_events),
                view_sync::handle_context_switch.after(view_sync::handle_block_events),
                view_sync::handle_server_context_switch.before(view_sync::handle_context_switch),
                view_submit::handle_submit_failed.after(view_sync::handle_context_switch),
                view_sync::sync_main_cell_to_conversation
                    .after(view_sync::handle_block_events)
                    .after(view_sync::handle_context_switch),
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
                view_shell_dock::spawn_shell_dock,
                view_lifecycle::track_conversation_container.after(view_lifecycle::spawn_main_cell),
                view_lifecycle::spawn_block_cells,
                view_lifecycle::sync_role_headers.after(view_lifecycle::spawn_block_cells),
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
                view_render::sync_block_cell_buffers,
                // Input overlay (chat)
                view_overlay::update_summon_animation,
                view_overlay::sync_overlay_visibility.after(view_overlay::update_summon_animation),
                view_overlay::sync_overlay_style_to_theme,
                // Shell dock
                view_shell_dock::update_shell_dock_summon,
                view_shell_dock::sync_shell_dock_visibility
                    .after(view_shell_dock::update_shell_dock_summon),
                view_shell_dock::sync_shell_dock_style_to_theme,
                // Highlighting
                view_render::highlight_focused_block.after(view_render::sync_block_cell_buffers),
                // Error child index (must run before block border style)
                crate::view::components::build_error_child_index
                    .after(view_render::sync_block_cell_buffers),
                // Block border style
                block_border::determine_block_border_style
                    .after(crate::view::components::build_error_child_index),
                ApplyDeferred.after(block_border::determine_block_border_style),
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
                view_render::reorder_conversation_children
                    .after(view_render::update_block_cell_nodes),
                view_scroll::smooth_scroll.after(view_render::layout_block_cells),
                view_render::cull_offscreen_blocks.after(view_scroll::smooth_scroll),
            )
                .in_set(CellPhase::Layout),
        );

        app.add_systems(
            Update,
            (
                view_submit::animate_compose_error,
                view_overlay::animate_summon,
                view_shell_dock::animate_shell_dock_summon,
            )
                .in_set(CellPhase::Layout),
        );

        // ====================================================================
        // PostUpdate — Content sizing, Layout, then readback
        // ====================================================================
        app.add_systems(
            PostUpdate,
            (
                view_render::readback_block_heights.after(bevy::ui::UiSystems::Layout),
                view_overlay::build_overlay_glyphs.after(bevy::ui::UiSystems::Layout),
                view_shell_dock::build_shell_dock_glyphs.after(bevy::ui::UiSystems::Layout),
            ),
        );
    }
}
