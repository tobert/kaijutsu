use bevy::{ecs::hierarchy::ChildSpawnerCommands, prelude::*};

use super::{context::ContextArea, input::InputDisplay, theme::Theme};
use crate::state::mode::Mode;

#[derive(Component)]
pub struct ModeIndicator;

pub fn setup(mut commands: Commands, theme: Res<Theme>) {
    commands.spawn(Camera2d);

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
            title_bar(root, &theme);

            // Middle: sidebar + context
            root.spawn(Node {
                width: Val::Percent(100.0),
                flex_grow: 1.0,
                flex_direction: FlexDirection::Row,
                ..default()
            })
            .with_children(|middle| {
                sidebar(middle, &theme);
                context_area(middle, &theme);
            });

            // Input bar (SACRED)
            input_bar(root, &theme);
        });
}

fn title_bar(parent: &mut ChildSpawnerCommands, theme: &Theme) {
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
            BorderColor::all(theme.border),
            BackgroundColor(theme.panel_bg),
        ))
        .with_children(|bar| {
            bar.spawn((
                Text::new("【会術】 Kaijutsu"),
                TextFont {
                    font_size: 20.0,
                    ..default()
                },
                TextColor(theme.accent),
            ));
            bar.spawn((
                Text::new("▣ room: lobby"),
                TextFont {
                    font_size: 14.0,
                    ..default()
                },
                TextColor(theme.fg),
            ));
        });
}

fn sidebar(parent: &mut ChildSpawnerCommands, theme: &Theme) {
    parent
        .spawn((
            Node {
                width: Val::Px(180.0),
                height: Val::Percent(100.0),
                flex_direction: FlexDirection::Column,
                padding: UiRect::all(Val::Px(12.0)),
                border: UiRect::right(Val::Px(2.0)),
                row_gap: Val::Px(16.0),
                ..default()
            },
            BorderColor::all(theme.border),
            BackgroundColor(theme.panel_bg),
        ))
        .with_children(|side| {
            sidebar_section(side, theme, "ROOMS", &["> lobby", "  dev", "  ops"]);
            sidebar_section(side, theme, "AGENTS", &["◉ opus", "◉ haiku", "○ local"]);
            sidebar_section(side, theme, "EQUIP", &["filesystem", "web_search"]);
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

fn context_area(parent: &mut ChildSpawnerCommands, theme: &Theme) {
    parent.spawn((
        Node {
            flex_grow: 1.0,
            height: Val::Percent(100.0),
            flex_direction: FlexDirection::Column,
            padding: UiRect::all(Val::Px(16.0)),
            row_gap: Val::Px(8.0),
            ..default()
        },
        BackgroundColor(theme.bg),
        ContextArea,
    ));
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
            BorderColor::all(theme.border),
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
