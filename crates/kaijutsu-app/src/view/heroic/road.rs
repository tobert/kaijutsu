//! Road geometry for the heroic view (`docs/newui.md`, "On the road"):
//! - dark glass lane floors
//! - thin cyan rails
//! - per-track beat & phrase ruler lines scrolling in wall-time distance
//! - white strike line (`now`) and commit lock line
//! - round rings and falling white instruction markers

use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;

use super::state::HeroicState;
use crate::view::scene_palette::{HeroicPalette, heroic_identity_color};

/// Dynamic road mesh entity rebuilt cleanly each frame from `HeroicState` +
/// `HeroicPalette` (single batched mesh per layer so draw calls stay tiny).
#[derive(Component)]
pub struct HeroicRoadGlass;

#[derive(Component)]
pub struct HeroicRoadLines;

#[derive(Component)]
pub struct HeroicStrikeLine;

#[derive(Component)]
pub struct HeroicLockLine;

#[derive(Component)]
pub struct HeroicRoundRing;

#[derive(Component)]
pub struct HeroicInstructionMarkers;

/// Spawn road structural entities under `root`.
pub fn spawn_road(
    commands: &mut Commands,
    root: Entity,
    palette: &HeroicPalette,
    meshes: &mut Assets<Mesh>,
    mats: &mut Assets<StandardMaterial>,
) {
    let empty_tri = || {
        Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default())
            .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, Vec::<[f32; 3]>::new())
            .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, Vec::<[f32; 3]>::new())
            .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, Vec::<[f32; 4]>::new())
            .with_inserted_indices(Indices::U32(Vec::new()))
    };
    let empty_lines = || {
        Mesh::new(PrimitiveTopology::LineList, RenderAssetUsages::default())
            .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, Vec::<[f32; 3]>::new())
            .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, Vec::<[f32; 4]>::new())
            .with_inserted_indices(Indices::U32(Vec::new()))
    };

    let unlit_vertex_mat = mats.add(StandardMaterial {
        base_color: Color::WHITE,
        unlit: true,
        ..default()
    });

    let strike_mat = mats.add(StandardMaterial {
        base_color: Color::LinearRgba(LinearRgba::rgb(
            palette.now.red * 1.45,
            palette.now.green * 1.45,
            palette.now.blue * 1.45,
        )),
        unlit: true,
        ..default()
    });

    let lock_mat = mats.add(StandardMaterial {
        base_color: Color::LinearRgba(LinearRgba::rgb(
            palette.rail.red * 0.75,
            palette.rail.green * 0.75,
            palette.rail.blue * 0.75,
        )),
        unlit: true,
        ..default()
    });

    commands.spawn((
        HeroicRoadGlass,
        Mesh3d(meshes.add(empty_tri())),
        MeshMaterial3d(unlit_vertex_mat.clone()),
        Transform::default(),
        Visibility::Inherited,
        Name::new("HeroicRoadGlass"),
        ChildOf(root),
    ));

    commands.spawn((
        HeroicRoadLines,
        Mesh3d(meshes.add(empty_lines())),
        MeshMaterial3d(unlit_vertex_mat.clone()),
        Transform::default(),
        Visibility::Inherited,
        Name::new("HeroicRoadLines"),
        ChildOf(root),
    ));

    commands.spawn((
        HeroicStrikeLine,
        Mesh3d(meshes.add(Cuboid::new(4.0, 0.025, 0.05))),
        MeshMaterial3d(strike_mat),
        Transform::from_xyz(0.0, 0.015, 0.0),
        Visibility::Inherited,
        Name::new("HeroicStrikeLine"),
        ChildOf(root),
    ));

    commands.spawn((
        HeroicLockLine,
        Mesh3d(meshes.add(Cuboid::new(4.0, 0.015, 0.025))),
        MeshMaterial3d(lock_mat),
        Transform::from_xyz(0.0, 0.012, -0.9),
        Visibility::Inherited,
        Name::new("HeroicLockLine"),
        ChildOf(root),
    ));

    commands.spawn((
        HeroicRoundRing,
        Mesh3d(meshes.add(empty_tri())),
        MeshMaterial3d(unlit_vertex_mat.clone()),
        Transform::default(),
        Visibility::Inherited,
        Name::new("HeroicRoundRing"),
        ChildOf(root),
    ));

    commands.spawn((
        HeroicInstructionMarkers,
        Mesh3d(meshes.add(empty_tri())),
        MeshMaterial3d(unlit_vertex_mat),
        Transform::default(),
        Visibility::Inherited,
        Name::new("HeroicInstructionMarkers"),
        ChildOf(root),
    ));
}

