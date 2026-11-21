//! Heroic view world environment (`docs/newui.md`, "Around the road" and "Shaders"):
//! sky gradient, stars, striped sun, ground plane, roadside magenta grid,
//! beat-locked edge fire, spectrogram hills, sky gliders, and optional context
//! constellation.

use std::f32::consts::FRAC_PI_2;

use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;

use super::state::HeroicState;
use crate::shaders::{HeroicFireMaterial, HeroicHillsMaterial, HeroicSunMaterial};
use crate::view::scene_palette::HeroicPalette;

/// Marker for the sky gradient backdrop quad.
#[derive(Component)]
pub struct HeroicSky;

/// Marker for the striped sun disc.
#[derive(Component)]
pub struct HeroicSun;

/// Marker for the dark ground plane.
#[derive(Component)]
pub struct HeroicGround;

/// Marker for left/right roadside edge fire quads.
#[derive(Component)]
pub struct HeroicFire {
    pub side: f32,
}

/// Marker for left/right spectrogram hill flank meshes.
#[derive(Component)]
pub struct HeroicHill {
    pub side: f32,
}

/// Marker for static roadside magenta grid lines.
#[derive(Component)]
pub struct HeroicGrid;

/// Marker for a sky glider crossing the sun (`idx` in 0..2).
#[derive(Component)]
pub struct HeroicGlider {
    pub idx: usize,
    pub span: f32,
}

/// Marker for a sky star or constellation node.
#[derive(Component)]
#[allow(dead_code)]
pub struct HeroicStar {
    pub idx: usize,
    pub is_constellation: bool,
}

