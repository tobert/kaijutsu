use bevy::prelude::*;
use kaijutsu_types::BlockKind;

use crate::shaders::BlockFxMaterial;
use crate::text::msdf::MsdfBlockGlyphs;
use crate::ui::msdf_panel::{self, LayoutViewport, PanelFonts};
use crate::ui::quick_context::{LineTone, PanelLine};
use crate::ui::screen::Screen;
use crate::ui::state::{ContentArea, ConversationRoot};
use crate::ui::theme::Theme;
use crate::view::block_render::msdf_surface_bundle;
use crate::view::ui_rtt::{UiRttTexture, logical_size};

use super::{NeoMode, WorkbenchDiff, model::{RecordState, Section, Workspace}};

#[derive(Component)]
pub(super) struct NeoRail;

#[derive(Component, Default)]
pub(super) struct PanelSize(Vec2);

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum RailLayout {
    Side(f32),
    Below(f32),
}

fn rail_layout(size: Vec2) -> RailLayout {
    if size.x >= 850.0 && size.y >= 360.0 {
        RailLayout::Side((size.x * 0.34).min(500.0).max(270.0))
    } else {
        RailLayout::Below((size.y * 0.32).min(224.0))
    }
}

pub(super) fn spawn_rail(
    mut commands: Commands,
    content: Query<Entity, With<ContentArea>>,
    mut materials: ResMut<Assets<BlockFxMaterial>>,
) {
    let Ok(content) = content.single() else {
        return;
    };
    commands.entity(content).with_children(|parent| {
        parent.spawn((
            super::scene::SceneViewport,
            Name::new("Neo Scene"),
            ImageNode::default(),
            Node {
                position_type: PositionType::Absolute,
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                display: Display::None,
                ..default()
            },
        ));
        parent.spawn((
            NeoRail,
            Name::new("Neo Rail"),
            Node {
                display: Display::None,
                flex_shrink: 0.0,
                ..default()
            },
            BorderColor::default(),
        )).with_children(|rail| {
            let flex = Node {
                flex_grow: 1.0,
                flex_basis: Val::Px(0.0),
                min_width: Val::Px(0.0),
                min_height: Val::Px(0.0),
                ..default()
            };
            let mut text_panel = |section, name| (
                section,
                PanelSize::default(),
                Name::new(name),
                Node {
                    overflow: Overflow::clip(),
                    ..flex.clone()
                },
                BackgroundColor::default(),
                BorderColor::default(),
                msdf_surface_bundle(materials.add(BlockFxMaterial::default())),
            );
            for (section, name) in [(Section::Workbench, "Neo Workbench"), (Section::History, "Neo Event History")] {
                rail.spawn(text_panel(section, name));
            }
        });
    });
}

pub(super) fn sync_layout(
    mode: Res<NeoMode>,
    screen: Res<State<Screen>>,
    viewport: Option<Res<LayoutViewport>>,
    mut content: Query<&mut Node, (With<ContentArea>, Without<ConversationRoot>, Without<NeoRail>, Without<Section>)>,
    mut transcript: Query<&mut Node, (With<ConversationRoot>, Without<ContentArea>, Without<NeoRail>, Without<Section>)>,
    mut rail: Query<&mut Node, (With<NeoRail>, Without<ContentArea>, Without<ConversationRoot>, Without<Section>)>,
    mut panels: Query<(&Section, &mut Node), (Without<ContentArea>, Without<ConversationRoot>, Without<NeoRail>)>,
    mut previous: Local<Option<Option<RailLayout>>>,
) {
    let want = if mode.panels_open && *screen.get() == Screen::Conversation {
        viewport.map(|viewport| rail_layout(viewport.size))
    } else {
        None
    };
    if *previous == Some(want) {
        return;
    }
    let (Ok(mut content), Ok(mut transcript), Ok(mut rail)) =
        (content.single_mut(), transcript.single_mut(), rail.single_mut())
    else {
        return;
    };
    *previous = Some(want);
    match want {
        Some(RailLayout::Side(width)) => {
            content.flex_direction = FlexDirection::Row;
            transcript.width = Val::Auto;
            transcript.min_width = Val::Px(0.0);
            rail.display = Display::Flex;
            rail.flex_direction = FlexDirection::Column;
            rail.border = UiRect::left(px(1));
            rail.width = Val::Px(width);
            rail.height = Val::Percent(100.0);
        }
        Some(RailLayout::Below(height)) => {
            content.flex_direction = FlexDirection::Column;
            transcript.width = Val::Percent(100.0);
            transcript.min_width = Val::Auto;
            rail.display = Display::Flex;
            rail.flex_direction = FlexDirection::Row;
            rail.border = UiRect::top(px(1));
            rail.width = Val::Percent(100.0);
            rail.height = Val::Px(height);
        }
        None => {
            content.flex_direction = FlexDirection::Column;
            transcript.width = Val::Percent(100.0);
            transcript.min_width = Val::Auto;
            rail.display = Display::None;
            rail.border = UiRect::ZERO;
        }
    }
    for (section, mut node) in &mut panels {
        node.border = if *section == Section::History {
            match want {
                Some(RailLayout::Side(_)) => UiRect::top(px(1)),
                Some(RailLayout::Below(_)) => UiRect::left(px(1)),
                None => UiRect::ZERO,
            }
        } else {
            UiRect::ZERO
        };
    }
}