fn fog_mix(
    c: LinearRgba,
    world_pos: Vec3,
    cam_pos: Vec3,
    palette: &HeroicPalette,
    dim: f32,
) -> [f32; 4] {
    let dist = cam_pos.distance(world_pos);
    let f = ((dist - palette.fog.start) / (palette.fog.end - palette.fog.start).max(0.01))
        .clamp(0.0, 1.0);
    let fog = palette.fog_color;
    [
        (c.red * dim) * (1.0 - f) + fog.red * f,
        (c.green * dim) * (1.0 - f) + fog.green * f,
        (c.blue * dim) * (1.0 - f) + fog.blue * f,
        1.0,
    ]
}

/// Update road glass, rails, beat/phrase rulers, strike/lock lines, round ring,
/// and falling instruction markers.
pub fn sync_road(
    time: Res<Time>,
    palette: Res<HeroicPalette>,
    state: Res<HeroicState>,
    mut meshes: ResMut<Assets<Mesh>>,
    cam_q: Query<&Transform, With<super::HeroicCamera>>,
    glass_q: Query<&Mesh3d, With<HeroicRoadGlass>>,
    lines_q: Query<&Mesh3d, With<HeroicRoadLines>>,
    mut strike_q: Query<&mut Transform, (With<HeroicStrikeLine>, Without<super::HeroicCamera>)>,
    mut lock_q: Query<&mut Transform, (With<HeroicLockLine>, Without<HeroicStrikeLine>, Without<super::HeroicCamera>)>,
    ring_q: Query<&Mesh3d, With<HeroicRoundRing>>,
    instr_q: Query<&Mesh3d, With<HeroicInstructionMarkers>>,
) {
    let cam_pos = cam_q
        .single()
        .map(|tf| tf.translation)
        .unwrap_or(Vec3::new(0.0, 2.25, 3.33));
    let lane_w = palette.lane_width;
    let road_w = state.road_width(lane_w);
    let half_w = road_w * 0.5;
    let road_len = (palette.road_seconds * palette.units_per_second).max(10.0);
    let layout: Vec<_> = state.road_lanes().collect();
    let n_lanes = layout.len().max(1);

    // 1. Strike and lock lines.
    if let Ok(mut tf) = strike_q.single_mut() {
        tf.scale.x = (road_w / 4.0).max(0.25);
    }
    if let Ok(mut tf) = lock_q.single_mut() {
        let avg_period = state
            .lanes
            .first()
            .map(|l| l.period_secs * l.commit_margin_ticks.max(1) as f32)
            .unwrap_or(0.5);
        tf.translation.z = -avg_period * palette.units_per_second;
        tf.scale.x = (road_w / 4.0).max(0.25);
    }

    let lane_dim = |performer: Option<&super::state::HeroicPerformer>| {
        match state.selected_performer {
            Some(selected) if performer.is_none_or(|p| p.context_id != selected) => {
                palette.unselected_opacity
            }
            _ => 1.0,
        }
    };

    // 2. Road glass strips (subdivided along Z for smooth vertex fog).
    if let Ok(mesh_h) = glass_q.single()
        && let Some(mut mesh) = meshes.get_mut(&mesh_h.0)
    {
        let segs = 16usize;
        let mut pos = Vec::new();
        let mut norm = Vec::new();
        let mut col = Vec::new();
        let mut idx = Vec::new();

        for li in 0..n_lanes {
            let cx = state.lane_center_x(li, lane_w);
            let performer = layout.get(li).and_then(|(_, performer, _)| *performer);
            let dim = lane_dim(performer);
            let base = pos.len() as u32;
            for s in 0..=segs {
                let z = -(s as f32 / segs as f32) * road_len;
                let floor = performer
                    .map(|p| {
                        let tint = heroic_identity_color(p.lane_name());
                        LinearRgba::rgb(
                            palette.ground.red + tint.red * 0.12,
                            palette.ground.green + tint.green * 0.12,
                            palette.ground.blue + tint.blue * 0.12,
                        )
                    })
                    .unwrap_or(palette.ground);
                let c = fog_mix(floor, Vec3::new(cx, 0.0, z), cam_pos, &palette, dim);
                pos.push([cx - lane_w * 0.48, 0.0, z]);
                pos.push([cx + lane_w * 0.48, 0.0, z]);
                norm.push([0.0, 1.0, 0.0]);
                norm.push([0.0, 1.0, 0.0]);
                col.push(c);
                col.push(c);
            }
            for s in 0..segs as u32 {
                let a = base + s * 2;
                idx.extend_from_slice(&[a, a + 1, a + 2, a + 1, a + 3, a + 2]);
            }
        }

        *mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default())
            .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, pos)
            .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, norm)
            .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, col)
            .with_inserted_indices(Indices::U32(idx));
    }

    // 3. Rails, beat lines, and phrase lines (continuing onto roadside grid in magenta).
    if let Ok(mesh_h) = lines_q.single()
        && let Some(mut mesh) = meshes.get_mut(&mesh_h.0)
    {
        let mut pos = Vec::new();
        let mut col = Vec::new();

        // Rails along each lane edge, subdivided for fog.
        let rail_segs = 16usize;
        for r_i in 0..=n_lanes {
            let x = -half_w + (r_i as f32) * lane_w;
            let track_edge = r_i == n_lanes || layout.get(r_i).is_none_or(|(_, _, first)| *first);
            let rail_gain = if track_edge { 0.95 } else { 0.46 };
            for s in 0..rail_segs {
                let z0 = -(s as f32 / rail_segs as f32) * road_len;
                let z1 = -((s + 1) as f32 / rail_segs as f32) * road_len;
                pos.push([x, 0.005, z0]);
                pos.push([x, 0.005, z1]);
                col.push(fog_mix(palette.rail, Vec3::new(x, 0.005, z0), cam_pos, &palette, rail_gain));
                col.push(fog_mix(palette.rail, Vec3::new(x, 0.005, z1), cam_pos, &palette, rail_gain));
            }
        }

        // Per-lane beat and phrase lines in wall time (`TrackInfo.periodUs`).
        for li in 0..n_lanes {
            let cx = state.lane_center_x(li, lane_w);
            let performer = layout.get(li).and_then(|(_, performer, _)| *performer);
            let dim = lane_dim(performer);
            let (playhead, period, bpp) = layout
                .get(li)
                .map(|(l, _, _)| (l.playhead_beats, l.period_secs.max(0.05), l.beats_per_phrase.max(1)))
                .unwrap_or((0.0, 0.5, 8));

            let max_beats = (palette.road_seconds / period).ceil() as i64;
            let first_beat = playhead.ceil() as i64;
            for b in first_beat..=(first_beat + max_beats) {
                let dt_secs = ((b as f64 - playhead) as f32) * period;
                let z = -dt_secs * palette.units_per_second;
                if z > 0.0 || z < -road_len {
                    continue;
                }
                let is_phrase = b.rem_euclid(bpp as i64) == 0;
                let line_col = if is_phrase {
                    fog_mix(palette.rail, Vec3::new(cx, 0.006, z), cam_pos, &palette, dim * 0.95)
                } else {
                    fog_mix(palette.beat_line, Vec3::new(cx, 0.006, z), cam_pos, &palette, dim * 0.85)
                };
                pos.push([cx - lane_w * 0.48, 0.006, z]);
                pos.push([cx + lane_w * 0.48, 0.006, z]);
                col.push(line_col);
                col.push(line_col);

                // Phrase lines continue onto the roadside grid in magenta.
                if is_phrase && (li == 0 || li + 1 == n_lanes) {
                    let mag = fog_mix(palette.horizon, Vec3::new(cx, 0.004, z), cam_pos, &palette, 0.65);
                    if li == 0 {
                        pos.push([-half_w - 28.0, 0.004, z]);
                        pos.push([-half_w, 0.004, z]);
                        col.push(mag);
                        col.push(mag);
                    }
                    if li + 1 == n_lanes {
                        pos.push([half_w, 0.004, z]);
                        pos.push([half_w + 28.0, 0.004, z]);
                        col.push(mag);
                        col.push(mag);
                    }
                }
            }
        }

        let idx: Vec<u32> = (0..pos.len() as u32).collect();
        *mesh = Mesh::new(PrimitiveTopology::LineList, RenderAssetUsages::default())
            .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, pos)
            .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, col)
            .with_inserted_indices(Indices::U32(idx));
    }

    // 4. Round ring approaching the viewer at the end of each round (`docs/newui.md`, "Ring").
    if let Ok(mesh_h) = ring_q.single()
        && let Some(mut mesh) = meshes.get_mut(&mesh_h.0)
    {
        let primary_beat = state.lanes.first().map(|l| l.playhead_beats).unwrap_or(0.0);
        let primary_period = state
            .lanes
            .first()
            .map(|l| l.period_secs.max(0.05))
            .unwrap_or(0.5);
        let round_beats = state.round_beats.max(16.0);
        let next_round_beat = (primary_beat / round_beats).ceil() * round_beats;
        let z = -(((next_round_beat - primary_beat) as f32) * primary_period * palette.units_per_second);

        let mut pos = Vec::new();
        let mut norm = Vec::new();
        let mut col = Vec::new();
        let mut idx = Vec::new();

        if z <= 0.5 && z >= -road_len {
            let r_outer = palette.sizes.ring_radius.max(road_w * 0.75);
            let r_inner = r_outer - 0.14;
            let segs = 48usize;
            let shares: Vec<(String, f32)> = if state.round_shares.is_empty() {
                vec![("default".into(), 1.0)]
            } else {
                state.round_shares.clone()
            };
            let total_share: f32 = shares.iter().map(|s| s.1).sum::<f32>().max(0.001);

            let mut acc_angle = 0.0_f32;
            for (name, share) in shares {
                let sweep = std::f32::consts::PI * (share / total_share);
                let arc_segs = ((segs as f32 * (share / total_share)).round() as usize).max(3);
                let c_lin = heroic_identity_color(&name);
                let c = fog_mix(c_lin, Vec3::new(0.0, 0.0, z), cam_pos, &palette, 1.15);
                let base = pos.len() as u32;
                for i in 0..=arc_segs {
                    let a = acc_angle + sweep * (i as f32 / arc_segs as f32);
                    let (sin_a, cos_a) = a.sin_cos();
                    pos.push([cos_a * r_inner, sin_a * r_inner, z]);
                    pos.push([cos_a * r_outer, sin_a * r_outer, z]);
                    norm.push([0.0, 0.0, 1.0]);
                    norm.push([0.0, 0.0, 1.0]);
                    col.push(c);
                    col.push(c);
                }
                for i in 0..arc_segs as u32 {
                    let v = base + i * 2;
                    idx.extend_from_slice(&[v, v + 1, v + 2, v + 1, v + 3, v + 2]);
                }
                acc_angle += sweep;
            }
        }

        *mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default())
            .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, pos)
            .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, norm)
            .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, col)
            .with_inserted_indices(Indices::U32(idx));
    }

    // 5. Falling white instruction markers across lanes.
    if let Ok(mesh_h) = instr_q.single()
        && let Some(mut mesh) = meshes.get_mut(&mesh_h.0)
    {
        let mut pos = Vec::new();
        let mut norm = Vec::new();
        let mut col = Vec::new();
        let mut idx = Vec::new();
        let now_secs = time.elapsed_secs_f64();
        let fall_secs = (palette.durations_ms.instruction_fall as f64 / 1000.0).max(0.05);

        for (li, (lane, performer, _)) in layout.iter().copied().enumerate() {
            let Some(perf) = performer else { continue };
            let cx = state.lane_center_x(li, lane_w);
            let dim = lane_dim(performer);
            for instr in &perf.instructions {
                let dt = (instr.target_beat - lane.playhead_beats) as f32 * lane.period_secs;
                let z = -dt * palette.units_per_second;
                if z > 0.5 || z < -road_len {
                    continue;
                }
                let fall_t = ((now_secs - instr.spawned_at_secs) / fall_secs).clamp(0.0, 1.0) as f32;
                let eased = 1.0 - (1.0 - fall_t).powi(3);
                let y = (1.0 - eased) * 4.5 + 0.06;
                let brightness = if instr.heard { 0.45 } else { 1.35 } * dim;
                let c = fog_mix(palette.now, Vec3::new(cx, y, z), cam_pos, &palette, brightness);
                let hw = lane_w * 0.42;
                let hz = 0.04;
                let base = pos.len() as u32;
                pos.push([cx - hw, y, z + hz]);
                pos.push([cx + hw, y, z + hz]);
                pos.push([cx + hw, y, z - hz]);
                pos.push([cx - hw, y, z - hz]);
                for _ in 0..4 {
                    norm.push([0.0, 1.0, 0.0]);
                    col.push(c);
                }
                idx.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
            }
        }

        *mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default())
            .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, pos)
            .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, norm)
            .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, col)
            .with_inserted_indices(Indices::U32(idx));
    }
}