/// Spawn the static and shader-driven environment under `root`.
pub fn spawn_environment(
    commands: &mut Commands,
    root: Entity,
    palette: &HeroicPalette,
    meshes: &mut Assets<Mesh>,
    mats: &mut Assets<StandardMaterial>,
    sun_mats: &mut Assets<HeroicSunMaterial>,
    fire_mats: &mut Assets<HeroicFireMaterial>,
    hill_mats: &mut Assets<HeroicHillsMaterial>,
) {
    // 1. Sky vertical gradient quad behind the sun (no fog).
    let sky_mesh = meshes.add(sky_gradient_mesh(palette));
    let sky_mat = mats.add(StandardMaterial {
        base_color: Color::WHITE,
        unlit: true,
        cull_mode: None,
        ..default()
    });
    commands.spawn((
        HeroicSky,
        Mesh3d(sky_mesh),
        MeshMaterial3d(sky_mat),
        Transform::from_xyz(0.0, 0.0, -(palette.sizes.sun_distance + 25.0)),
        Visibility::Inherited,
        Name::new("HeroicSky"),
        ChildOf(root),
    ));

    // 2. Striped sun disc centered at the vanishing point (half set at horizon y=0).
    let sun_d = palette.sizes.sun_diameter;
    let sun_mesh = meshes.add(Rectangle::new(sun_d, sun_d));
    let sun_mat = sun_mats.add(HeroicSunMaterial {
        sun_top: Vec4::new(palette.sun[0].red, palette.sun[0].green, palette.sun[0].blue, 1.0),
        sun_mid: Vec4::new(palette.sun[1].red, palette.sun[1].green, palette.sun[1].blue, 1.0),
        sun_bot: Vec4::new(palette.sun[2].red, palette.sun[2].green, palette.sun[2].blue, 1.0),
        params: Vec4::new(
            palette.intensity,
            0.0,
            if palette.reduced_motion { 1.0 } else { 0.0 },
            0.0,
        ),
    });
    commands.spawn((
        HeroicSun,
        Mesh3d(sun_mesh),
        MeshMaterial3d(sun_mat),
        Transform::from_xyz(0.0, 0.0, -palette.sizes.sun_distance),
        Visibility::Inherited,
        Name::new("HeroicSun"),
        ChildOf(root),
    ));

    // 3. Dark ground plane under the road and flanks.
    let ground_mesh = meshes.add(Plane3d::default().mesh().size(220.0, 220.0));
    let ground_mat = mats.add(StandardMaterial {
        base_color: Color::LinearRgba(palette.ground),
        unlit: true,
        ..default()
    });
    commands.spawn((
        HeroicGround,
        Mesh3d(ground_mesh),
        MeshMaterial3d(ground_mat),
        Transform::from_xyz(0.0, -0.04, -55.0),
        Visibility::Inherited,
        Name::new("HeroicGround"),
        ChildOf(root),
    ));

    // 4. Roadside magenta grid (single LineList mesh with distance-faded vertex colors).
    let grid_mesh = meshes.add(roadside_grid_mesh(palette));
    let grid_mat = mats.add(StandardMaterial {
        base_color: Color::WHITE,
        unlit: true,
        ..default()
    });
    commands.spawn((
        HeroicGrid,
        Mesh3d(grid_mesh),
        MeshMaterial3d(grid_mat),
        Transform::from_xyz(0.0, -0.02, 0.0),
        Visibility::Inherited,
        Name::new("HeroicRoadsideGrid"),
        ChildOf(root),
    ));

    // 5. Left and right edge fire quads.
    let road_len = (palette.road_seconds * palette.units_per_second).max(10.0);
    let fire_h = palette.sizes.fire_height;
    let fire_mesh = meshes.add(Rectangle::new(road_len, fire_h));
    for side in [-1.0_f32, 1.0] {
        let fire_mat = fire_mats.add(HeroicFireMaterial {
            params: Vec4::new(
                palette.units_per_second * 0.5,
                0.0,
                palette.intensity,
                if palette.reduced_motion { 1.0 } else { 0.0 },
            ),
            ramp: Vec4::new(
                palette.rainbow_lightness,
                palette.rainbow_chroma_max,
                road_len,
                palette.rainbow_chroma_min,
            ),
            fog_color_start: Vec4::new(
                palette.fog_color.red,
                palette.fog_color.green,
                palette.fog_color.blue,
                palette.fog.start,
            ),
            fog_end_pad: Vec4::new(palette.fog.end, 0.0, 0.0, 0.0),
            bands: [Vec4::ZERO; 4],
        });
        let x = side * (2.0 + palette.sizes.fire_offset);
        let rot = Quat::from_rotation_y(if side < 0.0 { FRAC_PI_2 } else { -FRAC_PI_2 });
        commands.spawn((
            HeroicFire { side },
            Mesh3d(fire_mesh.clone()),
            MeshMaterial3d(fire_mat),
            Transform::from_xyz(x, fire_h * 0.5, -road_len * 0.5).with_rotation(rot),
            Visibility::Inherited,
            Name::new(if side < 0.0 { "HeroicFireLeft" } else { "HeroicFireRight" }),
            ChildOf(root),
        ));
    }

    // 6. Left and right spectrogram hills (12 cells across x 32 deep).
    let hill_w = palette.sizes.hill_width;
    let hill_l = palette.sizes.hill_length;
    for side in [-1.0_f32, 1.0] {
        let hill_mesh = meshes.add(hill_grid_mesh(side, hill_w, hill_l));
        let hill_mat = hill_mats.add(HeroicHillsMaterial {
            params: Vec4::new(
                palette.intensity,
                0.0,
                palette.intensity,
                if palette.reduced_motion { 1.0 } else { 0.0 },
            ),
            grid: Vec4::new(
                palette.rainbow_lightness,
                palette.rainbow_chroma_max * 0.85,
                12.0,
                32.0,
            ),
            fill_fog_start: Vec4::new(
                palette.hill_fill.red,
                palette.hill_fill.green,
                palette.hill_fill.blue,
                palette.fog.start,
            ),
            fog_color_end: Vec4::new(
                palette.fog_color.red,
                palette.fog_color.green,
                palette.fog_color.blue,
                palette.fog.end,
            ),
            history: [Vec4::ZERO; 128],
        });
        commands.spawn((
            HeroicHill { side },
            Mesh3d(hill_mesh),
            MeshMaterial3d(hill_mat),
            Transform::from_xyz(side * palette.sizes.hill_inner_edge, 0.0, 0.0),
            Visibility::Inherited,
            Name::new(if side < 0.0 { "HeroicHillLeft" } else { "HeroicHillRight" }),
            ChildOf(root),
        ));
    }

    // 7. Plain sky stars (no fog, thin points).
    let star_mesh = meshes.add(Rectangle::new(0.14, 0.14));
    let star_mat = mats.add(StandardMaterial {
        base_color: Color::LinearRgba(LinearRgba::rgb(0.65, 0.55, 0.85)),
        unlit: true,
        ..default()
    });
    for i in 0..48 {
        let fx = ((i * 73 + 19) % 100) as f32 / 100.0 - 0.5;
        let fy = ((i * 41 + 7) % 100) as f32 / 100.0;
        let x = fx * 95.0;
        let y = 7.0 + fy * 34.0;
        let z = -(palette.sizes.sun_distance + 12.0);
        commands.spawn((
            HeroicStar {
                idx: i,
                is_constellation: false,
            },
            Mesh3d(star_mesh.clone()),
            MeshMaterial3d(star_mat.clone()),
            Transform::from_xyz(x, y, z),
            Visibility::Inherited,
            ChildOf(root),
        ));
    }

    // 8. Wire gliders crossing the sun slowly (`docs/newui.md`, "Sky life").
    let glider_mat = mats.add(StandardMaterial {
        base_color: Color::LinearRgba(LinearRgba::rgb(
            palette.horizon.red * 0.85,
            palette.horizon.green * 0.85,
            palette.horizon.blue * 0.85,
        )),
        unlit: true,
        ..default()
    });
    for (idx, span) in [(0usize, 14.0_f32), (1usize, 7.5_f32)] {
        let glider_mesh = meshes.add(glider_wire_mesh(span, 0.0));
        commands.spawn((
            HeroicGlider { idx, span },
            Mesh3d(glider_mesh),
            MeshMaterial3d(glider_mat.clone()),
            Transform::from_xyz(0.0, 9.0 + idx as f32 * 3.5, -(palette.sizes.sun_distance - 4.0)),
            Visibility::Inherited,
            Name::new(format!("HeroicGlider-{idx}")),
            ChildOf(root),
        ));
    }
}

