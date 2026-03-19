//! Constellation container + legend panel.
//!
//! Extracted from the deleted 2D `render.rs`. Contains:
//! - `spawn_constellation_container` — the flex child that holds all constellation content
//! - `spawn_legend_panel` + `update_legend_content` — info panel (top-left overlay)

use bevy::prelude::*;
use bevy_vello::prelude::UiVelloScene;
use bevy_vello::vello::kurbo::RoundedRect;
use bevy_vello::vello::peniko::{Color as VelloColor, Fill};

use super::{Constellation, ConstellationContainer};
use crate::text::truncate_chars;
use crate::ui::drift::DriftState;
use crate::ui::screen::Screen;
use crate::ui::theme::Theme;

/// Marker for the legend panel container in the constellation view.
#[derive(Component)]
struct ConstellationLegend;

/// Marker for legend content entities (rebuilt on data change).
#[derive(Component)]
struct LegendContent;

/// Marker for the legend background scene.
#[derive(Component)]
struct LegendBg;

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
/// Visibility matches the current screen state at spawn time so we don't
/// depend on `OnEnter` having already fired.
fn spawn_constellation_container(
    mut commands: Commands,
    existing: Query<Entity, With<ConstellationContainer>>,
    content_area: Query<Entity, With<crate::ui::state::ContentArea>>,
    screen: Res<State<Screen>>,
) {
    if !existing.is_empty() {
        return;
    }

    let Ok(content_entity) = content_area.single() else {
        return;
    };

    let vis = if *screen.get() == Screen::Constellation {
        Visibility::Inherited
    } else {
        Visibility::Hidden
    };

    let constellation_entity = commands
        .spawn((
            ConstellationContainer,
            Node {
                position_type: PositionType::Absolute,
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                overflow: Overflow::clip(),
                ..default()
            },
            vis,
        ))
        .id();

    commands
        .entity(content_entity)
        .add_child(constellation_entity);
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

    // Build background scene (renders in Vello pass, not UI pass — won't cover Vello text)
    let bg_color = theme.panel_bg.with_alpha(0.85).to_srgba();
    let vello_bg = VelloColor::new([bg_color.red, bg_color.green, bg_color.blue, bg_color.alpha]);
    let mut scene = bevy_vello::vello::Scene::new();
    let rect = RoundedRect::new(0.0, 0.0, 160.0, 120.0, 4.0);
    scene.fill(
        Fill::NonZero,
        bevy_vello::vello::kurbo::Affine::IDENTITY,
        vello_bg,
        None,
        &rect,
    );

    let legend_entity = commands
        .spawn((
            ConstellationLegend,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(16.0),
                top: Val::Px(16.0),
                width: Val::Px(160.0),
                min_height: Val::Px(40.0),
                flex_direction: FlexDirection::Column,
                padding: UiRect::all(Val::Px(10.0)),
                row_gap: Val::Px(4.0),
                ..default()
            },
            ZIndex(1),
        ))
        .with_children(|parent| {
            // Background scene — absolute, covers full panel
            parent.spawn((
                LegendBg,
                UiVelloScene::from(scene),
                Node {
                    position_type: PositionType::Absolute,
                    width: Val::Percent(100.0),
                    height: Val::Percent(100.0),
                    ..default()
                },
            ));
        })
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
    font_handles: Res<crate::text::FontHandles>,
    legend_q: Query<Entity, With<ConstellationLegend>>,
    content_q: Query<Entity, With<LegendContent>>,
    mut last_fingerprint: Local<u64>,
) {
    if !matches!(screen.get(), Screen::Constellation) {
        return;
    }

    let fingerprint = {
        let mut h: u64 = constellation.nodes.len() as u64;
        h = h
            .wrapping_mul(31)
            .wrapping_add(drift_state.staged.len() as u64);
        h = h
            .wrapping_mul(31)
            .wrapping_add(drift_state.contexts.len() as u64);
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

    let font = &font_handles.mono;

    // Header: kernel name
    let header = spawn_legend_text(
        &mut commands,
        font,
        &truncate_chars(kernel_name, 18),
        theme.fg,
        11.0,
    );
    commands.entity(legend_entity).add_child(header);

    // Summary line
    let summary = format!("{} contexts", total_contexts);
    let summary_entity = spawn_legend_text(&mut commands, font, &summary, theme.fg_dim, 9.0);
    commands.entity(legend_entity).add_child(summary_entity);

    // Staged drift count
    if staged_count > 0 {
        let drift_text = format!("{} staged drifts", staged_count);
        let drift_entity =
            spawn_legend_text(&mut commands, font, &drift_text, theme.ansi.cyan, 9.0);
        commands.entity(legend_entity).add_child(drift_entity);
    }

    *last_fingerprint = fingerprint;
}

fn spawn_legend_text(
    commands: &mut Commands,
    font: &Handle<bevy_vello::prelude::VelloFont>,
    text: &str,
    color: Color,
    font_size: f32,
) -> Entity {
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
                bevy_vello::prelude::UiVelloText {
                    value: text.into(),
                    style: crate::text::vello_style(font, color, font_size),
                    ..default()
                },
                Node {
                    width: Val::Percent(100.0),
                    ..default()
                },
            ));
        })
        .id()
}
