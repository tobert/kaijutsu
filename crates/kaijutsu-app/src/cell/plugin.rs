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
/// 3. **Spawn** - Entity spawning (MainCell, overlay, shell dock) + the
///    conversation geometry reconcile the surface path's own Content/Shape/
///    Measure/Window sets (`view::surface`) chain after
/// 4. **Buffer** - Overlay/shell-dock buffer sync + animation
/// 5. **Layout** - Scroll easing + animation
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub enum CellPhase {
    /// Mode switching, key handling, click-to-focus
    Input,
    /// Server events, document sync
    Sync,
    /// Entity spawning (MainCell, overlay, shell dock) + geometry reconcile
    Spawn,
    /// Overlay/shell-dock buffer sync
    Buffer,
    /// Scroll, animate
    Layout,
}

use super::block_border;
use crate::ui::tiling_reconciler::TilingPhase;
use crate::view::overlay::{OverlayStyle, OverlaySummonState};
use crate::view::shell_dock::ShellDockSummonState;
use crate::view::{
    ContextSwitchRequested, ConversationContainer, ConversationScrollState, DocumentCache,
    EditorEntities, FocusTarget, MainCell, PendingContextSwitch, SessionPrincipal, SubmitFailed,
    ViewingConversation,
};

use crate::view::geometry as view_geometry;
use crate::view::lifecycle as view_lifecycle;
use crate::view::overlay as view_overlay;
use crate::view::scroll as view_scroll;
use crate::view::shell_dock as view_shell_dock;
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
        // CellPhase::Spawn — entity spawning + geometry reconcile
        // ====================================================================
        app.add_systems(
            Update,
            (
                view_lifecycle::spawn_main_cell,
                view_overlay::spawn_input_overlay,
                view_shell_dock::spawn_shell_dock,
                view_lifecycle::track_conversation_container.after(view_lifecycle::spawn_main_cell),
                // The surface path's Content/Shape/Measure/Window sets
                // (`view::surface::ConversationSurfacePlugin`) chain directly
                // after this — see that plugin's module docs for why.
                view_geometry::sync_conversation_geometry,
            )
                .in_set(CellPhase::Spawn),
        );

        // ====================================================================
        // CellPhase::Buffer — overlay/shell-dock buffer sync, highlighting
        // ====================================================================
        app.add_systems(
            Update,
            (
                // Input overlay (chat)
                view_overlay::update_summon_animation,
                view_overlay::sync_overlay_visibility.after(view_overlay::update_summon_animation),
                view_overlay::sync_overlay_style_to_theme,
                // Shell dock
                view_shell_dock::update_shell_dock_summon,
                view_shell_dock::sync_shell_dock_visibility
                    .after(view_shell_dock::update_shell_dock_summon),
                view_shell_dock::sync_shell_dock_style_to_theme,
            )
                .in_set(CellPhase::Buffer),
        );

        // ====================================================================
        // CellPhase::Layout — scroll, animate
        // ====================================================================
        app.add_systems(
            Update,
            (
                view_scroll::smooth_scroll,
                view_scroll::scroll_render_mode.after(view_scroll::smooth_scroll),
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
        // PostUpdate — overlay/shell-dock glyph build
        // ====================================================================
        app.add_systems(
            PostUpdate,
            (
                view_overlay::build_overlay_glyphs.after(bevy::ui::UiSystems::Layout),
                view_shell_dock::build_shell_dock_glyphs.after(bevy::ui::UiSystems::Layout),
            ),
        );
    }
}