/// Update environment shader uniforms, fire offsets, hill history, and sky gliders.
pub fn sync_environment(
    time: Res<Time>,
    palette: Res<HeroicPalette>,
    state: Res<HeroicState>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut sun_mats: ResMut<Assets<HeroicSunMaterial>>,
    mut fire_mats: ResMut<Assets<HeroicFireMaterial>>,
    mut hill_mats: ResMut<Assets<HeroicHillsMaterial>>,
    mut sky_q: Query<(&HeroicSky, &Mesh3d)>,
    mut sun_q: Query<(&MeshMaterial3d<HeroicSunMaterial>, &mut Transform), With<HeroicSun>>,
    mut fire_q: Query<(&HeroicFire, &MeshMaterial3d<HeroicFireMaterial>, &mut Transform), Without<HeroicSun>>,
    mut hill_q: Query<(&HeroicHill, &MeshMaterial3d<HeroicHillsMaterial>, &mut Transform), (Without<HeroicSun>, Without<HeroicFire>)>,
    mut glider_q: Query<(&HeroicGlider, &Mesh3d, &mut Transform), (Without<HeroicSun>, Without<HeroicFire>, Without<HeroicHill>)>,
) {
    // Trouble quiets the world by about a third (`docs/newui.md`, "Restraint").
    let trouble_scale = if state.any_alarm() { 0.67 } else { 1.0 };
    let env_intensity = palette.intensity * trouble_scale;
    let reduced = if palette.reduced_motion { 1.0 } else { 0.0 };

    let primary_beat = state
        .lanes
        .first()
        .map(|l| l.playhead_beats)
        .unwrap_or(0.0);
    let primary_period = state
        .lanes
        .first()
        .map(|l| l.period_secs.max(0.05))
        .unwrap_or(0.5);
    let beat_len = palette.units_per_second * primary_period;
    let beat_pulse = (-(primary_beat - primary_beat.floor()) as f32 * 5.0).exp();

    if palette.is_changed() {
        for (_, mesh_handle) in sky_q.iter_mut() {
            if let Some(mut mesh) = meshes.get_mut(&mesh_handle.0) {
                *mesh = sky_gradient_mesh(&palette);
            }
        }
    }

    // Sun update: centered on vanishing point (y = 0, half set behind horizon).
    for (mat_h, mut tf) in sun_q.iter_mut() {
        let pulse_scale = 1.0 + env_intensity * 0.03 + beat_pulse * 0.015;
        tf.translation = Vec3::new(0.0, 0.0, -palette.sizes.sun_distance);
        tf.scale = Vec3::splat(if palette.reduced_motion { 1.0 } else { pulse_scale });
        if let Some(mut mat) = sun_mats.get_mut(&mat_h.0) {
            mat.sun_top = Vec4::new(palette.sun[0].red, palette.sun[0].green, palette.sun[0].blue, 1.0);
            mat.sun_mid = Vec4::new(palette.sun[1].red, palette.sun[1].green, palette.sun[1].blue, 1.0);
            mat.sun_bot = Vec4::new(palette.sun[2].red, palette.sun[2].green, palette.sun[2].blue, 1.0);
            mat.params = Vec4::new(env_intensity, beat_pulse, reduced, 0.0);
        }
    }

    // Pack 16-band energies into 4 vec4s.
    let mut packed_bands = [Vec4::ZERO; 4];
    for i in 0..4 {
        packed_bands[i] = Vec4::new(
            state.band_energies[i * 4],
            state.band_energies[i * 4 + 1],
            state.band_energies[i * 4 + 2],
            state.band_energies[i * 4 + 3],
        );
    }

    let half_road = state.road_width(palette.lane_width) * 0.5;
    let road_len = (palette.road_seconds * palette.units_per_second).max(10.0);
    let scroll_z = (primary_beat as f32) * beat_len;
    let chroma = palette.rainbow_chroma_min
        + (palette.rainbow_chroma_max - palette.rainbow_chroma_min) * env_intensity.clamp(0.0, 1.0);

    for (fire, mat_h, mut tf) in fire_q.iter_mut() {
        let x = fire.side * (half_road + palette.sizes.fire_offset);
        tf.translation.x = x;
        tf.translation.y = palette.sizes.fire_height * 0.5;
        tf.translation.z = -road_len * 0.5;
        if let Some(mut mat) = fire_mats.get_mut(&mat_h.0) {
            mat.params = Vec4::new(beat_len, scroll_z, env_intensity, reduced);
            mat.ramp = Vec4::new(
                palette.rainbow_lightness,
                chroma,
                road_len,
                palette.rainbow_chroma_min,
            );
            mat.fog_color_start = Vec4::new(
                palette.fog_color.red,
                palette.fog_color.green,
                palette.fog_color.blue,
                palette.fog.start,
            );
            mat.fog_end_pad = Vec4::new(palette.fog.end, 0.0, 0.0, 0.0);
            mat.bands = packed_bands;
        }
    }

    // Pack 32x16 hill history.
    let mut history = [Vec4::ZERO; 128];
    for (r, row) in state.hill_history.iter().take(32).enumerate() {
        for b in 0..4 {
            history[r * 4 + b] = Vec4::new(
                row[b * 4],
                row[b * 4 + 1],
                row[b * 4 + 2],
                row[b * 4 + 3],
            );
        }
    }

    for (hill, mat_h, mut tf) in hill_q.iter_mut() {
        let inner = palette.sizes.hill_inner_edge.max(half_road + 1.2);
        tf.translation.x = hill.side * inner;
        if let Some(mut mat) = hill_mats.get_mut(&mat_h.0) {
            mat.params = Vec4::new(env_intensity, state.hill_phase, env_intensity, reduced);
            mat.grid = Vec4::new(palette.rainbow_lightness, chroma * 0.9, 12.0, 32.0);
            mat.fill_fog_start = Vec4::new(
                palette.hill_fill.red,
                palette.hill_fill.green,
                palette.hill_fill.blue,
                palette.fog.start,
            );
            mat.fog_color_end = Vec4::new(
                palette.fog_color.red,
                palette.fog_color.green,
                palette.fog_color.blue,
                palette.fog.end,
            );
            mat.history = history;
        }
    }

    // Gliders: one wingbeat per 4-beat bar, slow crossing over ~2 minutes.
    let t = time.elapsed_secs();
    let bar_phase = if palette.reduced_motion {
        0.0
    } else {
        ((primary_beat * 0.25) as f32 * std::f32::consts::TAU).sin()
    };
    for (glider, mesh_h, mut tf) in glider_q.iter_mut() {
        let period = 110.0 + glider.idx as f32 * 45.0;
        let x_progress = if palette.reduced_motion {
            0.35 + glider.idx as f32 * 0.25
        } else {
            ((t / period) + glider.idx as f32 * 0.37).fract()
        };
        let x = (x_progress - 0.5) * 70.0;
        let y = 9.5 + glider.idx as f32 * 3.8 + bar_phase * 0.35;
        tf.translation = Vec3::new(x, y, -(palette.sizes.sun_distance - 3.5));
        if let Some(mut mesh) = meshes.get_mut(&mesh_h.0) {
            *mesh = glider_wire_mesh(glider.span, bar_phase);
        }
    }
}