pub(super) fn render_panels(
    mode: Res<NeoMode>,
    screen: Res<State<Screen>>,
    workspace: Res<Workspace>,
    diff: Res<WorkbenchDiff>,
    documents: Res<crate::view::DocumentCache>,
    mut seen: Local<Option<(kaijutsu_types::ContextId, u64)>>,
    mut nearby: Local<super::diff_preview::Previews>,
    theme: Res<Theme>,
    mut fonts: PanelFonts,
    mut rail_border: Query<&mut BorderColor, (With<NeoRail>, Without<Section>)>,
    mut panels: Query<(
        &Section,
        &ComputedNode,
        &mut PanelSize,
        &mut MsdfBlockGlyphs,
        &mut UiRttTexture,
        &mut BackgroundColor,
        &mut BorderColor,
    ), (With<Section>, Without<NeoRail>)>,
) {
    if let Ok(mut border) = rail_border.single_mut() {
        border.set_if_neq(BorderColor::all(theme.border));
    }
    if !mode.panels_open || mode.enabled || *screen.get() != Screen::Conversation {
        *seen = None;
        return;
    }
    for (_, _, _, _, _, mut bg, mut border) in &mut panels {
        bg.set_if_neq(BackgroundColor(theme.panel_bg));
        border.set_if_neq(BorderColor::all(theme.border));
    }
    let Some(font) = fonts.fonts.get(&fonts.handles.mono) else {
        return;
    };
    let Some(atlas) = fonts.atlas.as_mut() else {
        return;
    };
    let mirror = workspace.context.and_then(|id| documents.get(id)).map(|entry| &entry.mirror);
    let stamp = mirror.map(|mirror| (mirror.context_id(), mirror.version()));
    let redraw = *seen != stamp || mode.is_changed() || screen.is_changed() || theme.is_changed() || workspace.is_changed() || diff.is_changed();
    *seen = stamp;
    if workspace.is_changed() { nearby.retain(|id, _| workspace.diffs().any(|row| row.id == *id)); }

    for (section, computed, mut previous, mut output, mut texture, _, _) in &mut panels {
        let size = logical_size(computed);
        if size.min_element() <= 1.0 || (size == previous.0 && !redraw) {
            continue;
        }

        let cols = msdf_panel::columns(size.x as f64, msdf_panel::measure_char_width(font));
        let max_rows = msdf_panel::rows_for_height(size.y as f64);
        let rows = panel_rows(&workspace, *section, cols, max_rows, diff.preview.as_ref(), mirror, &mut nearby);
        let (glyphs, _) = msdf_panel::collect_panel_glyphs(
            &rows,
            &theme,
            font,
            atlas,
            &mut fonts.font_data_map,
        );
        output.glyphs = glyphs;
        output.version = output.version.wrapping_add(1).max(1);
        texture.built_width = size.x;
        texture.built_height = size.y;
        previous.0 = size;
    }
}

