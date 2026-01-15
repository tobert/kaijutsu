use bevy::{ecs::hierarchy::ChildSpawnerCommands, prelude::*, ui::widget::NodeImageMode};

use super::{context::ContextArea, input::InputDisplay, theme::Theme};
use crate::state::mode::Mode;

#[derive(Component)]
pub struct ModeIndicator;

/// Resource holding our loaded fonts
#[derive(Resource)]
pub struct UiFonts {
    pub jp: Handle<Font>,
}

pub fn setup(mut commands: Commands, theme: Res<Theme>, asset_server: Res<AssetServer>) {
    commands.spawn(Camera2d);

    // Load fonts
    let jp_font: Handle<Font> = asset_server.load("fonts/DroidSansJapanese.ttf");
    commands.insert_resource(UiFonts { jp: jp_font.clone() });

    // Load frame assets
    let sidebar_frame: Handle<Image> = asset_server.load("ui/panel-frame.png");
    let context_frame: Handle<Image> = asset_server.load("ui/context-frame-thin.png");

    // Root container
    commands
        .spawn((
            Node {
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                flex_direction: FlexDirection::Column,
                ..default()
            },
            BackgroundColor(theme.bg),
        ))
        .with_children(|root| {
            title_bar(root, &theme, jp_font.clone());

            // Middle: sidebar + context
            root.spawn(Node {
                width: Val::Percent(100.0),
                flex_grow: 1.0,
                flex_direction: FlexDirection::Row,
                ..default()
            })
            .with_children(|middle| {
                sidebar(middle, &theme, sidebar_frame);
                context_area(middle, &theme, context_frame);
            });

            // Input bar (SACRED)
            input_bar(root, &theme);
        });
}

fn title_bar(parent: &mut ChildSpawnerCommands, theme: &Theme, font: Handle<Font>) {
    parent
        .spawn((
            Node {
                width: Val::Percent(100.0),
                height: Val::Px(40.0),
                padding: UiRect::horizontal(Val::Px(16.0)),
                align_items: AlignItems::Center,
                justify_content: JustifyContent::SpaceBetween,
                border: UiRect::bottom(Val::Px(2.0)),
                ..default()
            },
            BorderColor::all(theme.accent),
            BackgroundColor(theme.panel_bg),
        ))
        .with_children(|bar| {
            bar.spawn((
                Text::new("【会術】 Kaijutsu"),
                TextFont {
                    font: font.clone(),
                    font_size: 20.0,
                    ..default()
                },
                TextColor(theme.accent),
            ));
            bar.spawn((
                Text::new("▣ room: lobby"),
                TextFont {
                    font: font.clone(),
                    font_size: 14.0,
                    ..default()
                },
                TextColor(theme.fg),
            ));
        });
}

fn sidebar(parent: &mut ChildSpawnerCommands, theme: &Theme, frame: Handle<Image>) {
    parent
        .spawn((
            Node {
                width: Val::Px(180.0),
                height: Val::Percent(100.0),
                flex_direction: FlexDirection::Column,
                padding: UiRect::all(Val::Px(20.0)), // Extra padding for frame border
                row_gap: Val::Px(16.0),
                ..default()
            },
            BackgroundColor(theme.panel_bg),
        ))
        .with_children(|side| {
            // Content first
            sidebar_section(side, theme, "ROOMS", &["> lobby", "  dev", "  ops"]);
            sidebar_section(side, theme, "AGENTS", &["◉ opus", "◉ haiku", "○ local"]);
            sidebar_section(side, theme, "EQUIP", &["filesystem", "web_search"]);

            // Frame overlay with 9-slice (spawned last = renders on top)
            side.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    top: Val::Px(0.0),
                    left: Val::Px(0.0),
                    right: Val::Px(0.0),
                    bottom: Val::Px(0.0),
                    ..default()
                },
                ImageNode {
                    image: frame,
                    image_mode: NodeImageMode::Sliced(TextureSlicer {
                        // panel-frame.png: 2438x2574, border ~100px
                        border: BorderRect::all(100.0),
                        center_scale_mode: SliceScaleMode::Stretch,
                        sides_scale_mode: SliceScaleMode::Stretch,
                        max_corner_scale: 1.0,
                    }),
                    ..default()
                },
            ));
        });
}

fn sidebar_section(parent: &mut ChildSpawnerCommands, theme: &Theme, title: &str, items: &[&str]) {
    parent
        .spawn(Node {
            flex_direction: FlexDirection::Column,
            row_gap: Val::Px(4.0),
            ..default()
        })
        .with_children(|section| {
            section.spawn((
                Text::new(title),
                TextFont {
                    font_size: 12.0,
                    ..default()
                },
                TextColor(theme.accent2),
            ));
            for item in items {
                section.spawn((
                    Text::new(*item),
                    TextFont {
                        font_size: 14.0,
                        ..default()
                    },
                    TextColor(theme.fg),
                ));
            }
        });
}

fn context_area(parent: &mut ChildSpawnerCommands, theme: &Theme, frame: Handle<Image>) {
    parent
        .spawn((
            Node {
                flex_grow: 1.0,
                height: Val::Percent(100.0),
                flex_direction: FlexDirection::Column,
                padding: UiRect::all(Val::Px(24.0)), // Extra padding for frame border
                row_gap: Val::Px(8.0),
                ..default()
            },
            BackgroundColor(theme.bg),
            ContextArea,
        ))
        .with_children(|ctx| {
            // Frame overlay with 9-slice (spawned = renders on top of messages)
            ctx.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    top: Val::Px(0.0),
                    left: Val::Px(0.0),
                    right: Val::Px(0.0),
                    bottom: Val::Px(0.0),
                    ..default()
                },
                ImageNode {
                    image: frame,
                    image_mode: NodeImageMode::Sliced(TextureSlicer {
                        // context-frame-thin.png: 5695x1623, border ~150px
                        border: BorderRect::all(150.0),
                        center_scale_mode: SliceScaleMode::Stretch,
                        sides_scale_mode: SliceScaleMode::Stretch,
                        max_corner_scale: 1.0,
                    }),
                    ..default()
                },
                // Don't block clicks on content below
                Pickable::IGNORE,
            ));
        });
}

fn input_bar(parent: &mut ChildSpawnerCommands, theme: &Theme) {
    parent
        .spawn((
            Node {
                width: Val::Percent(100.0),
                height: Val::Px(50.0),
                padding: UiRect::horizontal(Val::Px(16.0)),
                align_items: AlignItems::Center,
                justify_content: JustifyContent::SpaceBetween,
                border: UiRect::top(Val::Px(2.0)),
                ..default()
            },
            BorderColor::all(theme.accent),
            BackgroundColor(theme.panel_bg),
        ))
        .with_children(|bar| {
            bar.spawn((
                Text::new("> _"),
                TextFont {
                    font_size: 14.0,
                    ..default()
                },
                TextColor(theme.fg),
                InputDisplay,
            ));
            bar.spawn((
                Text::new("[N]"),
                TextFont {
                    font_size: 14.0,
                    ..default()
                },
                TextColor(theme.accent),
                ModeIndicator,
            ));
        });
}

pub fn update_mode_indicator(mode: Res<State<Mode>>, mut query: Query<&mut Text, With<ModeIndicator>>) {
    if mode.is_changed() {
        for mut text in &mut query {
            **text = mode.get().indicator().to_string();
        }
    }
}
