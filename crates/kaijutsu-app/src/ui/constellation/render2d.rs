//! Card-based rendering for constellation nodes.
//!
//! Each context node is a Bevy UI entity tree (card) absolutely positioned
//! within the ConstellationContainer. Edges are drawn as a single Vello scene
//! underneath cards.

use bevy::prelude::*;
use bevy_vello::prelude::{UiVelloScene, UiVelloText};
use bevy_vello::vello::kurbo::{Affine, BezPath, Cap, Point, RoundedRect, Stroke};
use bevy_vello::vello::peniko::{Color as VelloColor, Fill};

use kaijutsu_types::ContextId;

use super::{
    ActivityState, Constellation, ConstellationCamera, ConstellationContainer, ConstellationNode,
};
use crate::text::truncate_chars;
use crate::ui::screen::Screen;
use crate::ui::theme::{agent_color_for_provider, Theme};

/// Card height estimate for positioning (3 text lines + padding).
const CARD_HEIGHT: f32 = 75.0;
/// Vertical center fraction for card positioning (slightly below center for visual comfort).
const CAROUSEL_VERTICAL_CENTER: f32 = 0.55;
/// Corner radius for card background.
const CARD_CORNER_RADIUS: f64 = 6.0;
/// Border thickness for card outlines.
const CARD_BORDER_WIDTH: f64 = 2.0;

/// Register card rendering systems.
pub fn setup_render2d_systems(app: &mut App) {
    app.init_resource::<CardEntityMap>()
        .add_systems(
            Update,
            (
                sync_card_entities,
                update_card_positions,
                update_card_visuals,
                rebuild_edge_scene,
            )
                .chain()
                .run_if(in_state(Screen::Constellation)),
        );
}

// ============================================================================
// Components & Resources
// ============================================================================

/// Maps context_id → card root entity for lifecycle management.
#[derive(Resource, Default)]
struct CardEntityMap {
    map: std::collections::HashMap<ContextId, Entity>,
}

/// Marker on the card root entity.
#[derive(Component)]
struct CardMarker;

/// Marker on the card background entity (UiVelloScene for rounded rect).
#[derive(Component)]
struct CardBg;

/// Marker on the label text leaf.
#[derive(Component)]
struct CardLabelText;

/// Marker on the model text leaf.
#[derive(Component)]
struct CardModelText;

/// Marker on the recency text leaf.
#[derive(Component)]
struct CardRecencyText;

/// Marker for the edge scene entity (Bezier curves between cards).
#[derive(Component)]
pub struct ConstellationSceneMarker;

// ============================================================================
// Systems
// ============================================================================

/// Spawn/despawn card entities to match constellation nodes.
fn sync_card_entities(
    mut commands: Commands,
    constellation: Res<Constellation>,
    theme: Res<Theme>,
    font_handles: Res<crate::text::FontHandles>,
    container_q: Query<Entity, With<ConstellationContainer>>,
    mut card_map: ResMut<CardEntityMap>,
) {
    if !constellation.is_changed() {
        return;
    }

    let Ok(container_entity) = container_q.single() else {
        return;
    };

    let card_width = theme.constellation_card_width;

    // Current node IDs
    let current_ids: std::collections::HashSet<ContextId> = constellation
        .nodes
        .iter()
        .map(|n| n.context_id)
        .collect();

    // Despawn cards for removed nodes
    let stale: Vec<ContextId> = card_map
        .map
        .keys()
        .filter(|id| !current_ids.contains(id))
        .copied()
        .collect();
    for id in stale {
        if let Some(entity) = card_map.map.remove(&id) {
            commands.entity(entity).despawn();
        }
    }

    // Spawn cards for new nodes
    for node in &constellation.nodes {
        if card_map.map.contains_key(&node.context_id) {
            continue;
        }

        let card_entity = spawn_card(&mut commands, node, &theme, &font_handles, card_width);
        commands.entity(container_entity).add_child(card_entity);
        card_map.map.insert(node.context_id, card_entity);
    }
}

