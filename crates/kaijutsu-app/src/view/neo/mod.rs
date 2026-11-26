//! The Neo conversation workspace. The transcript remains the source reader.

use bevy::prelude::*;
use kaijutsu_types::{BlockId, ContentType, ContextId};

use crate::connection::ledger::LedgerMirror;
use crate::input::{Action, InputPhase, context::InputContext, events::ActionFired, systems::reveal_block};
use crate::text::diff::DiffPreview;
use crate::ui::screen::Screen;
use crate::view::{ConversationScrollState, DocumentCache, EditorEntities, FocusTarget, MainCell};
use crate::view::geometry::ConversationGeometry;
use crate::view::surface::chrome::waiting_on_asks;

mod diff_preview;
mod model;
mod panel;
mod projection;
mod scene;
mod score_preview;
mod cards;

use cards::SceneContent;

use model::{Records, Section, Workspace};

#[derive(Resource, Default)]
struct WorkbenchDiff {
    source: Option<(BlockId, u64)>,
    preview: Option<DiffPreview>,
}

fn sync_workbench_diff(
    mode: Res<NeoMode>,
    documents: Res<DocumentCache>,
    workspace: Res<Workspace>,
    mut diff: ResMut<WorkbenchDiff>,
    mut seen: Local<Option<(BlockId, u64)>>,
) {
    let mirror = workspace.context.and_then(|id| documents.get(id)).map(|entry| &entry.mirror);
    if mode.panels_open && !mode.enabled && let (Some(id), Some(mirror)) = (workspace.workbench_source(), mirror) {
        let stamp = (id, mirror.version());
        if *seen == Some(stamp) {
            return;
        }
        *seen = Some(stamp);
        if workspace.records.workbench.iter().any(|row| row.id == id && row.content_type == ContentType::Diff)
            && let Some(block) = mirror.block(&id)
            && let Some(body) = crate::view::diff_view::diff_body(block.kind, block.content_type, &block.content)
        {
            let key = (id, crate::view::diff_view::content_hash(body));
            if diff.source != Some(key) {
                diff.preview = Some(crate::text::diff::build_diff_preview(body, ContentType::Diff));
                diff.source = Some(key);
            }
            return;
        }
    } else {
        *seen = None;
    }
    if diff.source.is_some() {
        *diff = WorkbenchDiff::default();
    }
}

#[derive(Resource, Reflect)]
#[reflect(Resource)]
pub struct NeoMode {
    pub enabled: bool,
    pub panels_open: bool,
}

impl Default for NeoMode {
    fn default() -> Self {
        Self { enabled: false, panels_open: true }
    }
}

impl NeoMode {
    pub fn active(&self) -> bool {
        self.enabled || self.panels_open
    }
}

pub struct NeoPlugin;

impl Plugin for NeoPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<NeoMode>()
            .init_resource::<Workspace>()
            .init_resource::<SceneContent>()
            .init_resource::<WorkbenchDiff>()
            .register_type::<NeoMode>()
            .add_systems(Startup, (scene::setup, scene::thread::setup))
            .add_systems(PostUpdate, (scene::scenery::sync, scene::turns::sync, scene::thread::sync)
                .after(bevy::ui::UiSystems::Layout)
                .before(bevy::transform::TransformSystems::Propagate))
            .add_systems(PostStartup, panel::spawn_rail)
            .add_systems(Update, (cards::sync, scene::sync).chain().after(panel::sync_layout))
            .add_systems(
                Update,
                (toggle_neo, sync_workspace, handle_controls, sync_workbench_diff, panel::sync_layout)
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
        if *screen.get() != Screen::Conversation { continue; }
        match fired.action {
            Action::ToggleNeo => mode.enabled = !mode.enabled,
            Action::ToggleConversationPanels => {
                mode.panels_open = mode.enabled || !mode.panels_open;
                mode.enabled = false;
            }
            _ => {}
        }
    }
}

fn sync_workspace(
    mode: Res<NeoMode>,
    documents: Res<DocumentCache>,
    ledger: Res<LedgerMirror>,
    mut workspace: ResMut<Workspace>,
    mut seen: Local<Option<(ContextId, u64)>>,
    mut detected: Local<projection::DetectedDiffs>,
) {
    let context = documents.active_id();
    if context != workspace.context {
        workspace.reconcile(context, None);
    }
    if !mode.active() {
        *seen = None;
        return;
    }

    let Some(mirror) = context.and_then(|id| documents.get(id)).map(|entry| &entry.mirror) else {
        *seen = None;
        return;
    };
    if !mirror.is_hydrated() {
        // Keep the current records while the document store fetches a new snapshot.
        *seen = None;
        return;
    }
    let stamp = (mirror.context_id(), mirror.version());
    if *seen == Some(stamp) && !ledger.is_changed() {
        return;
    }
    *seen = Some(stamp);
    let records = Records::from_mirror(mirror, &waiting_on_asks(ledger.open()), &mut detected);
    if !workspace.ready || workspace.records != records {
        workspace.reconcile(context, Some(records));
    }
}