#[cfg(test)]
mod tile_tests {
    use super::*;

    #[test]
    fn rail_uses_single_rules_in_both_orientations() {
        let mut app = App::new();
        app.add_plugins(bevy::state::app::StatesPlugin)
            .init_state::<Screen>()
            .init_resource::<Assets<BlockFxMaterial>>()
            .insert_resource(NeoMode { enabled: false, panels_open: true })
            .insert_resource(LayoutViewport { size: Vec2::new(1200.0, 720.0) })
            .add_systems(Startup, spawn_rail)
            .add_systems(Update, sync_layout);
        app.world_mut().spawn((ContentArea, Node::default()));
        app.world_mut().spawn((ConversationRoot, Node::default()));
        app.update();
        let inspect = |app: &mut App, rail_border: UiRect, second_border: UiRect| {
            let world = app.world_mut();
            let rail = world.query_filtered::<&Node, With<NeoRail>>().single(world).unwrap();
            assert_eq!(rail.border, rail_border);
            assert_eq!(rail.padding, UiRect::ZERO);
            assert_eq!(rail.row_gap, Val::ZERO);
            assert_eq!(rail.column_gap, Val::ZERO);
            let mut panels = world.query::<(&Section, &Node)>();
            for (section, node) in panels.iter(world) {
                assert_eq!(node.border, if *section == Section::History { second_border } else { UiRect::ZERO });
            }
            assert_eq!(world.query_filtered::<Entity, With<super::super::scene::FocusCorner>>().iter(world).count(), 0);
        };
        inspect(&mut app, UiRect::left(px(1)), UiRect::top(px(1)));
        app.world_mut().resource_mut::<LayoutViewport>().size = Vec2::new(640.0, 600.0);
        app.update();
        inspect(&mut app, UiRect::top(px(1)), UiRect::left(px(1)));
    }
}