/// Spawn a single card entity tree.
fn spawn_card(
    commands: &mut Commands,
    node: &super::ContextNode,
    theme: &Theme,
    font_handles: &crate::text::FontHandles,
    card_width: f32,
) -> Entity {
    let label_text = card_label_text(node);
    let model_text = card_model_text(node);

    let card_root = commands
        .spawn((
            CardMarker,
            ConstellationNode {
                context_id: node.context_id.to_string(),
            },
            Interaction::default(),
            Node {
                position_type: PositionType::Absolute,
                width: Val::Px(card_width),
                min_height: Val::Px(CARD_HEIGHT),
                overflow: Overflow::visible(),
                ..default()
            },
        ))
        .with_children(|parent| {
            // Background scene (rounded rect + border) — absolute, covers card
            parent.spawn((
                CardBg,
                UiVelloScene::default(),
                Node {
                    position_type: PositionType::Absolute,
                    width: Val::Percent(100.0),
                    height: Val::Percent(100.0),
                    ..default()
                },
            ));

            // Content column with text leaves
            parent
                .spawn(Node {
                    flex_direction: FlexDirection::Column,
                    padding: UiRect::all(Val::Px(8.0)),
                    row_gap: Val::Px(3.0),
                    width: Val::Percent(100.0),
                    position_type: PositionType::Relative,
                    ..default()
                })
                .with_children(|content| {
                    // Label
                    content.spawn((
                        CardLabelText,
                        UiVelloText {
                            value: label_text,
                            style: crate::text::vello_style(
                                &font_handles.mono,
                                theme.fg,
                                12.0,
                            ),
                            ..default()
                        },
                        Node::default(),
                    ));

                    // Model
                    content.spawn((
                        CardModelText,
                        UiVelloText {
                            value: model_text,
                            style: crate::text::vello_style(
                                &font_handles.mono,
                                theme.fg_dim,
                                10.0,
                            ),
                            ..default()
                        },
                        Node::default(),
                    ));

                    // Recency
                    content.spawn((
                        CardRecencyText,
                        UiVelloText {
                            value: "—".into(),
                            style: crate::text::vello_style(
                                &font_handles.mono,
                                theme.fg_dim,
                                9.0,
                            ),
                            ..default()
                        },
                        Node::default(),
                    ));
                });
        })
        .id();

    card_root
}

/// Update card absolute positions, scale, and depth sorting each frame.
fn update_card_positions(
    mut commands: Commands,
    constellation: Res<Constellation>,
    camera: Res<ConstellationCamera>,
    theme: Res<Theme>,
    container_q: Query<&ComputedNode, With<ConstellationContainer>>,
    card_map: Res<CardEntityMap>,
    mut card_nodes: Query<(&mut Node, &mut Visibility), With<CardMarker>>,
) {
    let Ok(computed) = container_q.single() else {
        return;
    };

    let viewport_size = computed.size();
    if viewport_size.x < 1.0 || viewport_size.y < 1.0 {
        return;
    }

    // Center cards slightly below vertical center (60%) for visual comfort
    let center = Vec2::new(viewport_size.x / 2.0, viewport_size.y * CAROUSEL_VERTICAL_CENTER);
    let base_card_width = theme.constellation_card_width;

    for node in &constellation.nodes {
        let Some(&entity) = card_map.map.get(&node.context_id) else {
            continue;
        };
        let Ok((mut style, mut vis)) = card_nodes.get_mut(entity) else {
            continue;
        };

        // Depth-based scale: front (depth=1) → 1.0, back (depth=-1) → 0.4
        let depth_t = (node.depth + 1.0) / 2.0; // 0..1
        let scale = 0.4 + 0.6 * depth_t;

        let card_w = base_card_width * scale;
        let card_h = CARD_HEIGHT * scale;

        let screen_pos = center + (node.position + camera.offset) * camera.zoom;
        style.left = Val::Px(screen_pos.x - card_w / 2.0);
        style.top = Val::Px(screen_pos.y - card_h / 2.0);
        style.width = Val::Px(card_w);
        style.min_height = Val::Px(card_h);

        // Hide cards that are very far back (behind the ring)
        if node.depth < -0.85 {
            *vis = Visibility::Hidden;
        } else {
            *vis = Visibility::Inherited;
        }

        // ZIndex by depth: front cards render on top
        let z = (depth_t * 100.0) as i32;
        commands.entity(entity).insert(ZIndex(z));
    }
    // card_map.map is now HashMap<ContextId, Entity> — direct lookup, no string conversion
}

