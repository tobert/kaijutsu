use bevy::asset::RenderAssetUsages;
use bevy::camera::visibility::{NoFrustumCulling, RenderLayers};
use bevy::color::{ColorToComponents, Mix};
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;

use crate::view::scene_palette::HeroicPalette;

use super::{SceneViewport, STAGE_LAYER, layout};

const DEPTH: f32 = -5.0;
const HORIZON: f32 = -0.32;

#[derive(Component)]
pub(in crate::view::neo) struct Dusk;

fn frame_size(aspect: f32) -> (f32, f32) {
    let height = 2.0 * (layout::fov(aspect) * 0.5).tan() * (layout::CAMERA_DISTANCE - DEPTH);
    (height * aspect, height)
}

fn grid_strength(distance: f32) -> f32 {
    distance.clamp(0.0, 1.0).powf(1.4) * 0.24
}

fn quad(positions: &mut Vec<[f32; 3]>, colors: &mut Vec<[f32; 4]>, indices: &mut Vec<u32>,
    points: [[f32; 3]; 4], shades: [[f32; 4]; 4],
) {
    let base = positions.len() as u32;
    positions.extend(points);
    colors.extend(shades);
    indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
}

fn dusk_mesh(palette: &HeroicPalette) -> Mesh {
    let mut positions = Vec::new();
    let mut colors = Vec::new();
    let mut indices = Vec::new();
    let sky = [
        (1.0, palette.sky_top.to_f32_array()),
        (0.35, palette.sky_mid.to_f32_array()),
        (-0.18, palette.sky_low.mix(&palette.horizon, 0.08).to_f32_array()),
        (HORIZON, palette.sky_low.mix(&palette.horizon, 0.25).to_f32_array()),
        (-0.43, palette.ground.to_f32_array()),
        (-1.0, palette.ground.to_f32_array()),
    ];
    for pair in sky.windows(2) {
        let (top, a) = pair[0];
        let (bottom, b) = pair[1];
        quad(&mut positions, &mut colors, &mut indices,
            [[-1.0, bottom, 0.0], [1.0, bottom, 0.0], [1.0, top, 0.0], [-1.0, top, 0.0]],
            [b, b, a, a]);
    }
    let floor_color = |distance: f32| {
        let horizon = palette.sky_low.mix(&palette.horizon, 0.25);
        let base = palette.ground.mix(&horizon, (1.0 - distance).powf(6.0));
        base.mix(&palette.rail, grid_strength(distance)).to_f32_array()
    };
    let bottom = floor_color(1.0);
    let horizon = floor_color(0.0);
    for x in -12..=12 {
        let x = x as f32 / 6.0;
        let width = 0.002;
        quad(&mut positions, &mut colors, &mut indices,
            [[x - width, -1.0, 0.01], [x + width, -1.0, 0.01],
                [0.0, HORIZON, 0.01], [0.0, HORIZON, 0.01]],
            [bottom, bottom, horizon, horizon]);
    }
    for i in 1..=9 {
        let distance = (i as f32 / 9.0).powi(2);
        let y = HORIZON + (-1.0 - HORIZON) * distance;
        let thickness = 0.001 + distance * 0.003;
        let color = floor_color(distance);
        quad(&mut positions, &mut colors, &mut indices,
            [[-1.0, y - thickness, 0.01], [1.0, y - thickness, 0.01],
                [1.0, y + thickness, 0.01], [-1.0, y + thickness, 0.01]],
            [color; 4]);
    }
    let normals = vec![[0.0, 0.0, 1.0]; positions.len()];
    Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default())
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
        .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, colors)
        .with_inserted_indices(Indices::U32(indices))
}

pub(super) fn spawn(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    palette: &HeroicPalette,
) {
    let (width, height) = frame_size(16.0 / 9.0);
    commands.spawn((
        Name::new("Neo Dusk Horizon"),
        Dusk,
        Mesh3d(meshes.add(dusk_mesh(palette))),
        MeshMaterial3d(materials.add(StandardMaterial { unlit: true, ..default() })),
        Transform::from_xyz(0.0, 0.0, DEPTH).with_scale(Vec3::new(width * 0.5, height * 0.5, 1.0)),
        NoFrustumCulling,
        RenderLayers::layer(STAGE_LAYER),
    ));
}

pub(in crate::view::neo) fn sync(
    palette: Res<HeroicPalette>,
    viewport: Query<&ComputedNode, With<SceneViewport>>,
    mut dusk: Query<(&Mesh3d, &mut Transform), With<Dusk>>,
    mut meshes: ResMut<Assets<Mesh>>,
) {
    let Ok((mesh, mut transform)) = dusk.single_mut() else { return };
    if palette.is_changed() && let Some(mut mesh) = meshes.get_mut(&mesh.0) {
        *mesh = dusk_mesh(&palette);
    }
    let Ok(bounds) = viewport.single() else { return };
    if bounds.is_empty() { return; }
    let aspect = bounds.size().x / bounds.size().y;
    let (width, height) = frame_size(aspect);
    transform.set_if_neq(Transform::from_xyz(0.0, 0.0, DEPTH)
        .with_scale(Vec3::new(width * 0.5, height * 0.5, 1.0)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn horizon_scales_with_wide_and_narrow_viewports() {
        for aspect in [0.75, 1.0, 16.0 / 9.0, 2.75] {
            let (width, height) = frame_size(aspect);
            assert!((width / height - aspect).abs() < 0.001);
            assert!(height >= 7.2);
        }
    }

    #[test]
    fn floor_lines_fade_toward_horizon() {
        assert!(grid_strength(0.05) < grid_strength(0.5));
        assert!(grid_strength(0.5) < grid_strength(1.0));
        assert_eq!(grid_strength(0.0), 0.0);
    }
}
