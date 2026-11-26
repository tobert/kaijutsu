//! Native UI for the Neo board explorer.

use bevy::prelude::*;
use bevy::text::LineBreak;
use bevy::window::PrimaryWindow;

use crate::connection::RpcActor;
use crate::constants::ZLayer;
use crate::input::{Action, InputPhase, context::{ActiveInputContexts, InputContext}, events::ActionFired};
use crate::ui::{quick_context::format_age, screen::Screen, theme::Theme};

use super::board_feed::{BoardFeed, BoardPost};
use super::boards::{BoardLayer, BoardNav};
use super::ui_rtt::logical_size;

pub struct BoardScenePlugin;

impl Plugin for BoardScenePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, (render, scroll_posts).chain().after(InputPhase::Dispatch)
            .run_if(in_state(Screen::Boards)))
            .add_systems(OnExit(Screen::Boards), close);
    }
}

#[derive(Component)]
struct BoardScene;

#[derive(Component)]
struct PostScroll(String, usize);

fn close(mut commands: Commands, roots: Query<Entity, With<BoardScene>>) {
    for root in &roots {
        commands.entity(root).despawn();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Layout {
    stacked: bool,
    short: bool,
    compact: bool,
}

impl Layout {
    fn gap(self) -> f32 {
        if self.stacked || self.short || self.compact { 14.0 } else { 26.0 }
    }
}

struct Palette {
    bg: Color,
    panel: Color,
    card: Color,
    line: Color,
    fg: Color,
    muted: Color,
    cyan: Color,
    amber: Color,
}

impl Palette {
    fn new(theme: &Theme) -> Self {
        let cyan = Color::srgb(0.33, 0.97, 0.93);
        let base = Color::srgb(0.031, 0.046, 0.078);
        let bg = base.mix(&theme.bg, 0.14);
        Self {
            bg,
            panel: bg.mix(&Color::srgb(0.15, 0.20, 0.29), 0.20),
            card: bg.mix(&Color::srgb(0.15, 0.22, 0.32), 0.38),
            line: bg.mix(&cyan, 0.26),
            fg: Color::srgb(0.94, 0.97, 1.0),
            muted: Color::srgb(0.65, 0.75, 0.82),
            cyan,
            amber: Color::srgb(1.0, 0.78, 0.42),
        }
    }
}

fn column() -> Node {
    Node {
        flex_direction: FlexDirection::Column,
        min_width: Val::Px(0.0),
        ..default()
    }
}

fn fade(from: Color, to: Color) -> BackgroundGradient {
    LinearGradient::to_right(vec![ColorStop::auto(from), ColorStop::auto(to)]).into()
}

struct Lettering {
    body: Handle<Font>,
    mono: Handle<Font>,
}

impl Lettering {
    fn new(assets: &AssetServer) -> Self {
        Self {
            body: assets.load("fonts/NotoSansCJKJP-Light.ttf"),
            mono: assets.load("fonts/CascadiaCodeNF.ttf"),
        }
    }

    fn text(&self, text: impl Into<String>, size: f32, color: Color) -> impl Bundle {
        label(text, self.body.clone(), size, color, false)
    }

    fn signal(&self, text: impl Into<String>, size: f32, color: Color) -> impl Bundle {
        label(text, self.mono.clone(), size, color, true)
    }
}

fn label(text: impl Into<String>, font: Handle<Font>, size: f32, color: Color, fixed: bool) -> impl Bundle {
    (
        Text::new(text),
        TextFont::from_font_size(size).with_font(font),
        TextColor(color),
        if fixed { TextLayout::no_wrap() } else { TextLayout::linebreak(LineBreak::WordOrCharacter) },
        Node {
            min_width: Val::Px(0.0),
            flex_shrink: 0.0,
            overflow: Overflow::clip(),
            ..default()
        },
    )
}

fn header(
    parent: &mut ChildSpawnerCommands, font: &Lettering, colors: &Palette, layout: Layout,
    nav: &BoardNav, feed: &BoardFeed,
) {
    parent.spawn((
        Node {
            min_height: Val::Px(if layout.short { 65.0 } else { 98.0 }),
            flex_shrink: 0.0,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::SpaceBetween,
            padding: UiRect::horizontal(Val::Px(if layout.stacked { 18.0 } else { 28.0 })),
            border: UiRect { left: Val::Px(4.0), bottom: Val::Px(1.0), ..default() },
            overflow: Overflow::clip(),
            ..default()
        },
        BorderColor { left: colors.cyan, ..BorderColor::all(colors.line) },
        fade(colors.bg.mix(&colors.cyan, 0.13), colors.panel),
    )).with_children(|header| {
        header.spawn(font.signal("NEO / BOARDS", if layout.compact { 25.0 } else if layout.short || layout.stacked { 30.0 } else { 45.0 }, colors.fg));
        if layout.stacked || layout.compact { return; }
        let status = match nav.layer() {
            BoardLayer::Boards => feed.topics().first().map(|topic| {
                ("LATEST ACTIVITY", format!("{}  ·  {}", topic.board(), topic.age))
            }),
            BoardLayer::Topics => nav.board().map(|board| ("TOPICS", board.to_string())),
            BoardLayer::Posts => nav.board().map(|board| ("POSTS", board.to_string())),
        };
        if let Some((heading, path)) = status {
            header.spawn((
                Node {
                    width: Val::Percent(38.0),
                    border: UiRect::left(Val::Px(2.0)),
                    padding: UiRect::left(Val::Px(14.0)),
                    margin: UiRect::left(Val::Px(16.0)),
                    row_gap: Val::Px(3.0),
                    ..column()
                },
                BorderColor::all(colors.line),
            )).with_children(|status| {
                status.spawn(font.signal(heading, 15.0, colors.cyan));
                status.spawn(font.text(path, 20.0, colors.fg));
            });
        }
    });
}

fn panel(selected: bool, grow: f32, layout: Layout, colors: &Palette) -> impl Bundle {
    (
        Node {
            flex_grow: grow,
            flex_basis: Val::Px(0.0),
            min_height: Val::Px(0.0),
            row_gap: Val::Px(if layout.short { 9.0 } else { 14.0 }),
            border: UiRect { top: Val::Px(2.0), ..UiRect::all(Val::Px(1.0)) },
            padding: UiRect::all(Val::Px(layout.gap())),
            overflow: Overflow::clip(),
            ..column()
        },
        BackgroundColor(colors.panel),
        BorderColor { top: if selected { colors.cyan } else { colors.line }, ..BorderColor::all(colors.line) },
    )
}

fn rule(parent: &mut ChildSpawnerCommands, colors: &Palette) {
    parent.spawn((
        Node { height: Val::Px(1.0), flex_shrink: 0.0, ..default() },
        fade(colors.line, colors.panel),
    ));
}

fn post_count(count: usize) -> String {
    format!("{count} {}", if count == 1 { "post" } else { "posts" })
}

fn post_age(created_at: u64, now_ms: Option<i64>) -> Option<String> {
    if created_at == 0 { return None; }
    let delta = now_ms?.saturating_sub(i64::try_from(created_at).ok()?);
    Some(format_age(delta))
}

fn post_card(parent: &mut ChildSpawnerCommands, post: &BoardPost, font: &Lettering, colors: &Palette, now_ms: Option<i64>) {
    let mut source = format!("#{}", post.number);
    if !post.author.is_empty() { source.push_str(&format!("  {}", post.author)); }
    if let Some(age) = post_age(post.created_at, now_ms) { source.push_str(&format!("  ·  {age}")); }
    parent.spawn((
        Node {
            flex_shrink: 0.0,
            row_gap: Val::Px(10.0),
            padding: UiRect::all(Val::Px(18.0)),
            border: UiRect::left(Val::Px(2.0)),
            ..column()
        },
        BackgroundColor(colors.card),
        BorderColor::all(colors.line),
    )).with_children(|card| {
        card.spawn(font.text(source, 18.0, colors.cyan));
        card.spawn(font.text(post.body.as_str(), 24.0, colors.fg));
    });
}

fn render(
    mut commands: Commands,
    roots: Query<Entity, With<BoardScene>>,
    post_scroll: Query<(&PostScroll, &ScrollPosition)>,
    windows: Query<&Window, With<PrimaryWindow>>,
    scale: Res<UiScale>,
    assets: Res<AssetServer>,
    theme: Res<Theme>,
    nav: Res<BoardNav>,
    feed: Res<BoardFeed>,
    actor: Option<Res<RpcActor>>,
    mut previous_layout: Local<Option<Layout>>,
) {
    let Ok(window) = windows.single() else { return; };
    let width = window.width() / scale.0;
    let height = window.height() / scale.0;
    let compact = width < 540.0 || (width < 840.0 && height < 580.0);
    let layout = Layout { stacked: width < 840.0 && !compact, short: height < 680.0, compact };
    if !roots.is_empty() && !nav.is_changed() && !feed.is_changed() && !theme.is_changed()
        && *previous_layout == Some(layout)
    {
        return;
    }
    *previous_layout = Some(layout);
    for root in &roots {
        commands.entity(root).despawn();
    }

    let colors = Palette::new(&theme);
    let font = Lettering::new(&assets);
    let now_ms = actor.map(|actor| (actor.handle.kernel_now_ns() / 1_000_000) as i64);
    let topic = nav.topic(&feed);
    let post = nav.post(&feed);
    let detail = topic.zip(post);
    let scroll_y = post_scroll.single().ok().filter(|(source, _)|
        detail.is_some_and(|(topic, post)| topic.id == source.0 && post.number == source.1))
        .map_or(0.0, |(_, scroll)| scroll.y);

    commands.spawn((
        BoardScene,
        Name::new("Neo Boards"),
        GlobalZIndex(ZLayer::CONTENT),
        Node {
            position_type: PositionType::Absolute,
            width: Val::Percent(100.0),
            height: Val::Percent(100.0),
            padding: UiRect {
                top: Val::Px(40.0 + layout.gap()),
                bottom: Val::Px(32.0 + layout.gap()),
                ..UiRect::horizontal(Val::Px(layout.gap()))
            },
            row_gap: Val::Px(layout.gap()),
            overflow: Overflow::clip(),
            ..column()
        },
        BackgroundGradient::from(LinearGradient::to_bottom_right(vec![
            ColorStop::percent(colors.bg.mix(&colors.cyan, 0.06), 0),
            ColorStop::percent(colors.bg, 44),
            ColorStop::percent(colors.bg.mix(&Color::srgb(0.14, 0.09, 0.30), 0.29), 100),
        ])),
    )).with_children(|root| {
        header(root, &font, &colors, layout, &nav, &feed);
        root.spawn(Node {
            flex_grow: 1.0,
            flex_basis: Val::Px(0.0),
            flex_direction: if layout.stacked { FlexDirection::Column } else { FlexDirection::Row },
            min_height: Val::Px(0.0),
            row_gap: Val::Px(layout.gap()),
            column_gap: Val::Px(layout.gap()),
            ..default()
        }).with_children(|room| {
            let reading = nav.layer() == BoardLayer::Posts;
            if !layout.compact || !reading || topic.is_none() {
                index(room, &nav, &feed, &font, &colors, layout, now_ms);
            }
            if topic.is_some() && (!layout.compact || reading) {
                reader(room, &nav, &feed, &font, &colors, layout, scroll_y, now_ms);
            }
        });
        controls(root, nav.layer(), nav.guide(&feed).is_some(), &font, &colors);
    });
}

fn index(
    parent: &mut ChildSpawnerCommands, nav: &BoardNav, feed: &BoardFeed,
    font: &Lettering, colors: &Palette, layout: Layout, now_ms: Option<i64>,
) {
    let roomy = !layout.short && !layout.stacked && !layout.compact;
    let (heading, rows): (_, Vec<IndexRow>) = match nav.layer() {
        BoardLayer::Boards => ("Boards", nav.boards(feed).into_iter().map(|board| {
            let mut topics = feed.topics().iter().filter(|topic| topic.board() == board);
            let latest = topics.next();
            let count = usize::from(latest.is_some()) + topics.count();
            let mut meta = format!("{count} {}", if count == 1 { "topic" } else { "topics" });
            if let Some(latest) = latest.filter(|topic| !topic.age.is_empty()) {
                meta.push_str(&format!("  ·  updated {}", latest.age));
            }
            IndexRow {
                title: board.into(), meta, selected: nav.board() == Some(board),
                subject: latest.filter(|_| roomy).map(|topic| index_title(&topic.title)),
            }
        }).collect()),
        BoardLayer::Topics => ("Topics", nav.topics(feed).into_iter().map(|topic| {
            let count = post_count(topic.post_count);
            let meta = if topic.age.is_empty() { count } else { format!("{count}  ·  {}", topic.age) };
            IndexRow {
                title: index_title(&topic.title), meta, subject: None,
                selected: nav.topic(feed).is_some_and(|selected| selected.id == topic.id),
            }
        }).collect()),
        BoardLayer::Posts => ("Posts", feed.posts().iter().map(|post| {
            let title = if post.author.is_empty() { format!("#{}", post.number) } else {
                format!("#{}  {}", post.number, post.author)
            };
            IndexRow {
                title, meta: post_age(post.created_at, now_ms).unwrap_or_default(), subject: None,
                selected: nav.post(feed).is_some_and(|selected| selected.number == post.number),
            }
        }).collect()),
    };
    parent.spawn((panel(true, 1.0, layout, colors), Name::new("Board index"))).with_children(|index| {
        index.spawn(font.signal(heading, if layout.short { 21.0 } else { 25.0 }, colors.cyan));
        if rows.is_empty() {
            if nav.layer() != BoardLayer::Posts || nav.topic(feed).is_none() {
                index.spawn(font.text(unavailable(feed, heading), 22.0, colors.muted));
            }
            return;
        }
        let selected_index = rows.iter().position(|row| row.selected).unwrap_or(0);
        let before = usize::from(roomy);
        let first = selected_index.saturating_sub(before);
        index.spawn(Node {
            row_gap: Val::Px(3.0),
            min_height: Val::Px(0.0),
            overflow: Overflow::clip(),
            ..column()
        }).with_children(|list| {
            for row in rows.into_iter().skip(first) {
                index_row(list, row, font, colors, layout);
            }
        });
    });
}

struct IndexRow {
    title: String,
    meta: String,
    subject: Option<String>,
    selected: bool,
}

fn index_title(title: &str) -> String {
    let line = title.split_whitespace().collect::<Vec<_>>().join(" ");
    line.char_indices().nth(86).map_or_else(|| line.clone(), |(end, _)| format!("{}…", &line[..end]))
}

fn index_row(
    parent: &mut ChildSpawnerCommands, item: IndexRow, font: &Lettering, colors: &Palette, layout: Layout,
) {
    let IndexRow { title, meta, subject, selected } = item;
    let highlight = if selected { colors.amber } else { colors.line };
    parent.spawn((
        Node {
            flex_shrink: 0.0,
            border: UiRect::left(Val::Px(if selected { 4.0 } else { 1.0 })),
            padding: UiRect::axes(Val::Px(14.0), Val::Px(if layout.short { 5.0 } else { 9.0 })),
            ..column()
        },
        BorderColor::all(highlight),
        fade(colors.panel.mix(&highlight, if selected { 0.16 } else { 0.09 }), colors.panel),
    )).with_children(|row| {
        row.spawn(font.text(title, if layout.short { 23.0 } else { 27.0 }, if selected { colors.amber } else { colors.fg }));
        if let Some(subject) = subject {
            row.spawn(font.text(subject, 18.0, colors.fg));
        }
        if !meta.is_empty() {
            row.spawn(font.text(meta, 17.0, colors.muted));
        }
    });
}

fn unavailable(feed: &BoardFeed, heading: &str) -> String {
    if let Some(error) = feed.error() { return error.to_string(); }
    if feed.loading() { return format!("Loading {}…", heading.to_ascii_lowercase()); }
    format!("No {} to show", heading.to_ascii_lowercase())
}

fn reader(
    parent: &mut ChildSpawnerCommands, nav: &BoardNav, feed: &BoardFeed,
    font: &Lettering, colors: &Palette, layout: Layout, scroll_y: f32, now_ms: Option<i64>,
) {
    let topic = nav.topic(feed);
    let post = nav.post(feed);
    let layer = nav.layer();
    let grow = if layer == BoardLayer::Posts { 2.6 } else { 1.7 };
    parent.spawn((panel(false, grow, layout, colors), Name::new("Board reader"))).with_children(|reader| {
        let Some(topic) = topic else {
            reader.spawn(font.text(unavailable(feed, "topics"), 30.0, colors.muted));
            return;
        };
        reader.spawn(font.text(topic.board(), 18.0, colors.cyan));
        reader.spawn(font.text(topic.title.as_str(), if layout.short || layout.stacked { 30.0 } else { 40.0 }, colors.fg));
        if !layout.short {
            let count = post_count(topic.post_count);
            let details = if topic.status.is_empty() || topic.status == "active" { count } else {
                format!("{}  ·  {count}", topic.status)
            };
            reader.spawn(font.text(details, 17.0, colors.muted));
        }
        rule(reader, colors);
        let Some(post) = post else {
            reader.spawn(font.text(unavailable(feed, "posts"), 22.0, colors.muted));
            return;
        };
        reader.spawn((
            PostScroll(topic.id.clone(), post.number),
            ScrollPosition(Vec2::new(0.0, scroll_y)),
            Node {
                flex_grow: 1.0,
                flex_basis: Val::Px(0.0),
                min_height: Val::Px(0.0),
                overflow: Overflow::scroll_y(),
                ..column()
            },
        )).with_children(|body| post_card(body, post, font, colors, now_ms));
    });
}

fn controls(parent: &mut ChildSpawnerCommands, layer: BoardLayer, guide: bool, font: &Lettering, colors: &Palette) {
    parent.spawn(Node {
        flex_wrap: FlexWrap::Wrap,
        align_items: AlignItems::Center,
        justify_content: JustifyContent::SpaceBetween,
        column_gap: Val::Px(16.0),
        row_gap: Val::Px(4.0),
        flex_shrink: 0.0,
        ..default()
    }).with_children(|controls| {
        let (browse, act) = match layer {
            BoardLayer::Boards | BoardLayer::Topics => ("↑ ↓  Browse", "Enter / A  Open"),
            BoardLayer::Posts => ("↑ ↓  Posts", "PgUp/PgDn / LB/RB  Read"),
        };
        for hint in [browse, act, "Esc / B  Back"] {
            controls.spawn(font.signal(hint, 17.0, colors.muted));
        }
        if guide {
            controls.spawn(font.signal("G / X  Chomosuke", 18.0, colors.amber));
        }
    });
}

fn scroll_posts(
    mut actions: MessageReader<ActionFired>,
    contexts: Res<ActiveInputContexts>,
    mut scroll: Query<(&mut ScrollPosition, &ComputedNode), With<PostScroll>>,
) {
    let Ok((mut position, node)) = scroll.single_mut() else {
        actions.clear();
        return;
    };
    let page = logical_size(node).y * 0.85;
    let max_offset = ((node.content_size().y - node.size().y) * node.inverse_scale_factor()).max(0.0);
    for fired in actions.read() {
        let delta = match fired.action {
            Action::ScrollDelta(delta) if fired.context == InputContext::Global && contexts.contains(InputContext::Boards) => delta,
            Action::BoardScrollUp if fired.context == InputContext::Boards => -page,
            Action::BoardScrollDown if fired.context == InputContext::Boards => page,
            _ => continue,
        };
        position.y = (position.y.min(max_offset) + delta).clamp(0.0, max_offset);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_cannot_accumulate_scroll_outside_the_post() {
        let mut app = App::new();
        app.add_message::<ActionFired>()
            .insert_resource(ActiveInputContexts(vec![InputContext::Boards]))
            .add_systems(Update, scroll_posts);
        let reader = app.world_mut().spawn((
            PostScroll("topic".into(), 1),
            ScrollPosition::default(),
            ComputedNode::default(),
        )).id();
        app.world_mut().write_message(ActionFired::new(Action::ScrollDelta(900.0), InputContext::Global));
        app.update();
        assert_eq!(app.world().get::<ScrollPosition>(reader).unwrap().y, 0.0);
    }
}