/// Update card visuals: background scenes, text content, focus highlighting.
fn update_card_visuals(
    constellation: Res<Constellation>,
    theme: Res<Theme>,
    font_handles: Res<crate::text::FontHandles>,
    time: Res<Time>,
    card_map: Res<CardEntityMap>,
    children_q: Query<&Children>,
    mut bg_q: Query<&mut UiVelloScene, With<CardBg>>,
    mut label_q: Query<&mut UiVelloText, (With<CardLabelText>, Without<CardModelText>, Without<CardRecencyText>)>,
    mut model_q: Query<&mut UiVelloText, (With<CardModelText>, Without<CardLabelText>, Without<CardRecencyText>)>,
    mut recency_q: Query<&mut UiVelloText, (With<CardRecencyText>, Without<CardLabelText>, Without<CardModelText>)>,
) {
    let elapsed = time.elapsed_secs();
    let elapsed_f64 = time.elapsed_secs_f64();
    let base_card_width = theme.constellation_card_width;

    for node in &constellation.nodes {
        let Some(&card_entity) = card_map.map.get(&node.context_id) else {
            continue;
        };

        let is_focused = constellation.focus_id == Some(node.context_id);
        let bevy_color = agent_color_for_provider(&theme, node.provider.as_deref());
        let vello_color = bevy_to_vello_color(bevy_color);

        // Build background scene (scaled to match depth-adjusted card size)
        let depth_t = (node.depth + 1.0) / 2.0;
        let scale = 0.4 + 0.6 * depth_t;
        let card_w = (base_card_width * scale) as f64;
        let card_h = (CARD_HEIGHT * scale) as f64;

        let mut scene = bevy_vello::vello::Scene::new();
        let rect = RoundedRect::new(0.0, 0.0, card_w, card_h, CARD_CORNER_RADIUS);

        // Depth-based opacity: front=1.0, back=0.3
        let depth_t = (node.depth + 1.0) / 2.0;
        let depth_alpha = 0.3 + 0.7 * depth_t;

        // Fill
        let base_alpha = if is_focused {
            0.95
        } else if node.joined {
            0.90
        } else {
            0.80
        };
        let fill_alpha = base_alpha * depth_alpha;
        let fill_color = if is_focused {
            VelloColor::new([0.12, 0.13, 0.18, fill_alpha])
        } else if node.joined {
            VelloColor::new([0.10, 0.11, 0.15, fill_alpha])
        } else {
            VelloColor::new([0.08, 0.09, 0.12, fill_alpha])
        };
        scene.fill(Fill::NonZero, Affine::IDENTITY, fill_color, None, &rect);

        // Border (modulated by depth)
        let border_alpha = if is_focused {
            1.0
        } else if node.joined {
            0.6
        } else {
            0.3
        } * depth_alpha;
        let border_color = multiply_alpha(vello_color, border_alpha);

        // Border thickness: focused = 3px, streaming = pulsing, others = 1.5px
        let base_stroke = if is_focused { 3.0 } else { CARD_BORDER_WIDTH };
        let stroke_width = if node.activity == ActivityState::Streaming {
            let pulse = (elapsed * 3.0).sin() * 0.5 + 0.5;
            base_stroke + pulse as f64 * 1.5
        } else {
            base_stroke
        };

        scene.stroke(
            &Stroke::new(stroke_width),
            Affine::IDENTITY,
            border_color,
            None,
            &rect,
        );

        // Error accent: red left edge stripe
        if node.activity == ActivityState::Error {
            let error_color = VelloColor::new([0.97, 0.46, 0.56, 0.8]);
            let error_rect = RoundedRect::new(0.0, 0.0, 3.0, card_h, 0.0);
            scene.fill(Fill::NonZero, Affine::IDENTITY, error_color, None, &error_rect);
        }

        // Apply scene to CardBg
        let Ok(children) = children_q.get(card_entity) else {
            continue;
        };
        for child in children.iter() {
            if let Ok(mut bg_scene) = bg_q.get_mut(child) {
                *bg_scene = UiVelloScene::from(scene.clone());
            }

            // Update text in content column children
            let Ok(grandchildren) = children_q.get(child) else {
                continue;
            };
            for gc in grandchildren.iter() {
                if let Ok(mut text) = label_q.get_mut(gc) {
                    let new_val = card_label_text(node);
                    if text.value != new_val {
                        text.value = new_val;
                        // Update color based on focus
                        text.style = crate::text::vello_style(
                            &font_handles.mono,
                            if is_focused { theme.fg } else if node.joined { theme.fg } else { theme.fg_dim },
                            12.0,
                        );
                    }
                }
                if let Ok(mut text) = model_q.get_mut(gc) {
                    let new_val = card_model_text(node);
                    if text.value != new_val {
                        text.value = new_val;
                    }
                }
                if let Ok(mut text) = recency_q.get_mut(gc) {
                    let new_val = format_recency(node.last_activity_time, elapsed_f64);
                    if text.value != new_val {
                        text.value = new_val;
                    }
                }
            }
        }
    }
}

