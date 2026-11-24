use bevy::prelude::*;

use crate::shaders::BlockFxMaterial;
use crate::text::msdf::MsdfBlockGlyphs;
use crate::ui::msdf_panel::{self, LayoutViewport, PanelFonts};
use crate::ui::quick_context::{LineTone, PanelLine};
use crate::ui::screen::Screen;
use crate::ui::state::{ContentArea, ConversationRoot};
use crate::ui::theme::Theme;
use crate::view::block_render::msdf_surface_bundle;
use crate::view::ui_rtt::{UiRttTexture, logical_size};

use super::{NeoMode, model::{RecordState, Section, Workspace}};

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
        RailLayout::Side((size.x * 0.30).clamp(270.0, 396.0))
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
            NeoRail,
            Name::new("Neo Rail"),
            Node {
                display: Display::None,
                flex_shrink: 0.0,
                padding: UiRect::all(Val::Px(10.0)),
                row_gap: Val::Px(10.0),
                column_gap: Val::Px(10.0),
                ..default()
            },
        )).with_children(|rail| {
            for (section, name) in [
                (Section::Workbench, "Neo Workbench"),
                (Section::History, "Neo Event History"),
            ] {
                rail.spawn((
                    section,
                    PanelSize::default(),
                    Name::new(name),
                    Node {
                        flex_grow: 1.0,
                        flex_basis: Val::Px(0.0),
                        min_width: Val::Px(0.0),
                        min_height: Val::Px(0.0),
                        border: UiRect::all(Val::Px(1.0)),
                        border_radius: BorderRadius::all(Val::Px(6.0)),
                        overflow: Overflow::clip(),
                        ..default()
                    },
                    BackgroundColor::default(),
                    BorderColor::default(),
                    msdf_surface_bundle(materials.add(BlockFxMaterial::default())),
                ));
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
    mut sections: Query<(&Section, &mut Node), (Without<NeoRail>, Without<ContentArea>, Without<ConversationRoot>)>,
    mut previous: Local<Option<Option<RailLayout>>>,
) {
    let want = if mode.enabled && *screen.get() == Screen::Conversation {
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
            rail.width = Val::Px(width);
            rail.height = Val::Percent(100.0);
        }
        Some(RailLayout::Below(height)) => {
            content.flex_direction = FlexDirection::Column;
            transcript.width = Val::Percent(100.0);
            transcript.min_width = Val::Auto;
            rail.display = Display::Flex;
            rail.flex_direction = FlexDirection::Row;
            rail.width = Val::Percent(100.0);
            rail.height = Val::Px(height);
        }
        None => {
            content.flex_direction = FlexDirection::Column;
            transcript.width = Val::Percent(100.0);
            transcript.min_width = Val::Auto;
            rail.display = Display::None;
        }
    }
    for (section, mut node) in &mut sections {
        if *section == Section::History {
            // The history sits one step behind the workbench.
            node.margin = match want {
                Some(RailLayout::Side(_)) => UiRect::left(Val::Px(12.0)),
                Some(RailLayout::Below(_)) => UiRect::top(Val::Px(8.0)),
                None => UiRect::ZERO,
            };
        }
    }
}

pub(super) fn render_panels(
    mode: Res<NeoMode>,
    screen: Res<State<Screen>>,
    workspace: Res<Workspace>,
    theme: Res<Theme>,
    mut fonts: PanelFonts,
    mut panels: Query<(
        &Section,
        &ComputedNode,
        &mut PanelSize,
        &mut MsdfBlockGlyphs,
        &mut UiRttTexture,
        &mut BackgroundColor,
        &mut BorderColor,
    )>,
) {
    if !mode.enabled || *screen.get() != Screen::Conversation {
        return;
    }
    let Some(font) = fonts.fonts.get(&fonts.handles.mono) else {
        return;
    };
    let Some(atlas) = fonts.atlas.as_mut() else {
        return;
    };
    let redraw = mode.is_changed() || screen.is_changed() || theme.is_changed() || workspace.is_changed();

    for (section, computed, mut previous, mut output, mut texture, mut bg, mut border) in &mut panels {
        let size = logical_size(computed);
        if size.min_element() <= 1.0 || (size == previous.0 && !redraw) {
            continue;
        }

        let cols = msdf_panel::columns(size.x as f64, msdf_panel::measure_char_width(font));
        let max_rows = msdf_panel::rows_for_height(size.y as f64);
        let rows = panel_rows(&workspace, *section, cols, max_rows);
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

        bg.0 = theme.panel_bg.with_alpha(if *section == Section::Workbench { 1.0 } else { 0.66 });
        *border = BorderColor::all(if workspace.section == *section {
            theme.accent.with_alpha(theme.accent.alpha() * 0.46)
        } else {
            theme.border
        });
    }
}

fn panel_rows(
    workspace: &Workspace,
    section: Section,
    columns: usize,
    max_rows: usize,
) -> Vec<PanelLine> {
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
    let selected = records.iter().position(|record| active && Some(record.id) == workspace.selected);
    let footer_rows = if active && max_rows >= 4 { 1 } else { 0 };
    let detail_rows = if selected.is_some() && max_rows >= 5 { 1 } else { 0 };
    let body_rows = max_rows - 1 - footer_rows - detail_rows;
    let start = selected.map_or(0, |index| index.saturating_sub(body_rows - 1));
    for (index, record) in records.iter().enumerate().skip(start).take(body_rows) {
        let focused = selected == Some(index);
        let tone = match record.state {
            RecordState::Waiting => LineTone::Warn,
            RecordState::Failed => LineTone::Bad,
            RecordState::Normal if focused => LineTone::Head,
            RecordState::Normal if section == Section::History && index >= 3 => LineTone::Dim,
            RecordState::Normal => LineTone::Row,
        };
        let marker = if focused { "> " } else { "  " };
        add(&mut lines, &format!("{marker}{}", record.title), tone);
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
    if detail_rows > 0 && let Some(index) = selected {
        add(&mut lines, &records[index].summary, LineTone::Dim);
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
        assert_eq!(rail_layout(Vec2::new(1_280.0, 720.0)), RailLayout::Side(384.0));
        assert_eq!(rail_layout(Vec2::new(2_560.0, 1_440.0)), RailLayout::Side(396.0));
        assert_eq!(rail_layout(Vec2::new(640.0, 600.0)), RailLayout::Below(192.0));
    }

    #[test]
    fn vi_screen_round_trip_hides_the_rail_and_toggle_restores_the_transcript() {
        let mut app = App::new();
        app.add_plugins(bevy::state::app::StatesPlugin)
            .init_state::<Screen>()
            .insert_resource(NeoMode { enabled: true })
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
        assert_eq!(snapshot(&app), (FlexDirection::Column, Val::Percent(100.0), Display::None));
    }

    #[test]
    fn a_short_panel_keeps_the_selected_record_visible() {
        let mut workspace = Workspace::default();
        let context = ContextId::from_bytes([1; 16]);
        let principal = PrincipalId::from_bytes([2; 16]);
        for i in 1..=10 {
            workspace.records.workbench.push(Record {
                id: BlockId::new(context, principal, i),
                title: format!("Article {i}"),
                summary: String::new(),
                state: RecordState::Normal,
            });
        }
        workspace.selected = Some(BlockId::new(context, principal, 10));
        let lines = panel_rows(&workspace, Section::Workbench, 30, 4);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[2].text, "> Article 10");
    }
}