fn sky_gradient_mesh(palette: &HeroicPalette) -> Mesh {
    let w = 180.0_f32;
    let h_bot = -6.0_f32;
    let h_low = 12.0_f32; // horizon glow band covering lower striped sun + hilltops
    let h_mid = 34.0_f32;
    let h_top = 86.0_f32;

    let rows = [
        (h_bot, palette.horizon),
        (h_low, palette.sky_low),
        (h_mid, palette.sky_mid),
        (h_top, palette.sky_top),
    ];

    let mut positions = Vec::with_capacity(8);
    let mut normals = Vec::with_capacity(8);
    let mut colors = Vec::with_capacity(8);
    for &(y, col) in &rows {
        let rgba = [col.red, col.green, col.blue, 1.0];
        positions.push([-w, y, 0.0]);
        positions.push([w, y, 0.0]);
        normals.push([0.0, 0.0, 1.0]);
        normals.push([0.0, 0.0, 1.0]);
        colors.push(rgba);
        colors.push(rgba);
    }

    let mut indices = Vec::with_capacity(18);
    for r in 0..3u32 {
        let a = r * 2;
        indices.extend_from_slice(&[a, a + 1, a + 2, a + 1, a + 3, a + 2]);
    }

    Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default())
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
        .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, colors)
        .with_inserted_indices(Indices::U32(indices))
}