/// Rebuild the edge scene (Bezier curves connecting parent→child card centers).
fn rebuild_edge_scene(
    mut commands: Commands,
    constellation: Res<Constellation>,
    camera: Res<ConstellationCamera>,
    theme: Res<Theme>,
    container_q: Query<(Entity, &ComputedNode), With<ConstellationContainer>>,
    scene_q: Query<Entity, With<ConstellationSceneMarker>>,
) {
    let Ok((container_entity, computed)) = container_q.single() else {
        return;
    };

    let viewport_size = computed.size();
    if viewport_size.x < 1.0 || viewport_size.y < 1.0 {
        return;
    }

    let center = Vec2::new(viewport_size.x / 2.0, viewport_size.y * CAROUSEL_VERTICAL_CENTER);

    // Build id → screen position map
    let positions: std::collections::HashMap<ContextId, Vec2> = constellation
        .nodes
        .iter()
        .map(|n| {
            let screen_pos = center + (n.position + camera.offset) * camera.zoom;
            (n.context_id, screen_pos)
        })
        .collect();

    let edge_color = bevy_to_vello_color(theme.fg_dim.with_alpha(0.4));

    let mut vello_scene = bevy_vello::vello::Scene::new();

    for node in &constellation.nodes {
        let Some(parent_id) = node.parent_id else {
            continue;
        };
        let Some(&from) = positions.get(&parent_id) else {
            continue;
        };
        let Some(&to) = positions.get(&node.context_id) else {
            continue;
        };

        draw_edge(&mut vello_scene, from, to, edge_color);
    }

    // Update or spawn the edge scene
    if let Some(scene_entity) = scene_q.iter().next() {
        commands
            .entity(scene_entity)
            .insert(UiVelloScene::from(vello_scene));
    } else {
        let scene_entity = commands
            .spawn((
                ConstellationSceneMarker,
                UiVelloScene::from(vello_scene),
                Node {
                    position_type: PositionType::Absolute,
                    width: Val::Percent(100.0),
                    height: Val::Percent(100.0),
                    ..default()
                },
            ))
            .id();
        commands.entity(container_entity).add_child(scene_entity);
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// Primary label for a card: label > short context ID.
fn card_label_text(node: &super::ContextNode) -> String {
    if let Some(ref label) = node.label {
        truncate_chars(label, 20)
    } else {
        node.context_id.short()
    }
}

/// Model display text for a card.
fn card_model_text(node: &super::ContextNode) -> String {
    if let Some(ref model) = node.model {
        let short = model.rsplit('/').next().unwrap_or(model.as_str());
        truncate_chars(short, 22)
    } else {
        "(no model)".to_string()
    }
}

/// Format recency as a human-readable relative time.
fn format_recency(last_activity: f64, now: f64) -> String {
    if last_activity <= 0.0 {
        return "—".to_string();
    }
    let delta = now - last_activity;
    if delta < 5.0 {
        "just now".to_string()
    } else if delta < 60.0 {
        format!("{}s ago", delta as u64)
    } else if delta < 3600.0 {
        format!("{}m ago", (delta / 60.0) as u64)
    } else {
        format!("{}h ago", (delta / 3600.0) as u64)
    }
}

/// Draw a single edge as a quadratic Bezier curve.
fn draw_edge(scene: &mut bevy_vello::vello::Scene, from: Vec2, to: Vec2, color: VelloColor) {
    let from_pt = Point::new(from.x as f64, from.y as f64);
    let to_pt = Point::new(to.x as f64, to.y as f64);

    // Control point offset perpendicular to line
    let mid = Point::new(
        (from.x + to.x) as f64 / 2.0,
        (from.y + to.y) as f64 / 2.0,
    );
    let delta = Vec2::new(to.x - from.x, to.y - from.y);
    let perp = Vec2::new(-delta.y, delta.x).normalize_or_zero() * 15.0;
    let control = Point::new(mid.x + perp.x as f64, mid.y + perp.y as f64);

    let mut path = BezPath::new();
    path.move_to(from_pt);
    path.quad_to(control, to_pt);

    let stroke = Stroke::new(1.5).with_caps(Cap::Round);
    scene.stroke(&stroke, Affine::IDENTITY, color, None, &path);
}

// ============================================================================
// Color helpers
// ============================================================================

fn bevy_to_vello_color(color: Color) -> VelloColor {
    let srgba = color.to_srgba();
    VelloColor::new([srgba.red, srgba.green, srgba.blue, srgba.alpha])
}

fn multiply_alpha(color: VelloColor, factor: f32) -> VelloColor {
    let [r, g, b, a] = color.components;
    VelloColor::new([r, g, b, a * factor])
}
