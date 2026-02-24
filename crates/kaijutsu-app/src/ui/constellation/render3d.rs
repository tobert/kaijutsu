//! 3D rendering for constellation nodes and edges in the Poincaré ball.
//!
//! Replaces 2D UiMaterial cards with 3D billboard quads positioned using the
//! H3 layout engine. Edges are rendered as line segments. Labels use screen-space
//! projected MSDF text overlays.
//!
//! All 3D entities use `RenderLayers::layer(1)` and are rendered by the
//! constellation viewport camera.

use bevy::{
    asset::RenderAssetUsages,
    camera::visibility::RenderLayers,
    mesh::PrimitiveTopology,
    prelude::*,
};

use super::{
    Constellation,
    hyper::LorentzTransform,
    layout::H3Layout,
    viewport::{ViewportState, TestSphere},
};
use crate::ui::theme::{Theme, agent_color_for_provider};

/// The render layer used for all constellation 3D content.
const CONSTELLATION_LAYER: usize = 1;

/// Marker for a 3D node entity in the constellation scene.
#[derive(Component)]
pub struct Node3d {
    pub context_id: String,
}

/// Marker for the edge line mesh entity.
#[derive(Component)]
pub struct EdgeMesh;

/// Resource holding the H3 layout and focus transform.
#[derive(Resource)]
pub struct ConstellationScene {
    pub layout: H3Layout,
    pub focus_transform: LorentzTransform,
    /// Cached ball positions (after focus transform projection).
    pub ball_positions: Vec<Vec3>,
}

impl Default for ConstellationScene {
    fn default() -> Self {
        Self {
            layout: H3Layout::default(),
            focus_transform: LorentzTransform::IDENTITY,
            ball_positions: Vec::new(),
        }
    }
}

/// Register 3D rendering systems.
pub fn setup_render3d_systems(app: &mut App) {
    app.init_resource::<ConstellationScene>()
        .add_systems(
            Update,
            (
                update_layout,
                spawn_3d_nodes,
                despawn_stale_3d_nodes,
                update_3d_node_transforms,
                update_3d_node_visuals,
                rebuild_3d_edges,
                cleanup_test_spheres,
            )
                .chain(),
        );
}

/// Recompute the H3 layout when constellation data changes.
fn update_layout(
    constellation: Res<Constellation>,
    theme: Res<Theme>,
    mut scene: ResMut<ConstellationScene>,
) {
    if !constellation.is_changed() && !theme.is_changed() {
        return;
    }

    if constellation.nodes.is_empty() {
        scene.layout.nodes.clear();
        scene.ball_positions.clear();
        return;
    }

    // Update layout params from theme
    scene.layout.base_leaf_radius = theme.constellation_base_leaf_radius;
    scene.layout.packing_factor = theme.constellation_packing_factor;

    // Extract ids and parents
    let ids: Vec<String> = constellation.nodes.iter().map(|n| n.context_id.clone()).collect();
    let parents: Vec<Option<String>> = constellation.nodes.iter().map(|n| n.parent_id.clone()).collect();

    scene.layout.full_layout(&ids, &parents);
    scene.ball_positions = scene.layout.project_all(&scene.focus_transform);
}

/// Spawn 3D mesh entities for new constellation nodes.
fn spawn_3d_nodes(
    mut commands: Commands,
    constellation: Res<Constellation>,
    scene: Res<ConstellationScene>,
    theme: Res<Theme>,
    viewport_state: Res<ViewportState>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    existing: Query<&Node3d>,
) {
    if viewport_state.camera_entity.is_none() {
        return;
    }

    if scene.ball_positions.len() != constellation.nodes.len() {
        return; // Layout not yet computed
    }

    let existing_ids: Vec<&str> = existing.iter().map(|n| n.context_id.as_str()).collect();
    let sphere_mesh = meshes.add(Sphere::new(0.04));

    for (i, node) in constellation.nodes.iter().enumerate() {
        if existing_ids.contains(&node.context_id.as_str()) {
            continue;
        }

        let ball_pos = scene.ball_positions[i];
        let agent_color = agent_color_for_provider(&theme, node.provider.as_deref());
        let srgba = agent_color.to_srgba();

        commands.spawn((
            Node3d {
                context_id: node.context_id.clone(),
            },
            Mesh3d(sphere_mesh.clone()),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color: agent_color,
                emissive: LinearRgba::new(
                    srgba.red * 2.0,
                    srgba.green * 2.0,
                    srgba.blue * 2.0,
                    1.0,
                ),
                ..default()
            })),
            Transform::from_translation(ball_pos),
            RenderLayers::layer(CONSTELLATION_LAYER),
        ));
    }
}

