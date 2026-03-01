//! Constellation container + legend panel.
//!
//! Extracted from the deleted 2D `render.rs`. Contains:
//! - `spawn_constellation_container` — the flex child that holds all constellation content
//! - `spawn_legend_panel` + `update_legend_content` — info panel (top-left overlay)

use bevy::prelude::*;

use super::{Constellation, ConstellationContainer};
use crate::ui::drift::DriftState;
use crate::ui::screen::Screen;
use crate::ui::theme::Theme;

/// Marker for the legend panel container in the constellation view.
#[derive(Component)]
struct ConstellationLegend;

/// Marker for legend content entities (rebuilt on data change).
#[derive(Component)]
struct LegendContent;

pub fn setup_legend_systems(app: &mut App) {
    app.add_systems(
        Update,
        (
            spawn_constellation_container,
            spawn_legend_panel,
            update_legend_content,
        )
            .chain(),
    );
}

/// Spawn the constellation container as a full-size flex child of ContentArea.
///
/// Starts with `Display::None` — toggled by `sync_constellation_visibility`.
fn spawn_constellation_container(
    mut commands: Commands,
    existing: Query<Entity, With<ConstellationContainer>>,
    content_area: Query<Entity, With<crate::ui::state::ContentArea>>,
) {
    if !existing.is_empty() {
        return;
    }

    let Ok(content_entity) = content_area.single() else {
        return;
    };

    let constellation_entity = commands
        .spawn((
            ConstellationContainer,
            Node {
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                flex_grow: 1.0,
                overflow: Overflow::clip(),
                display: Display::None,
                ..default()
            },
            Visibility::Hidden,
            BackgroundColor(Color::NONE),
        ))
        .id();

    commands.entity(content_entity).add_child(constellation_entity);
    info!("Spawned constellation container");
}

/// Spawn the legend panel container as a child of ConstellationContainer.
fn spawn_legend_panel(
    mut commands: Commands,
    theme: Res<Theme>,
    container: Query<Entity, With<ConstellationContainer>>,
    existing: Query<Entity, With<ConstellationLegend>>,
) {
    if !existing.is_empty() {
        return;
    }

    let Ok(container_entity) = container.single() else {
        return;
    };

    let legend_entity = commands
        .spawn((
            ConstellationLegend,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(16.0),
                top: Val::Px(16.0),
                width: Val::Px(220.0),
                min_height: Val::Px(80.0),
                flex_direction: FlexDirection::Column,
                padding: UiRect::all(Val::Px(12.0)),
                row_gap: Val::Px(4.0),
                ..default()
            },
            BackgroundColor(theme.panel_bg.with_alpha(0.85)),
            ZIndex(1),
        ))
        .id();

    commands.entity(container_entity).add_child(legend_entity);
    info!("Spawned constellation legend panel");
}

