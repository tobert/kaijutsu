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
    ConversationScrollState, ConversationSpacer, DocumentCache, EditorEntities, FocusTarget,
    LayoutGeneration, MainCell, PendingContextSwitch, RoleGroupBorderLayout, SessionPrincipal,
    SubmitFailed, ViewingConversation,
};

use crate::view::surface::ConversationRenderPath;

use crate::view::geometry as view_geometry;
use crate::view::lifecycle as view_lifecycle;
use crate::view::overlay as view_overlay;
use crate::view::shell_dock as view_shell_dock;
use crate::view::render as view_render;
use crate::view::scroll as view_scroll;
use crate::view::submit as view_submit;
use crate::view::sync as view_sync;

/// Run condition: the legacy taffy conversation path is live (the default).
///
/// The mirror image of `view::surface::surface_active`, but defined here
/// rather than there — this gates the *legacy* systems this file owns, not
/// the surface path's own systems. Kept as one named condition (rather than
/// a closure per call site) so every gated system reads the same way and a
/// `grep` for `legacy_active` finds the whole gated set.
fn legacy_active(path: Res<ConversationRenderPath>) -> bool {
    *path == ConversationRenderPath::Legacy
}

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
            .register_type::<ConversationSpacer>()
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
        use crate::kaish::brp_methods;
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
            .init_resource::<SessionPrincipal>()
            .init_resource::<DocumentCache>()
            .init_resource::<crate::cell::ScrollOffsets>()
            .init_resource::<PendingContextSwitch>()
            .init_resource::<EditorEntities>()
            .init_resource::<OverlaySummonState>()
            .init_resource::<ShellDockSummonState>()
            .init_resource::<crate::view::components::GlobalErrorQueue>();

        // ====================================================================
        // CellPhase::Sync — server events, document sync, prompt submission
        // ====================================================================
        app.add_systems(
            Update,
            (
                view_sync::handle_block_events,
                view_sync::drain_context_hydrations.after(view_sync::handle_block_events),
                view_sync::handle_context_switch.after(view_sync::handle_block_events),
                view_sync::handle_server_context_switch.before(view_sync::handle_context_switch),
                view_submit::handle_submit_failed.after(view_sync::handle_context_switch),
                // Drains each followed context's change feed — the
                // docs/change-feed.md steady-state + recovery driver that
                // replaced `check_cache_staleness`. Must run before
                // `sync_main_cell_to_conversation` reads `mirror.version()`,
                // and after `drain_context_hydrations` so a just-installed
                // mirror's own feed is drained the same frame it lands.
                view_sync::drain_context_feeds
                    .after(view_sync::drain_context_hydrations)
                    .after(view_sync::handle_context_switch),
                view_sync::sync_main_cell_to_conversation
                    .after(view_sync::handle_block_events)
                    .after(view_sync::handle_context_switch)
                    .after(view_sync::drain_context_feeds),
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
                view_lifecycle::ensure_conversation_spacers
                    .after(view_lifecycle::track_conversation_container)
                    .run_if(legacy_active),
                view_geometry::sync_conversation_geometry,
                view_lifecycle::spawn_block_cells
                    .after(view_geometry::sync_conversation_geometry)
                    .run_if(legacy_active),
                view_lifecycle::sync_role_headers
                    .after(view_lifecycle::spawn_block_cells)
                    .run_if(legacy_active),
                ApplyDeferred
                    .after(view_lifecycle::sync_role_headers)
                    .after(view_lifecycle::ensure_conversation_spacers),
            )
                .in_set(CellPhase::Spawn),
        );

        // ====================================================================
        // CellPhase::Buffer — text buffer init/sync, highlighting
        // ====================================================================
        app.add_systems(
            Update,
            (
                // Theme repaint gate-reopen must precede the buffer sync so
                // one frame carries color re-derive + glyph re-bake together.
                crate::view::block_render::repaint_block_scenes_on_theme_change
                    .before(view_render::sync_block_cell_buffers),
                // Block cell buffers (TopLeft anchor)
                view_render::sync_block_cell_buffers.run_if(legacy_active),
                // Input overlay (chat)
                view_overlay::update_summon_animation,
                view_overlay::sync_overlay_visibility.after(view_overlay::update_summon_animation),
                view_overlay::sync_overlay_style_to_theme,
                // Shell dock
                view_shell_dock::update_shell_dock_summon,
                view_shell_dock::sync_shell_dock_visibility
                    .after(view_shell_dock::update_shell_dock_summon),
                view_shell_dock::sync_shell_dock_style_to_theme,
                // Block border style (also carries the block-focus ring)
                block_border::determine_block_border_style
                    .after(view_render::sync_block_cell_buffers),
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
                view_render::layout_block_cells.run_if(legacy_active),
                view_render::update_block_cell_nodes
                    .after(view_render::layout_block_cells)
                    .run_if(legacy_active),
                view_render::reorder_conversation_children
                    .after(view_render::update_block_cell_nodes)
                    .run_if(legacy_active),
                view_scroll::smooth_scroll.after(view_render::layout_block_cells),
                view_scroll::scroll_render_mode.after(view_scroll::smooth_scroll),
                view_render::virtualize_conversation
                    .after(view_scroll::smooth_scroll)
                    .run_if(legacy_active),
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
                view_render::readback_block_heights
                    .after(bevy::ui::UiSystems::Layout)
                    .run_if(legacy_active),
                view_overlay::build_overlay_glyphs.after(bevy::ui::UiSystems::Layout),
                view_shell_dock::build_shell_dock_glyphs.after(bevy::ui::UiSystems::Layout),
            ),
        );
    }
}

