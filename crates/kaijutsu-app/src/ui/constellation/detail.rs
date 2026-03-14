//! Detail sidebar for the focused constellation node.
//!
//! Positioned on the right side of the constellation view, shows info about
//! the currently focused context: label, model, provider, fork lineage,
//! activity state, and recency. Left accent stripe uses agent color (CP2077 feel).

use bevy::prelude::*;
use bevy_vello::prelude::{UiVelloScene, UiVelloText};
use bevy_vello::vello::kurbo::{Affine, RoundedRect};
use bevy_vello::vello::peniko::{Color as VelloColor, Fill};

use super::{Constellation, ConstellationContainer};
use crate::ui::screen::Screen;
use crate::ui::theme::{agent_color_for_provider, Theme};

/// Marker for the detail sidebar container.
#[derive(Component)]
struct DetailSidebar;

/// Marker for detail content entities (rebuilt on focus change).
#[derive(Component)]
struct DetailContent;

/// Marker for the detail background scene.
#[derive(Component)]
struct DetailBg;

pub fn setup_detail_systems(app: &mut App) {
    app.add_systems(
        Update,
        (
            spawn_detail_sidebar,
            update_detail_content,
        )
            .chain(),
    );
}

/// Spawn the detail sidebar as a child of ConstellationContainer.
fn spawn_detail_sidebar(
    mut commands: Commands,
    theme: Res<Theme>,
    container: Query<Entity, With<ConstellationContainer>>,
    existing: Query<Entity, With<DetailSidebar>>,
) {
    if !existing.is_empty() {
        return;
    }

    let Ok(container_entity) = container.single() else {
        return;
    };

    // Background scene (Vello — won't occlude Vello text)
    let bg_color = theme.panel_bg.with_alpha(0.90).to_srgba();
    let vello_bg = VelloColor::new([bg_color.red, bg_color.green, bg_color.blue, bg_color.alpha]);
    let mut scene = bevy_vello::vello::Scene::new();
    let rect = RoundedRect::new(0.0, 0.0, 280.0, 300.0, 6.0);
    scene.fill(Fill::NonZero, Affine::IDENTITY, vello_bg, None, &rect);

    let sidebar_entity = commands
        .spawn((
            DetailSidebar,
            Node {
                position_type: PositionType::Absolute,
                right: Val::Px(16.0),
                top: Val::Px(16.0),
                width: Val::Px(280.0),
                min_height: Val::Px(60.0),
                flex_direction: FlexDirection::Column,
                padding: UiRect::all(Val::Px(12.0)),
                row_gap: Val::Px(4.0),
                ..default()
            },
            ZIndex(2),
        ))
        .with_children(|parent| {
            // Background scene — absolute, covers full panel
            parent.spawn((
                DetailBg,
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

    commands.entity(container_entity).add_child(sidebar_entity);
    info!("Spawned constellation detail sidebar");
}

/// Rebuild detail sidebar content when focus or node data changes.
fn update_detail_content(
    mut commands: Commands,
    screen: Res<State<Screen>>,
    constellation: Res<Constellation>,
    theme: Res<Theme>,
    font_handles: Res<crate::text::FontHandles>,
    time: Res<Time>,
    sidebar_q: Query<Entity, With<DetailSidebar>>,
    content_q: Query<Entity, With<DetailContent>>,
    mut bg_q: Query<&mut UiVelloScene, With<DetailBg>>,
    mut last_fingerprint: Local<u64>,
) {
    if !matches!(screen.get(), Screen::Constellation) {
        return;
    }

    let focus_id = match constellation.focus_id {
        Some(id) => id,
        None => return,
    };

    let node = match constellation.node_by_id(focus_id) {
        Some(n) => n,
        None => return,
    };

    // Fingerprint to avoid unnecessary rebuilds
    let fingerprint = {
        let mut h: u64 = 0x517cc1b727220a95;
        for byte in focus_id.to_string().bytes() {
            h = h.wrapping_mul(31).wrapping_add(byte as u64);
        }
        h = h.wrapping_mul(31).wrapping_add(node.label.as_ref().map(|l| l.len()).unwrap_or(0) as u64);
        h = h.wrapping_mul(31).wrapping_add(node.model.as_ref().map(|m| m.len()).unwrap_or(0) as u64);
        h = h.wrapping_mul(31).wrapping_add(node.provider.as_ref().map(|p| p.len()).unwrap_or(0) as u64);
        h = h.wrapping_mul(31).wrapping_add(node.activity as u64);
        h = h.wrapping_mul(31).wrapping_add(if node.joined { 1 } else { 0 });
        h = h.wrapping_mul(31).wrapping_add(node.graph_distance as u64);
        h = h.wrapping_mul(31).wrapping_add(node.keywords.len() as u64);
        h = h.wrapping_mul(31).wrapping_add(node.top_block_preview.as_ref().map(|p| p.len()).unwrap_or(0) as u64);
        h
    };

    if fingerprint == *last_fingerprint && !content_q.is_empty() {
        return;
    }

    let Ok(sidebar_entity) = sidebar_q.single() else {
        return;
    };

    // Despawn old content
    for entity in content_q.iter() {
        commands.entity(entity).despawn();
    }

    let font = &font_handles.mono;
    let agent_color = agent_color_for_provider(&theme, node.provider.as_deref());

    // Rebuild background with accent stripe
    for mut bg in bg_q.iter_mut() {
        let bg_srgba = theme.panel_bg.with_alpha(0.90).to_srgba();
        let vello_bg = VelloColor::new([bg_srgba.red, bg_srgba.green, bg_srgba.blue, bg_srgba.alpha]);
        let accent_srgba = agent_color.to_srgba();
        let vello_accent = VelloColor::new([
            accent_srgba.red,
            accent_srgba.green,
            accent_srgba.blue,
            accent_srgba.alpha,
        ]);

        let mut scene = bevy_vello::vello::Scene::new();
        let rect = RoundedRect::new(0.0, 0.0, 280.0, 300.0, 6.0);
        scene.fill(Fill::NonZero, Affine::IDENTITY, vello_bg, None, &rect);

        // Left accent stripe (3px wide)
        let stripe = RoundedRect::new(0.0, 0.0, 3.0, 300.0, 0.0);
        scene.fill(Fill::NonZero, Affine::IDENTITY, vello_accent, None, &stripe);

        *bg = UiVelloScene::from(scene);
    }

    // Label (14pt)
    let label_text = node.label.as_deref().unwrap_or("(unnamed)");
    let label = spawn_detail_text(&mut commands, font, label_text, theme.fg, 14.0);
    commands.entity(sidebar_entity).add_child(label);

    // Context ID short (9pt, dim)
    let id_text = focus_id.short();
    let id_ent = spawn_detail_text(&mut commands, font, &id_text, theme.fg_dim, 9.0);
    commands.entity(sidebar_entity).add_child(id_ent);

    // Divider
    let div1 = spawn_divider(&mut commands, &theme);
    commands.entity(sidebar_entity).add_child(div1);

    // Model (11pt)
    let model_text = node.model.as_deref().unwrap_or("(no model)");
    let model_short = model_text.rsplit('/').next().unwrap_or(model_text);
    let model_ent = spawn_detail_text(&mut commands, font, model_short, theme.fg, 11.0);
    commands.entity(sidebar_entity).add_child(model_ent);

    // Provider (10pt, agent color)
    if let Some(ref provider) = node.provider {
        let provider_ent = spawn_detail_text(&mut commands, font, provider, agent_color, 10.0);
        commands.entity(sidebar_entity).add_child(provider_ent);
    }

    // Fork kind badge + lineage
    if let Some(ref fork_kind) = node.fork_kind {
        let badge_text = format!("[{}]", fork_kind);
        let badge = spawn_detail_text(&mut commands, font, &badge_text, theme.fg_dim, 10.0);
        commands.entity(sidebar_entity).add_child(badge);
    }

    if let Some(parent_id) = node.forked_from {
        let parent_short = parent_id.short();
        let parent_label = constellation
            .node_by_id(parent_id)
            .and_then(|n| n.label.as_deref())
            .unwrap_or(&parent_short);
        let lineage_text = format!("from: {}", parent_label);
        let lineage = spawn_detail_text(&mut commands, font, &lineage_text, theme.fg_dim, 9.0);
        commands.entity(sidebar_entity).add_child(lineage);
    }

    // Divider
    let div2 = spawn_divider(&mut commands, &theme);
    commands.entity(sidebar_entity).add_child(div2);

    // Activity state (10pt, colored by state)
    let (activity_text, activity_color) = match node.activity {
        super::ActivityState::Idle => ("idle", theme.fg_dim),
        super::ActivityState::Active => ("active", theme.fg),
        super::ActivityState::Streaming => ("streaming", theme.ansi.green),
        super::ActivityState::Waiting => ("waiting", theme.ansi.yellow),
        super::ActivityState::Error => ("error", theme.ansi.red),
        super::ActivityState::Completed => ("completed", theme.ansi.cyan),
    };
    let activity_ent = spawn_detail_text(&mut commands, font, activity_text, activity_color, 10.0);
    commands.entity(sidebar_entity).add_child(activity_ent);

    // Recency
    let recency = super::render2d::format_recency(node.last_activity_time, time.elapsed_secs_f64());
    let recency_ent = spawn_detail_text(&mut commands, font, &recency, theme.fg_dim, 9.0);
    commands.entity(sidebar_entity).add_child(recency_ent);

    // Joined indicator
    if node.joined {
        let joined = spawn_detail_text(&mut commands, font, "joined", theme.ansi.green, 9.0);
        commands.entity(sidebar_entity).add_child(joined);
    }

    // Synthesis keywords
    if !node.keywords.is_empty() {
        let div3 = spawn_divider(&mut commands, &theme);
        commands.entity(sidebar_entity).add_child(div3);

        let kw_header = spawn_detail_text(&mut commands, font, "keywords", theme.fg_dim, 9.0);
        commands.entity(sidebar_entity).add_child(kw_header);

        let kw_text = node.keywords.join(", ");
        let kw_ent = spawn_detail_text(&mut commands, font, &kw_text, theme.fg, 10.0);
        commands.entity(sidebar_entity).add_child(kw_ent);
    }

    // Representative block preview
    if let Some(ref preview) = node.top_block_preview {
        if !preview.is_empty() {
            let preview_header = spawn_detail_text(&mut commands, font, "representative", theme.fg_dim, 9.0);
            commands.entity(sidebar_entity).add_child(preview_header);

            let preview_ent = spawn_detail_text(&mut commands, font, preview, theme.fg_dim, 9.0);
            commands.entity(sidebar_entity).add_child(preview_ent);
        }
    }

    *last_fingerprint = fingerprint;
}

fn spawn_detail_text(
    commands: &mut Commands,
    font: &Handle<bevy_vello::prelude::VelloFont>,
    text: &str,
    color: Color,
    font_size: f32,
) -> Entity {
    commands
        .spawn((
            DetailContent,
            Node {
                min_height: Val::Px(font_size + 4.0),
                ..default()
            },
        ))
        .with_children(|parent| {
            parent.spawn((
                UiVelloText {
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

fn spawn_divider(
    commands: &mut Commands,
    theme: &Theme,
) -> Entity {
    let border_srgba = theme.border.to_srgba();
    let vello_border = VelloColor::new([
        border_srgba.red,
        border_srgba.green,
        border_srgba.blue,
        0.3,
    ]);

    let mut scene = bevy_vello::vello::Scene::new();
    let line_rect = RoundedRect::new(0.0, 0.0, 256.0, 1.0, 0.0);
    scene.fill(Fill::NonZero, Affine::IDENTITY, vello_border, None, &line_rect);

    commands
        .spawn((
            DetailContent,
            UiVelloScene::from(scene),
            Node {
                width: Val::Percent(100.0),
                height: Val::Px(1.0),
                margin: UiRect::vertical(Val::Px(4.0)),
                ..default()
            },
        ))
        .id()
}