fn roadside_grid_mesh(palette: &HeroicPalette) -> Mesh {
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut colors: Vec<[f32; 4]> = Vec::new();

    let max_x = 56.0_f32;
    let max_z = 64.0_f32;
    let step = 2.0_f32;

    let fog_col = palette.fog_color;
    let grid_col = palette.horizon;
    let color_at_z = |z: f32| -> [f32; 4] {
        let dist = -z;
        let f = ((dist - palette.fog.start) / (palette.fog.end - palette.fog.start).max(0.01))
            .clamp(0.0, 1.0);
        let r = grid_col.red * (1.0 - f) + fog_col.red * f;
        let g = grid_col.green * (1.0 - f) + fog_col.green * f;
        let b = grid_col.blue * (1.0 - f) + fog_col.blue * f;
        // Keep below 1.0 so the grid doesn't bloom over the road.
        [r * 0.55, g * 0.55, b * 0.55, 1.0]
    };

    // Lines parallel to Z.
    let mut x = -max_x;
    while x <= max_x + 0.01 {
        let mut z = 0.0_f32;
        while z > -max_z {
            let z_next = (z - step * 2.0).max(-max_z);
            positions.push([x, 0.0, z]);
            positions.push([x, 0.0, z_next]);
            colors.push(color_at_z(z));
            colors.push(color_at_z(z_next));
            z = z_next;
        }
        x += step;
    }

    // Lines parallel to X.
    let mut z = 0.0_f32;
    while z >= -max_z - 0.01 {
        let c = color_at_z(z);
        positions.push([-max_x, 0.0, z]);
        positions.push([max_x, 0.0, z]);
        colors.push(c);
        colors.push(c);
        z -= step;
    }

    let indices: Vec<u32> = (0..positions.len() as u32).collect();
    Mesh::new(PrimitiveTopology::LineList, RenderAssetUsages::default())
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
        .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, colors)
        .with_inserted_indices(Indices::U32(indices))
}