#[cfg(test)]
mod legacy_gate_tests {
    use super::*;
    use crate::view::lifecycle::ensure_conversation_spacers;

    /// Mirrors `view::surface::tests::surface_active_only_for_the_surface_path`
    /// on the other side of the fence: the condition legacy systems gate on
    /// must actually distinguish the two `ConversationRenderPath` states.
    #[test]
    fn legacy_active_only_for_the_legacy_path() {
        let mut world = World::new();
        world.insert_resource(ConversationRenderPath::Legacy);
        let mut sys = IntoSystem::into_system(legacy_active);
        sys.initialize(&mut world);
        assert!(sys.run((), &mut world).unwrap());

        world.insert_resource(ConversationRenderPath::Surface);
        assert!(!sys.run((), &mut world).unwrap());
    }

    /// End-to-end through an actual `run_if`-gated schedule (not just the
    /// bare condition function): `ensure_conversation_spacers` — the
    /// conversation-spacer upkeep system — must spawn spacers under `Legacy`
    /// and must not touch the world at all under `Surface`, exactly the
    /// double-measurement fight the flag exists to prevent (see
    /// `view::surface` module docs).
    #[test]
    fn spacer_upkeep_runs_under_legacy_and_is_skipped_under_surface() {
        let mut legacy_app = App::new();
        legacy_app.insert_resource(ConversationRenderPath::Legacy);
        legacy_app.init_resource::<crate::cell::EditorEntities>();
        legacy_app.add_systems(Update, ensure_conversation_spacers.run_if(legacy_active));
        let conv = legacy_app.world_mut().spawn_empty().id();
        legacy_app
            .world_mut()
            .resource_mut::<crate::cell::EditorEntities>()
            .conversation_container = Some(conv);
        legacy_app.update();
        assert!(
            legacy_app
                .world()
                .resource::<crate::cell::EditorEntities>()
                .top_spacer
                .is_some(),
            "legacy path must still run spacer upkeep"
        );

        let mut surface_app = App::new();
        surface_app.insert_resource(ConversationRenderPath::Surface);
        surface_app.init_resource::<crate::cell::EditorEntities>();
        surface_app.add_systems(Update, ensure_conversation_spacers.run_if(legacy_active));
        let conv = surface_app.world_mut().spawn_empty().id();
        surface_app
            .world_mut()
            .resource_mut::<crate::cell::EditorEntities>()
            .conversation_container = Some(conv);
        surface_app.update();
        assert!(
            surface_app
                .world()
                .resource::<crate::cell::EditorEntities>()
                .top_spacer
                .is_none(),
            "surface path must not run the legacy spacer upkeep system"
        );
    }
}