/// Rebuild legend content when DriftState or Constellation changes.
fn update_legend_content(
    mut commands: Commands,
    screen: Res<State<Screen>>,
    drift_state: Res<DriftState>,
    constellation: Res<Constellation>,
    theme: Res<Theme>,
    legend_q: Query<Entity, With<ConstellationLegend>>,
    content_q: Query<Entity, With<LegendContent>>,
    mut last_fingerprint: Local<u64>,
) {
    if !matches!(screen.get(), Screen::Constellation) {
        return;
    }

    let fingerprint = {
        let mut h: u64 = constellation.nodes.len() as u64;
        h = h.wrapping_mul(31).wrapping_add(drift_state.staged.len() as u64);
        h = h.wrapping_mul(31).wrapping_add(drift_state.contexts.len() as u64);
        for ctx in &drift_state.contexts {
            h = h.wrapping_mul(31).wrapping_add(ctx.provider.len() as u64);
        }
        h
    };

    if fingerprint == *last_fingerprint && !content_q.is_empty() {
        return;
    }

    let Ok(legend_entity) = legend_q.single() else {
        return;
    };

    for entity in content_q.iter() {
        commands.entity(entity).despawn();
    }

    let total_contexts = constellation.nodes.len();
    let staged_count = drift_state.staged_count();
    let kernel_name = "(kernel)";

    let mut provider_counts: Vec<(&str, Color, usize)> = Vec::new();
    let provider_groups = [
        ("human", theme.agent_color_human),
        ("anthropic", theme.agent_color_claude),
        ("google", theme.agent_color_gemini),
        ("deepseek", theme.agent_color_deepseek),
        ("local", theme.agent_color_local),
    ];

    for (provider_key, color) in &provider_groups {
        let count = drift_state.contexts
            .iter()
            .filter(|c| {
                let p = c.provider.to_ascii_lowercase();
                match *provider_key {
                    "anthropic" => p.contains("anthropic") || p.contains("claude"),
                    "google" => p.contains("google") || p.contains("gemini"),
                    "deepseek" => p.contains("deepseek"),
                    "local" => p.contains("ollama") || p.contains("local") || p.contains("llama"),
                    "human" => p.is_empty(),
                    _ => false,
                }
            })
            .count();
        if count > 0 {
            provider_counts.push((provider_key, *color, count));
        }
    }

    let unique_providers = provider_counts.len();

    // Header: kernel name
    let header = spawn_legend_text(&mut commands, &truncate_name(kernel_name, 22), theme.fg, 11.0);
    commands.entity(legend_entity).add_child(header);

    // Summary line
    let summary = format!("{} contexts \u{00b7} {} agents", total_contexts, unique_providers);
    let summary_entity = spawn_legend_text(&mut commands, &summary, theme.fg_dim, 9.0);
    commands.entity(legend_entity).add_child(summary_entity);

    // Separator
    let sep = commands
        .spawn((
            LegendContent,
            Node {
                width: Val::Percent(100.0),
                height: Val::Px(1.0),
                margin: UiRect::vertical(Val::Px(3.0)),
                ..default()
            },
            BackgroundColor(theme.border.with_alpha(0.4)),
        ))
        .id();
    commands.entity(legend_entity).add_child(sep);

    // Per-provider rows
    for (label, color, count) in &provider_counts {
        let display_name = match *label {
            "anthropic" => "claude",
            "google" => "gemini",
            "human" => "amy",
            l => l,
        };
        let row = spawn_legend_agent_row(&mut commands, display_name, *color, *count, &theme);
        commands.entity(legend_entity).add_child(row);
    }

    // Staged drift count
    if staged_count > 0 {
        let sep2 = commands
            .spawn((
                LegendContent,
                Node {
                    width: Val::Percent(100.0),
                    height: Val::Px(1.0),
                    margin: UiRect::vertical(Val::Px(3.0)),
                    ..default()
                },
                BackgroundColor(theme.border.with_alpha(0.4)),
            ))
            .id();
        commands.entity(legend_entity).add_child(sep2);

        let drift_text = format!("{} staged drifts", staged_count);
        let drift_entity = spawn_legend_text(&mut commands, &drift_text, theme.ansi.cyan, 9.0);
        commands.entity(legend_entity).add_child(drift_entity);
    }

    *last_fingerprint = fingerprint;
}

fn truncate_name(name: &str, max_len: usize) -> String {
    if name.len() <= max_len {
        name.to_string()
    } else {
        format!("{}...", &name[..max_len - 3])
    }
}

fn spawn_legend_text(commands: &mut Commands, text: &str, color: Color, font_size: f32) -> Entity {
    commands
        .spawn((
            LegendContent,
            Node {
                min_height: Val::Px(font_size + 4.0),
                ..default()
            },
        ))
        .with_children(|parent| {
            parent.spawn((
                crate::text::KjUiText::new(text)
                    .with_font_size(font_size)
                    .with_color(color),
                bevy_vello::prelude::UiVelloText::default(),
                Node {
                    width: Val::Percent(100.0),
                    height: Val::Px(font_size + 2.0),
                    ..default()
                },
            ));
        })
        .id()
}

fn spawn_legend_agent_row(
    commands: &mut Commands,
    name: &str,
    color: Color,
    count: usize,
    theme: &Theme,
) -> Entity {
    commands
        .spawn((
            LegendContent,
            Node {
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::Center,
                column_gap: Val::Px(6.0),
                min_height: Val::Px(16.0),
                ..default()
            },
        ))
        .with_children(|parent| {
            // Colored dot
            parent.spawn((
                Node {
                    width: Val::Px(8.0),
                    height: Val::Px(8.0),
                    border_radius: BorderRadius::all(Val::Px(4.0)),
                    ..default()
                },
                BackgroundColor(color),
            ));
            // Agent name
            parent.spawn((
                crate::text::KjUiText::new(name)
                    .with_font_size(10.0)
                    .with_color(color),
                bevy_vello::prelude::UiVelloText::default(),
                Node {
                    width: Val::Px(70.0),
                    height: Val::Px(12.0),
                    ..default()
                },
            ));
            // Count
            let count_text = format!("{} ctx", count);
            parent.spawn((
                crate::text::KjUiText::new(&count_text)
                    .with_font_size(9.0)
                    .with_color(theme.fg_dim),
                bevy_vello::prelude::UiVelloText::default(),
                Node {
                    width: Val::Px(50.0),
                    height: Val::Px(11.0),
                    margin: UiRect::left(Val::Auto),
                    ..default()
                },
            ));
        })
        .id()
}