fn handle_controls(
    mut actions: MessageReader<ActionFired>,
    mut mode: ResMut<NeoMode>,
    screen: Res<State<Screen>>,
    mut workspace: ResMut<Workspace>,
    entities: Res<EditorEntities>,
    geometries: Query<&ConversationGeometry, With<MainCell>>,
    mut focus: ResMut<FocusTarget>,
    mut scroll: ResMut<ConversationScrollState>,
) {
    for fired in actions.read() {
        if fired.context != InputContext::Neo || !mode.active() || *screen.get() != Screen::Conversation {
            continue;
        }

        match fired.action {
            Action::StepNext | Action::StepPrev if mode.enabled => workspace.step_scene(fired.action == Action::StepNext),
            Action::StepNext => workspace.step(true),
            Action::StepPrev => workspace.step(false),
            Action::NeoSwitchSection if mode.enabled => workspace.switch_scene_section(),
            Action::NeoSwitchSection => workspace.switch_section(),
            Action::NeoReveal => {
                let source = match workspace.section {
                    Section::Workbench if mode.enabled => workspace.scene_artifact(),
                    Section::Workbench => workspace.workbench_source(),
                    Section::History if mode.enabled => workspace.scene_event().map(|row| row.id),
                    Section::History => workspace.history_source().map(|row| row.id),
                };
                if let (Some(id), Some(cell)) = (source, entities.main_cell)
                    && let Ok(geometry) = geometries.get(cell)
                {
                    if reveal_block(geometry, &mut focus, &mut scroll, id) {
                        mode.enabled = false;
                    }
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
    fn conversation_starts_with_panels_and_keeps_their_setting_across_neo() {
        let mut app = App::new();
        app.add_plugins(bevy::state::app::StatesPlugin)
            .init_state::<Screen>()
            .init_resource::<NeoMode>()
            .add_message::<ActionFired>()
            .add_systems(Update, toggle_neo);
        let state = |app: &App| {
            let mode = app.world().resource::<NeoMode>();
            (mode.panels_open, mode.enabled)
        };
        app.update();
        assert_eq!(state(&app), (true, false));
        for (action, expected) in [
            (Action::ToggleNeo, (true, true)),
            (Action::ToggleNeo, (true, false)),
            (Action::ToggleConversationPanels, (false, false)),
            (Action::ToggleNeo, (false, true)),
            (Action::ToggleNeo, (false, false)),
            (Action::ToggleConversationPanels, (true, false)),
        ] {
            app.world_mut().write_message(ActionFired::new(action, InputContext::Global)).unwrap();
            app.update();
            assert_eq!(state(&app), expected);
        }
    }

    #[test]
    fn workbench_reads_the_latest_diff_and_refreshes_edits_below_its_title() {
        let mut app = App::new();
        app.init_resource::<NeoMode>()
            .init_resource::<DocumentCache>()
            .init_resource::<LedgerMirror>()
            .init_resource::<Workspace>()
            .init_resource::<WorkbenchDiff>()
            .add_systems(Update, (sync_workspace, sync_workbench_diff).chain());
        let context = ContextId::from_bytes([1; 16]);
        let id = BlockId::new(context, PrincipalId::from_bytes([2; 16]), 1);
        for (version, change, content_type) in [
            (1, "+first", ContentType::Diff),
            (2, "+amended", ContentType::Plain),
            (3, "+fenced", ContentType::Plain),
            (4, "+with footer", ContentType::Plain),
        ] {
            let mut source = format!("--- a/f\n+++ b/f\n@@ -1 +1 @@\n-old\n{change}");
            if version == 3 {
                source = format!("```diff\n{source}\n```");
            }
            if version == 4 {
                source.push_str("\n[exit 1]");
            }
            let block = BlockSnapshotBuilder::new(id, BlockKind::ToolResult)
                .content_type(content_type).content(source).build();
            let mut mirror = ContextMirror::new(context);
            mirror.apply_snapshot(vec![block], version).unwrap();
            let mut docs = app.world_mut().resource_mut::<DocumentCache>();
            docs.insert(context, DocumentEntry::new(mirror, None, "test".into()));
            docs.set_active(context);
            app.update();
            let rows = &app.world().resource::<Workspace>().records.workbench;
            assert_eq!(rows.len(), 1, "diff tool result missing at revision {version}");
            assert!(rows[0].title.starts_with("Diff "));
            let preview = app.world().resource::<WorkbenchDiff>().preview.as_ref().unwrap();
            assert!(preview.plain_text.contains(change));
        }
        app.world_mut().resource_mut::<NeoMode>().panels_open = false;
        app.update();
        assert!(app.world().resource::<WorkbenchDiff>().preview.is_none());
    }

    #[test]
    fn ledger_changes_refresh_waiting_records_without_a_document_change() {
        use kaijutsu_client::LedgerState;
        use kaijutsu_types::{AskOrigin, AskStatus, AskSummary, Status};
        use model::RecordState;

        let mut app = App::new();
        app.insert_resource(NeoMode { enabled: true, ..default() })
            .init_resource::<DocumentCache>()
            .init_resource::<LedgerMirror>()
            .init_resource::<Workspace>()
            .add_systems(Update, sync_workspace);
        let context = ContextId::from_bytes([1; 16]);
        let principal = PrincipalId::from_bytes([2; 16]);
        let call = BlockId::new(context, principal, 1);
        let result = BlockId::new(context, principal, 2);
        let mut mirror = ContextMirror::new(context);
        mirror.apply_snapshot(vec![
            BlockSnapshotBuilder::new(call, BlockKind::ToolCall).status(Status::Error).build(),
            BlockSnapshotBuilder::new(result, BlockKind::ToolResult).tool_call_id(call).is_error(true).build(),
        ], 8).unwrap();
        let mut documents = app.world_mut().resource_mut::<DocumentCache>();
        documents.insert(context, DocumentEntry::new(mirror, None, "test".to_string()));
        documents.set_active(context);
        app.update();
        assert_eq!(app.world().resource::<Workspace>().records.history[0].state, RecordState::Failed);

        let mut ask = AskSummary {
            request_id: "01a20b2c-aaaa-bbbb-cccc-000000000001".into(),
            status: AskStatus::Pending,
            origin: AskOrigin::ShellGate,
            context_id: Some(context),
            description: String::new(),
            statements: Vec::new(),
            requester: None,
            performer: None,
            reviewer: None,
            created_at_ms: 0,
            decided_at_ms: None,
            tool_call_block_id: Some(call),
            lineage_root: None,
            has_exec_source: false,
        };
        app.world_mut().resource_mut::<LedgerMirror>().apply_state(&LedgerState::from_listing(1, vec![ask.clone()]));
        app.update();
        let workspace = app.world().resource::<Workspace>();
        for row in &workspace.records.history {
            assert_eq!(row.state, RecordState::Waiting);
            assert!(row.summary.starts_with("waiting"));
        }

        ask.status = AskStatus::Denied;
        app.world_mut().resource_mut::<LedgerMirror>().apply_state(&LedgerState::from_listing(2, vec![ask]));
        app.update();
        let workspace = app.world().resource::<Workspace>();
        assert_eq!(workspace.records.history[0].state, RecordState::Failed);
        assert_eq!(workspace.records.history[0].summary, "error");
        assert_eq!(app.world().resource::<DocumentCache>().get(context).unwrap().mirror.version(), 8);
    }

    #[test]
    fn neo_actions_reveal_the_selected_source_and_leave_other_context_actions_alone() {
        check_neo_reveal(ContentType::Diff, ContentType::Diff);
    }

    #[test]
    fn neo_reveals_abc_among_linked_media() {
        check_neo_reveal(ContentType::Abc, ContentType::Media);
    }

    fn check_neo_reveal(older_type: ContentType, newer_type: ContentType) {
        let mut app = App::new();
        app.add_plugins(bevy::state::app::StatesPlugin)
            .init_state::<Screen>()
            .init_resource::<NeoMode>()
            .init_resource::<Workspace>()
            .init_resource::<DocumentCache>()
            .init_resource::<LedgerMirror>()
            .init_resource::<EditorEntities>()
            .init_resource::<FocusTarget>()
            .init_resource::<ConversationScrollState>()
            .add_message::<ActionFired>()
            .add_systems(Update, (toggle_neo, sync_workspace, handle_controls).chain());

        let context = ContextId::from_bytes([1; 16]);
        let principal = PrincipalId::from_bytes([2; 16]);
        let older = BlockId::new(context, principal, 1);
        let newer = BlockId::new(context, principal, 2);
        let snapshots = vec![
            BlockSnapshotBuilder::new(older, BlockKind::Text).content_type(older_type)
                .content("X:1\nT:Rehearsal\nK:C\nCDEF GABc|").build(),
            BlockSnapshotBuilder::new(newer, BlockKind::Text).content_type(newer_type)
                .parent_id(older).build(),
        ];
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