fn panel_rows(
    workspace: &Workspace,
    section: Section,
    columns: usize,
    max_rows: usize,
    preview: Option<&crate::text::diff::DiffPreview>,
    mirror: Option<&kaijutsu_client::ContextMirror>,
    nearby: &mut super::diff_preview::Previews,
) -> Vec<PanelLine> {
    if columns == 0 || max_rows == 0 { return Vec::new(); }
    let active = workspace.section == section;
    let title = match section {
        Section::Workbench => "ARTIFACT WORKBENCH",
        Section::History => "EVENT HISTORY",
    };
    let add = |lines: &mut Vec<PanelLine>, text: &str, tone| {
        lines.push(PanelLine { text: msdf_panel::truncate(text, columns), tone });
    };
    let mut lines = Vec::with_capacity(max_rows.min(52));
    add(&mut lines, title, if active { LineTone::Head } else { LineTone::Dim });
    if max_rows <= 1 {
        return lines;
    }

    let records = workspace.records.in_section(section);
    let source = match section {
        Section::Workbench => workspace.workbench_source(),
        Section::History => workspace.history_source().map(|row| row.id),
    };
    let selected = records.iter().position(|record| Some(record.id) == source);
    if section == Section::Workbench && let (Some(preview), Some(index)) = (preview, selected) {
        lines[0].text = msdf_panel::truncate(&format!("WORKBENCH   {} / {} · Diff", index + 1, records.len()), columns);
        let footer = active && max_rows >= 5;
        let state = &records[index].state;
        let status_tone = match state {
            RecordState::Waiting => LineTone::Warn,
            RecordState::Failed => LineTone::Bad,
            RecordState::Normal => LineTone::Dim,
        };
        let show_status = *state != RecordState::Normal || records[index].excluded;
        if show_status {
            add(&mut lines, &records[index].summary, status_tone);
        }
        let show_origin = max_rows >= lines.len() + 4 + usize::from(footer);
        let budget = max_rows.saturating_sub(lines.len() + usize::from(footer) + usize::from(show_origin));
        lines.extend(super::diff_preview::rows(preview, columns, budget));
        if show_origin {
            let origin = match workspace.artifact_receipt(records[index].id) {
                Some(call) if call.kind == BlockKind::ToolCall => format!("From · {}", call.title.trim_start_matches("Call  ")),
                Some(receipt) if receipt.call_id.is_some() => format!("From · {}", receipt.title.trim_start_matches("Result  ")),
                Some(_) => "Source · tool result".into(),
                None => "No linked result in recent history".into(),
            };
            add(&mut lines, &origin, LineTone::Dim);
        }
        if footer {
            let neighbors = [("[", index.checked_sub(1)), ("]", Some(index + 1))];
            let count = neighbors.iter().filter(|(_, i)| i.is_some_and(|i| i < records.len())).count();
            let show_neighbors = count > 0 && max_rows >= lines.len() + count + 3 && mirror.is_some();
            if show_neighbors && let Some(mirror) = mirror {
                add(&mut lines, "", LineTone::Dim);
                add(&mut lines, "Nearby artifacts", LineTone::Head);
                for (key, neighbor) in neighbors {
                    let Some(row) = neighbor.and_then(|i| records.get(i)) else { continue };
                    let name = if row.content_type == kaijutsu_types::ContentType::Diff {
                        super::diff_preview::preview(mirror, row.id, nearby).and_then(super::diff_preview::first_file)
                    } else { None };
                    add(&mut lines, &format!("{key} {}", name.unwrap_or(&row.title)), LineTone::Row);
                }
            }
            let hint = if show_neighbors { "\\ history  Shift+Enter source" } else { "[] select  \\ history  Shift+Enter source" };
            add(&mut lines, hint, LineTone::Dim);
        }
        return lines;
    }
    if section == Section::History && !active && let Some(index) = selected {
        let title = if records[index].kind == BlockKind::ToolCall {
            "EVENT HISTORY · Linked call"
        } else {
            "EVENT HISTORY · Linked result"
        };
        lines[0].text = msdf_panel::truncate(title, columns);
    }
    let footer_rows = if active && max_rows >= 4 { 1 } else { 0 };
    let mut detail = Vec::new();
    if let Some(index) = selected && max_rows >= 4 {
        let row = &records[index];
        if section == Section::History && let Some(mirror) = mirror {
            let matching = workspace.event_diff(row).filter(|id| Some(*id) == workspace.workbench_source()).and(preview);
            detail = super::cards::event_detail(mirror, workspace, row, matching, columns.saturating_sub(2));
        } else if !row.summary.is_empty() {
            detail.push(PanelLine { text: row.summary.clone(), tone: LineTone::Dim });
        }
    }
    let between = selected.filter(|_| section == Section::History && max_rows >= 6).and_then(|index| {
        let call = records[index].call_id?;
        let origin = records.iter().position(|row| row.id == call)?;
        index.abs_diff(origin).checked_sub(1).filter(|&count| count > 0)
    });
    let available = max_rows - 1 - footer_rows - usize::from(between.is_some());
    detail.truncate(available.saturating_sub(2).min(4));
    let body_rows = available - detail.len();
    let start = selected.map_or(0, |index| index.saturating_sub(body_rows / 2))
        .min(records.len().saturating_sub(body_rows));
    for (index, record) in records.iter().enumerate().skip(start).take(body_rows) {
        let focused = selected == Some(index);
        let linked = section == Section::History && selected.is_some_and(|i| {
            records[i].call_id == Some(record.id) || record.call_id == Some(records[i].id)
        });
        let tone = match record.state {
            RecordState::Waiting => LineTone::Warn,
            RecordState::Failed => LineTone::Bad,
            RecordState::Normal if focused || linked => LineTone::Head,
            RecordState::Normal if section == Section::History && index >= 3 => LineTone::Dim,
            RecordState::Normal => LineTone::Row,
        };
        let marker = if focused { "> " } else if linked { "< " } else { "  " };
        add(&mut lines, &format!("{marker}{}", record.title), tone);
        if focused {
            for line in &detail {
                add(&mut lines, &format!("  {}", line.text), line.tone);
            }
        }
    }
    if records.is_empty() {
        let text = match (workspace.context, workspace.ready, section) {
            (None, _, _) => "No context joined",
            (_, false, _) => "Loading context…",
            (_, true, Section::Workbench) => "No recent artifacts",
            (_, true, Section::History) => "No recent events",
        };
        add(&mut lines, text, LineTone::Dim);
    }
    if let Some(count) = between {
        let events = if count == 1 { "event" } else { "events" };
        add(&mut lines, &format!("{count} {events} between call and result"), LineTone::Dim);
    }
    if footer_rows > 0 {
        let hint = if selected.is_some() { "Shift+Enter source" } else { "[] select  \\ switch" };
        add(&mut lines, hint, LineTone::Dim);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::neo::model::Record;
    use kaijutsu_types::{BlockId, ContextId, PrincipalId};

    #[test]
    fn wide_rail_leaves_room_for_dialogue_and_narrow_rail_moves_below() {
        assert_eq!(rail_layout(Vec2::new(1_000.0, 720.0)), RailLayout::Side(340.0));
        assert_eq!(rail_layout(Vec2::new(2_560.0, 1_440.0)), RailLayout::Side(500.0));
        assert_eq!(rail_layout(Vec2::new(640.0, 600.0)), RailLayout::Below(192.0));
    }

    #[test]
    fn switching_section_and_view_preserves_rail_width() {
        let mut app = App::new();
        app.add_plugins(bevy::state::app::StatesPlugin)
            .init_state::<Screen>()
            .insert_resource(NeoMode { enabled: true, panels_open: true })
            .init_resource::<Workspace>()
            .insert_resource(LayoutViewport { size: Vec2::new(1_920.0, 1_080.0) })
            .add_systems(Update, sync_layout);
        app.world_mut().spawn((ContentArea, Node::default()));
        app.world_mut().spawn((ConversationRoot, Node::default()));
        let rail = app.world_mut().spawn((NeoRail, Node::default())).id();
        app.update();
        let initial = app.world().get::<Node>(rail).unwrap().width;
        app.world_mut().resource_mut::<Workspace>().switch_section();
        app.update();
        assert_eq!(app.world().get::<Node>(rail).unwrap().width, initial);
        app.world_mut().resource_mut::<NeoMode>().enabled = false;
        app.update();
        assert_eq!(app.world().get::<Node>(rail).unwrap().width, initial);
        app.world_mut().resource_mut::<Workspace>().switch_section();
        app.update();
        assert_eq!(app.world().get::<Node>(rail).unwrap().width, initial);
    }

    #[test]
    fn scene_overlays_the_transcript_and_history_is_a_text_panel() {
        let mut app = App::new();
        app.init_resource::<Assets<BlockFxMaterial>>().add_systems(Startup, spawn_rail);
        let content = app.world_mut().spawn(ContentArea).id();
        app.update();
        let world = app.world_mut();
        let (scene, parent) = world.query_filtered::<(&Node, &ChildOf), With<super::super::scene::SceneViewport>>()
            .single(world).unwrap();
        assert_eq!(parent.parent(), content);
        assert_eq!(scene.position_type, PositionType::Absolute);
        assert_eq!(scene.width, Val::Percent(100.0));
        assert_eq!(scene.height, Val::Percent(100.0));
        assert!(world.query::<&Section>().iter(world).any(|section| *section == Section::History));
    }

    #[test]
    fn vi_screen_round_trip_hides_the_rail_and_toggle_restores_the_transcript() {
        let mut app = App::new();
        app.add_plugins(bevy::state::app::StatesPlugin)
            .init_state::<Screen>()
            .insert_resource(NeoMode { enabled: true, panels_open: true })
            .init_resource::<Workspace>()
            .insert_resource(LayoutViewport { size: Vec2::new(1_280.0, 720.0) })
            .add_systems(Update, sync_layout);
        let content = app.world_mut().spawn((ContentArea, Node::default())).id();
        let root = app.world_mut().spawn((ConversationRoot, Node::default())).id();
        let rail = app.world_mut().spawn((NeoRail, Node::default())).id();
        let snapshot = |app: &App| (
            app.world().get::<Node>(content).unwrap().flex_direction,
            app.world().get::<Node>(root).unwrap().width,
            app.world().get::<Node>(rail).unwrap().display,
        );

        app.update();
        assert_eq!(snapshot(&app), (FlexDirection::Row, Val::Auto, Display::Flex));
        app.world_mut().resource_mut::<NextState<Screen>>().set(Screen::Diff);
        app.update();
        assert_eq!(snapshot(&app), (FlexDirection::Column, Val::Percent(100.0), Display::None));
        assert!(app.world().resource::<NeoMode>().enabled);

        app.world_mut().resource_mut::<NextState<Screen>>().set(Screen::Conversation);
        app.update();
        assert_eq!(snapshot(&app), (FlexDirection::Row, Val::Auto, Display::Flex));
        app.world_mut().resource_mut::<NeoMode>().enabled = false;
        app.update();
        assert_eq!(snapshot(&app), (FlexDirection::Row, Val::Auto, Display::Flex));
        app.world_mut().resource_mut::<NeoMode>().panels_open = false;
        app.update();
        assert_eq!(snapshot(&app), (FlexDirection::Column, Val::Percent(100.0), Display::None));
    }

    #[test]
    fn history_marks_only_the_linked_call_across_an_intervening_event() {
        let context = ContextId::from_bytes([1; 16]);
        let id = |seq| BlockId::new(context, PrincipalId::from_bytes([2; 16]), seq);
        let mut workspace = Workspace::default();
        workspace.switch_section();
        for (seq, title, call_id) in [(3, "Result  first", Some(id(1))), (2, "Call  second", None), (1, "Call  first", None)] {
            workspace.records.history.push(Record {
                kind: if call_id.is_some() { kaijutsu_types::BlockKind::ToolResult } else { kaijutsu_types::BlockKind::ToolCall },
                id: id(seq), title: title.into(), call_id, artifact_id: None, excluded: false, content_type: kaijutsu_types::ContentType::Plain,
                status: kaijutsu_types::Status::Done, state: RecordState::Normal, summary: "done".into(),
            });
        }
        let rows = panel_rows(&workspace, Section::History, 50, 10, None, None, &mut default());
        assert!(rows.iter().any(|row| row.text == "< Call  first"));
        assert!(rows.iter().any(|row| row.text == "  Call  second"));
        assert!(rows.iter().any(|row| row.text.contains("1 event between call and result")));
    }

    #[test]
    fn event_panel_displays_the_selected_patches_receipt_and_independent_command_outcomes() {
        use kaijutsu_types::{BlockKind, BlockSnapshotBuilder, ContentType, Status};
        use super::super::{model::Records, projection::DetectedDiffs};
        let context = ContextId::from_bytes([7; 16]);
        let id = |seq| BlockId::new(context, PrincipalId::from_bytes([4; 16]), seq);
        let body = "--- a/harbor\n+++ b/harbor\n@@ -1 +1 @@\n-old\n+new";
        let mut mirror = kaijutsu_client::ContextMirror::new(context);
        mirror.apply_snapshot(vec![
            BlockSnapshotBuilder::new(id(1), BlockKind::ToolCall).tool_name("diff_block").build(),
            BlockSnapshotBuilder::new(id(2), BlockKind::Text).content_type(ContentType::Diff).content(body).build(),
            BlockSnapshotBuilder::new(id(3), BlockKind::ToolResult).tool_call_id(id(1)).status(Status::Done)
                .content(format!(r#"{{"block_id":"{}"}}"#, id(2).to_key())).build(),
            BlockSnapshotBuilder::new(id(4), BlockKind::ToolCall).tool_name("check").build(),
            BlockSnapshotBuilder::new(id(5), BlockKind::ToolResult).tool_call_id(id(4)).is_error(true)
                .exit_code(3).content("missing orchard").build(),
            BlockSnapshotBuilder::new(id(6), BlockKind::Text).content_type(ContentType::Diff).content(body).build(),
        ], 8).unwrap();
        let mut ws = Workspace::default();
        ws.reconcile(Some(context), Some(Records::from_mirror(&mirror, &default(), &mut DetectedDiffs::default())));
        let diff = crate::text::diff::build_diff_preview(body, ContentType::Diff);
        let render = |workspace: &Workspace, section, height| panel_rows(workspace, section, 42, height, Some(&diff), Some(&mirror), &mut default());
        assert!(render(&ws, Section::Workbench, 8).iter().any(|r| r.text.contains("No linked result")));
        assert!(!render(&ws, Section::History, 8)[0].text.contains("Linked"));
        ws.step(true);
        let rows = render(&ws, Section::History, 8);
        assert!(rows[0].text.contains("Linked result"));
        assert!(rows.iter().any(|r| r.text.starts_with("> Result") && r.text.contains("diff_block")));
        assert!(rows.iter().any(|r| r.text.contains("Patch · 1 file")));
        assert!(render(&ws, Section::Workbench, 8).iter().any(|r| r.text == "From · diff_block"));
        ws.switch_section();
        ws.selected = Some(id(5));
        let rows = render(&ws, Section::History, 9);
        assert!(rows.iter().any(|r| r.text.trim() == "exit 3" && r.tone == LineTone::Bad));
        assert!(rows.iter().any(|r| r.text.contains("missing orchard")));
        assert!(!rows.iter().any(|r| r.text.contains("Patch ·")));
        for height in 0..=12 {
            for section in [Section::History, Section::Workbench] {
                assert!(render(&ws, section, height).len() <= height);
            }
        }
    }

    #[test]
    fn roomy_workbench_names_the_artifacts_on_either_side_of_the_patch() {
        use kaijutsu_types::{BlockKind, BlockSnapshotBuilder, ContentType};
        use super::super::{model::Records, projection::DetectedDiffs};
        let context = ContextId::from_bytes([5; 16]);
        let mut mirror = kaijutsu_client::ContextMirror::new(context);
        let blocks = ["pier", "harbor", "orchard"].iter().enumerate().map(|(i, name)| {
            let id = BlockId::new(context, PrincipalId::from_bytes([4; 16]), i as u64);
            BlockSnapshotBuilder::new(id, BlockKind::Text).content_type(ContentType::Diff)
                .content(format!("--- a/{name}\n+++ b/{name}\n@@ -1 +1 @@\n-a\n+b")).build()
        }).collect();
        mirror.apply_snapshot(blocks, 1).unwrap();
        let mut ws = Workspace::default();
        ws.reconcile(Some(context), Some(Records::from_mirror(&mirror, &default(), &mut DetectedDiffs::default())));
        ws.step(true);
        let preview = crate::text::diff::build_diff_preview(&mirror.blocks()[1].content, ContentType::Diff);
        let render = |height| panel_rows(&ws, Section::Workbench, 42, height, Some(&preview), Some(&mirror), &mut default());
        let wide = render(20);
        assert!(wide.iter().any(|r| r.text == "[ orchard"));
        assert!(wide.iter().any(|r| r.text == "] pier"));
        let short = render(6);
        assert!(!short.iter().any(|r| r.text == "[ orchard"));
        assert!(short.iter().any(|r| r.text.contains("harbor")));
    }

    #[test]
    fn a_short_panel_keeps_the_selected_record_visible() {
        let mut workspace = Workspace::default();
        let context = ContextId::from_bytes([1; 16]);
        let principal = PrincipalId::from_bytes([2; 16]);
        for i in 1..=10 {
            workspace.records.workbench.push(Record {
                id: BlockId::new(context, principal, i),
                kind: kaijutsu_types::BlockKind::Text,
                call_id: None,
                artifact_id: None,
                excluded: false,
                content_type: kaijutsu_types::ContentType::Plain,
                title: format!("Article {i}"),
                summary: String::new(),
                status: kaijutsu_types::Status::Done,
                state: RecordState::Normal,
            });
        }
        workspace.selected = Some(BlockId::new(context, principal, 10));
        let lines = panel_rows(&workspace, Section::Workbench, 30, 4, None, None, &mut default());
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[2].text, "> Article 10");
    }
}