/// Despawn 3D node entities that no longer exist in the constellation.
fn despawn_stale_3d_nodes(
    mut commands: Commands,
    constellation: Res<Constellation>,
    nodes: Query<(Entity, &Node3d)>,
) {
    for (entity, node) in nodes.iter() {
        if constellation.node_by_id(&node.context_id).is_none() {
            commands.entity(entity).despawn();
        }
    }
}

/// Update 3D node positions from projected ball coordinates.
fn update_3d_node_transforms(
    constellation: Res<Constellation>,
    scene: Res<ConstellationScene>,
    mut nodes: Query<(&Node3d, &mut Transform), Without<EdgeMesh>>,
) {
    if scene.ball_positions.len() != constellation.nodes.len() {
        return;
    }

    for (node_marker, mut transform) in nodes.iter_mut() {
        if let Some(idx) = constellation
            .nodes
            .iter()
            .position(|n| n.context_id == node_marker.context_id)
        {
            let target = scene.ball_positions[idx];
            // Smooth lerp toward target position
            transform.translation = transform.translation.lerp(target, 0.15);
        }
    }
}

/// Update 3D node visual properties (size, color) based on activity and focus.
fn update_3d_node_visuals(
    constellation: Res<Constellation>,
    theme: Res<Theme>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut nodes: Query<(&Node3d, &MeshMaterial3d<StandardMaterial>, &mut Transform)>,
) {
    for (node_marker, material_handle, mut transform) in nodes.iter_mut() {
        let Some(ctx_node) = constellation.node_by_id(&node_marker.context_id) else {
            continue;
        };

        let is_focused = constellation.focus_id.as_deref() == Some(&ctx_node.context_id);

        // Scale: focused nodes are larger
        let scale = if is_focused {
            1.5
        } else {
            match ctx_node.activity {
                super::ActivityState::Streaming | super::ActivityState::Active => 1.2,
                _ => 1.0,
            }
        };
        transform.scale = Vec3::splat(scale);

        // Update material color based on activity
        if let Some(mat) = materials.get_mut(material_handle.0.id()) {
            let agent_color = agent_color_for_provider(&theme, ctx_node.provider.as_deref());
            mat.base_color = agent_color;

            let srgba = agent_color.to_srgba();
            let emissive_strength = if is_focused {
                4.0
            } else {
                ctx_node.activity.glow_intensity() * 3.0
            };
            mat.emissive = LinearRgba::new(
                srgba.red * emissive_strength,
                srgba.green * emissive_strength,
                srgba.blue * emissive_strength,
                1.0,
            );
        }
    }
}

/// Rebuild the edge line mesh when layout changes.
fn rebuild_3d_edges(
    mut commands: Commands,
    constellation: Res<Constellation>,
    scene: Res<ConstellationScene>,
    viewport_state: Res<ViewportState>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    existing_edges: Query<Entity, With<EdgeMesh>>,
) {
    if !constellation.is_changed() || viewport_state.camera_entity.is_none() {
        return;
    }

    if scene.ball_positions.len() != constellation.nodes.len() {
        return;
    }

    // Despawn old edge mesh
    for entity in existing_edges.iter() {
        commands.entity(entity).despawn();
    }

    if constellation.nodes.len() < 2 {
        return;
    }

    // Build id → index map
    let id_to_idx: std::collections::HashMap<&str, usize> = constellation
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.context_id.as_str(), i))
        .collect();

    // Collect edge vertex pairs
    let mut positions: Vec<[f32; 3]> = Vec::new();

    for (i, node) in constellation.nodes.iter().enumerate() {
        if let Some(ref parent_id) = node.parent_id {
            if let Some(&parent_idx) = id_to_idx.get(parent_id.as_str()) {
                let from = scene.ball_positions[parent_idx];
                let to = scene.ball_positions[i];
                positions.push(from.into());
                positions.push(to.into());
            }
        }
    }

    if positions.is_empty() {
        return;
    }

    let mut mesh = Mesh::new(PrimitiveTopology::LineList, RenderAssetUsages::all());
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);

    commands.spawn((
        EdgeMesh,
        Mesh3d(meshes.add(mesh)),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgba(0.3, 0.4, 0.7, 0.4),
            emissive: LinearRgba::new(0.2, 0.3, 0.5, 0.3),
            unlit: true,
            alpha_mode: AlphaMode::Blend,
            ..default()
        })),
        Transform::IDENTITY,
        RenderLayers::layer(CONSTELLATION_LAYER),
    ));
}

/// Remove Phase 1.5 test spheres once real nodes are being rendered.
fn cleanup_test_spheres(
    mut commands: Commands,
    scene: Res<ConstellationScene>,
    test_spheres: Query<Entity, With<TestSphere>>,
) {
    if scene.layout.nodes.is_empty() {
        return; // Keep test spheres until we have real data
    }

    for entity in test_spheres.iter() {
        commands.entity(entity).despawn();
    }
}
