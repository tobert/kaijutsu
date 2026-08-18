//! Entity lifecycle — the MainCell singleton and the focused-pane tracker.
//!
//! Owns `spawn_main_cell` and `track_conversation_container`. Block content
//! itself is entity-free (`view::surface`): a block never gets its own
//! entity, so there is nothing here to spawn or despawn per block any more —
//! what's left is bookkeeping for the two singletons every conversation-view
//! system reaches through `EditorEntities`: the `MainCell` (the `CellEditor`
//! + `ConversationGeometry` source of truth) and the currently-focused
//! pane's `ConversationContainer`.

use bevy::prelude::*;

use crate::cell::{CellEditor, MainCell};

/// Consolidated resource tracking editor-related singleton entities.
#[derive(Resource, Default)]
pub struct EditorEntities {
    /// The main conversation cell entity.
    pub main_cell: Option<Entity>,
    /// The focused pane's `ConversationContainer` entity — the surface path's
    /// viewport anchor (`view::surface::window`, `chrome`, `rich`,
    /// `shape_cache`) and `smooth_scroll`'s `ScrollPosition`-adjacent read.
    pub conversation_container: Option<Entity>,
}

// ============================================================================
// SPAWN SYSTEMS
// ============================================================================

/// Spawn the main kernel cell on startup.
///
/// This is the primary workspace cell that displays kernel output, shell interactions,
/// and agent conversations. It fills the space between the header and prompt.
pub fn spawn_main_cell(
    mut commands: Commands,
    mut entities: ResMut<EditorEntities>,
    conversation_container: Query<Entity, Added<crate::cell::ConversationContainer>>,
) {
    if entities.main_cell.is_some() {
        return;
    }

    let Ok(conv_entity) = conversation_container.single() else {
        return;
    };

    entities.conversation_container = Some(conv_entity);

    let welcome_text = "No context joined";

    // MainCell holds the CellEditor (source of truth for content) and the
    // ConversationGeometry the surface path reads — it carries no render
    // state of its own.
    let entity = commands
        .spawn((
            CellEditor::default().with_text(welcome_text),
            MainCell,
            // The logical geometry model rides with the MainCell from birth
            // so every geometry-reading system (reorder, virtualize,
            // readback) finds it on the very first LayoutGeneration bump —
            // a deferred insert would eat that generation.
            crate::view::geometry::ConversationGeometry::default(),
        ))
        .id();

    entities.main_cell = Some(entity);
    info!("Spawned main kernel cell");
}

/// Track the focused `ConversationContainer`.
///
/// After a pane split, the reconciler despawns and rebuilds all `PaneMarker`
/// entities (`ui/tiling_reconciler.rs`), so the container backing the
/// focused pane is a fresh entity every time. Every surface-path system that
/// reaches a pane's viewport does so through `EditorEntities.conversation_container`
/// (`view::surface::{window,chrome,rich,shape_cache}`, `view::scroll`) —
/// this is the sole writer that keeps it pointed at the currently-focused
/// pane. Content itself needs no re-parenting: the surface path is
/// entity-free, so there is nothing hanging off the old container to carry
/// forward.
pub fn track_conversation_container(
    mut entities: ResMut<EditorEntities>,
    focused_containers: Query<
        Entity,
        (
            With<crate::cell::ConversationContainer>,
            With<crate::ui::tiling::PaneFocus>,
        ),
    >,
) {
    let Ok(focused) = focused_containers.single() else {
        return;
    };

    if entities.conversation_container == Some(focused) {
        return;
    }

    entities.conversation_container = Some(focused);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::ConversationContainer;
    use crate::ui::tiling::PaneFocus;

    fn build_test_app() -> App {
        let mut app = App::new();
        app.init_resource::<EditorEntities>();
        app.add_systems(Update, track_conversation_container);
        app
    }

    #[test]
    fn track_conversation_container_follows_the_focused_pane() {
        let mut app = build_test_app();
        let focused_conv = app
            .world_mut()
            .spawn((ConversationContainer, PaneFocus))
            .id();

        app.update();

        let entities = app.world().resource::<EditorEntities>();
        assert_eq!(
            entities.conversation_container,
            Some(focused_conv),
            "EditorEntities.conversation_container must track the focused pane"
        );
    }

    #[test]
    fn track_conversation_container_is_a_noop_once_focus_already_tracked() {
        let mut app = build_test_app();
        let focused_conv = app
            .world_mut()
            .spawn((ConversationContainer, PaneFocus))
            .id();

        app.update();
        assert_eq!(
            app.world().resource::<EditorEntities>().conversation_container,
            Some(focused_conv)
        );

        // Running again with the same focused container already tracked
        // must not touch the resource (nothing to assert on the write
        // itself — this pins that the no-op branch is reachable/harmless).
        app.update();
        assert_eq!(
            app.world().resource::<EditorEntities>().conversation_container,
            Some(focused_conv)
        );
    }
}
