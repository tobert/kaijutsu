//! The Neo conversation workspace. The transcript remains the source reader.

use bevy::prelude::*;
use kaijutsu_types::ContextId;

use crate::input::{Action, InputPhase, context::InputContext, events::ActionFired, systems::reveal_block};
use crate::ui::screen::Screen;
use crate::view::{ConversationScrollState, DocumentCache, EditorEntities, FocusTarget, MainCell};
use crate::view::geometry::ConversationGeometry;

mod model;
mod panel;
mod projection;

use model::{Records, Workspace};

#[derive(Resource, Reflect, Default)]
#[reflect(Resource)]
pub struct NeoMode {
    pub enabled: bool,
}

pub struct NeoPlugin;

impl Plugin for NeoPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<NeoMode>()
            .init_resource::<Workspace>()
            .register_type::<NeoMode>()
            .add_systems(PostStartup, panel::spawn_rail)
            .add_systems(
                Update,
                (toggle_neo, sync_workspace, handle_controls, panel::sync_layout)
                    .chain()
                    .after(InputPhase::Dispatch)
                    .after(crate::view::geometry::sync_conversation_geometry)
                    .before(crate::view::scroll::smooth_scroll),
            )
            .add_systems(
                PostUpdate,
                panel::render_panels
                    .after(bevy::ui::UiSystems::Layout)
                    .before(crate::view::block_render::resize_block_textures),
            );
    }
}

fn toggle_neo(
    mut actions: MessageReader<ActionFired>,
    screen: Res<State<Screen>>,
    mut mode: ResMut<NeoMode>,
) {
    for fired in actions.read() {
        if *screen.get() == Screen::Conversation && fired.action == Action::ToggleNeo {
            mode.enabled = !mode.enabled;
        }
    }
}

fn sync_workspace(
    mode: Res<NeoMode>,
    documents: Res<DocumentCache>,
    mut workspace: ResMut<Workspace>,
    mut seen: Local<Option<(ContextId, u64)>>,
) {
    let context = documents.active_id();
    if context != workspace.context {
        workspace.reconcile(context, None);
    }
    if !mode.enabled {
        *seen = None;
        return;
    }

    let Some(mirror) = context.and_then(|id| documents.get(id)).map(|entry| &entry.mirror) else {
        *seen = None;
        return;
    };
    if !mirror.is_hydrated() {
        // The mirror retains its last good snapshot during gap recovery.
        // Keep that view until the replacement snapshot is installed.
        *seen = None;
        return;
    }
    let stamp = (mirror.context_id(), mirror.version());
    if *seen == Some(stamp) {
        return;
    }
    *seen = Some(stamp);
    let records = Records::from_mirror(mirror);
    if !workspace.ready || workspace.records != records {
        workspace.reconcile(context, Some(records));
    }
}

fn handle_controls(
    mut actions: MessageReader<ActionFired>,
    mode: Res<NeoMode>,
    screen: Res<State<Screen>>,
    mut workspace: ResMut<Workspace>,
    entities: Res<EditorEntities>,
    geometries: Query<&ConversationGeometry, With<MainCell>>,
    mut focus: ResMut<FocusTarget>,
    mut scroll: ResMut<ConversationScrollState>,
) {
    for fired in actions.read() {
        if fired.context != InputContext::Neo || !mode.enabled || *screen.get() != Screen::Conversation {
            continue;
        }

        match fired.action {
            Action::StepNext => workspace.step(true),
            Action::StepPrev => workspace.step(false),
            Action::NeoSwitchSection => workspace.switch_section(),
            Action::NeoReveal => {
                if let (Some(id), Some(cell)) = (workspace.selected, entities.main_cell)
                    && let Ok(geometry) = geometries.get(cell)
                {
                    reveal_block(geometry, &mut focus, &mut scroll, id);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::geometry::{EstimateParams, RowSeed};
    use kaijutsu_client::{ContextMirror, DocumentEntry};
    use kaijutsu_types::{BlockId, BlockKind, BlockSnapshotBuilder, ContentType, PrincipalId, Role};

    #[test]
    fn neo_actions_reveal_the_selected_source_and_leave_other_context_actions_alone() {
        let mut app = App::new();
        app.add_plugins(bevy::state::app::StatesPlugin)
            .init_state::<Screen>()
            .init_resource::<NeoMode>()
            .init_resource::<Workspace>()
            .init_resource::<DocumentCache>()
            .init_resource::<EditorEntities>()
            .init_resource::<FocusTarget>()
            .init_resource::<ConversationScrollState>()
            .add_message::<ActionFired>()
            .add_systems(Update, (toggle_neo, sync_workspace, handle_controls).chain());

        let context = ContextId::from_bytes([1; 16]);
        let principal = PrincipalId::from_bytes([2; 16]);
        let older = BlockId::new(context, principal, 1);
        let newer = BlockId::new(context, principal, 2);
        let snapshots: Vec<_> = [older, newer].into_iter().map(|id| {
            BlockSnapshotBuilder::new(id, BlockKind::Text).content_type(ContentType::Diff).build()
        }).collect();
        let mut mirror = ContextMirror::new(context);
        mirror.apply_snapshot(snapshots, 8).unwrap();
        let mut documents = app.world_mut().resource_mut::<DocumentCache>();
        documents.insert(context, DocumentEntry::new(mirror, None, "test".to_string()));
        documents.set_active(context);

        let mut geometry = ConversationGeometry::default();
        geometry.reconcile(&[older, newer], |_| Some(RowSeed {
            text_len: 120,
            newline_count: 8,
            role: Role::Model,
            kind: BlockKind::Text,
            collapsed: false,
            parent_id: None,
        }), &EstimateParams::default(), 8);
        geometry.recompute_offsets();
        let newer_y = geometry.block_row(&newer).unwrap().y_offset;
        let cell = app.world_mut().spawn((MainCell, geometry)).id();
        app.world_mut().resource_mut::<EditorEntities>().main_cell = Some(cell);
        let mut scroll = app.world_mut().resource_mut::<ConversationScrollState>();
        scroll.content_height = 700.0;
        scroll.visible_height = 70.0;
        scroll.offset = newer_y;
        scroll.target_offset = newer_y;

        let fire = |app: &mut App, action, context| {
            app.world_mut().write_message(ActionFired::new(action, context)).unwrap();
        };
        fire(&mut app, Action::ToggleNeo, InputContext::Global);
        fire(&mut app, Action::StepNext, InputContext::Neo);
        fire(&mut app, Action::StepNext, InputContext::Neo);
        fire(&mut app, Action::NeoReveal, InputContext::Navigation);
        app.update();
        assert_eq!(app.world().resource::<Workspace>().selected, Some(older));
        assert_eq!(app.world().resource::<FocusTarget>().block_id, None);

        fire(&mut app, Action::NeoReveal, InputContext::Neo);
        app.update();
        let scroll = app.world().resource::<ConversationScrollState>();
        let geometry = app.world().get::<ConversationGeometry>(cell).unwrap();
        let focus = app.world().resource::<FocusTarget>();
        assert_eq!(focus.block_id, Some(older));
        assert_eq!(focus.reader_block(scroll, Some(geometry)), Some(older));
        assert!(scroll.offset < newer_y);
    }
}