fn hill_grid_mesh(side: f32, width: f32, length: f32) -> Mesh {
    let cols = 12usize;
    let rows = 32usize;
    let mut positions = Vec::with_capacity((cols + 1) * (rows + 1));
    let mut normals = Vec::with_capacity((cols + 1) * (rows + 1));
    let mut uvs = Vec::with_capacity((cols + 1) * (rows + 1));

    for r in 0..=rows {
        let v = r as f32 / rows as f32;
        let z = -v * length;
        for c in 0..=cols {
            let d = c as f32 / cols as f32;
            let x = side * d * width;
            positions.push([x, 0.0, z]);
            normals.push([0.0, 1.0, 0.0]);
            uvs.push([d, v]);
        }
    }

    let mut indices = Vec::with_capacity(cols * rows * 6);
    let stride = (cols + 1) as u32;
    for r in 0..rows as u32 {
        for c in 0..cols as u32 {
            let a = r * stride + c;
            let b = a + 1;
            let c_idx = a + stride;
            let d_idx = c_idx + 1;
            if side > 0.0 {
                indices.extend_from_slice(&[a, b, c_idx, b, d_idx, c_idx]);
            } else {
                indices.extend_from_slice(&[a, c_idx, b, b, c_idx, d_idx]);
            }
        }
    }

    Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default())
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
        .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, uvs)
        .with_inserted_indices(Indices::U32(indices))
}

fn glider_wire_mesh(span: f32, wing_phase: f32) -> Mesh {
    let half = span * 0.5;
    let tip_y = wing_phase * span * 0.08;
    let nose = [0.0, 0.0, -span * 0.28];
    let tail = [0.0, 0.0, span * 0.14];
    let left_mid = [-half * 0.45, tip_y * 0.4, 0.0];
    let right_mid = [half * 0.45, tip_y * 0.4, 0.0];
    let left_tip = [-half, tip_y, span * 0.10];
    let right_tip = [half, tip_y, span * 0.10];

    let positions = vec![
        nose, left_mid,
        left_mid, left_tip,
        left_tip, tail,
        tail, right_tip,
        right_tip, right_mid,
        right_mid, nose,
        nose, tail,
    ];
    let indices: Vec<u32> = (0..positions.len() as u32).collect();
    Mesh::new(PrimitiveTopology::LineList, RenderAssetUsages::default())
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
        .with_inserted_indices(Indices::U32(indices))
}

#[cfg(test)]
mod tests {
    use bevy::mesh::VertexAttributeValues;

    use super::*;

    #[test]
    fn hill_inner_edge_stays_pinned_at_y_zero() {
        let mesh = hill_grid_mesh(1.0, 26.0, 64.0);
        let Some(VertexAttributeValues::Float32x3(pos)) = mesh.attribute(Mesh::ATTRIBUTE_POSITION)
        else {
            panic!("positions");
        };
        let Some(VertexAttributeValues::Float32x2(uvs)) = mesh.attribute(Mesh::ATTRIBUTE_UV_0)
        else {
            panic!("uvs");
        };
        for (p, uv) in pos.iter().zip(uvs.iter()) {
            if uv[0] == 0.0 {
                assert_eq!(p[0], 0.0);
                assert_eq!(p[1], 0.0);
            }
        }
    }
}
