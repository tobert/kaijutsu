//! Performer crystal geometry and live motion (`docs/newui.md`, "A crystal per performer").
//!
//! Each attached performer gets a deterministic convex faceted bipyramid
//! generated from `(character, context_id)` — blending 40% parent parameters
//! when forked/spawned from a parent context — plus a spinning wireframe cage
//! during `RunningRetry`, parabolic phrase hops, a dim phrase slab on the
//! road floor, and a slack floor strip that turns trouble-red on alarm.

use std::collections::{HashMap, HashSet};
use std::f32::consts::TAU;

use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use kaijutsu_hyoushigi::{Disposition, FallbackReason, Readiness};
use kaijutsu_present::work::SlackState;
use kaijutsu_types::ContextId;

use super::state::{HeroicPerformer, HeroicState};
use crate::view::room::bearing::hash01;
use crate::view::scene_palette::{HeroicPalette, heroic_identity_color};

/// Deterministic geometric parameters for a performer crystal.
#[derive(Debug, Clone, PartialEq)]
pub struct CrystalParams {
    pub top_height: f32,
    pub bottom_height: f32,
    pub sides: usize,
    pub radii: Vec<f32>,
    pub twist: f32,
    pub bob_amp: f32,
    pub bob_freq: f32,
    pub tilt_phase: f32,
}

fn fnv1a_seed(key: &str, salt: u32) -> u32 {
    let mut h: u32 = 2166136261 ^ salt;
    for b in key.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    h
}

fn base_crystal_params(seed: &str) -> CrystalParams {
    let h0 = hash01(fnv1a_seed(seed, 1));
    let h1 = hash01(fnv1a_seed(seed, 2));
    let h2 = hash01(fnv1a_seed(seed, 3));
    let h3 = hash01(fnv1a_seed(seed, 4));
    let h4 = hash01(fnv1a_seed(seed, 5));
    let h5 = hash01(fnv1a_seed(seed, 6));
    let h6 = hash01(fnv1a_seed(seed, 7));

    let sides = 5 + ((h0 * 3.0).floor() as usize).min(2); // 5..=7
    let mut radii = Vec::with_capacity(sides);
    for i in 0..sides {
        let hi = hash01(fnv1a_seed(seed, 100 + i as u32));
        // Tight band keeps any polygon >= 5 sides strictly convex.
        radii.push(0.25 + hi * 0.08);
    }

    CrystalParams {
        top_height: 0.44 + h1 * 0.24,
        bottom_height: 0.22 + h2 * 0.14,
        sides,
        radii,
        twist: (h3 - 0.5) * 0.36,
        bob_amp: 0.025 + h4 * 0.03,
        bob_freq: 1.1 + h5 * 0.9,
        tilt_phase: h6 * TAU,
    }
}

/// Enforce strict radial convexity in XZ so no waist vertex indents inward.
fn enforce_convex_radii(radii: &mut [f32]) {
    let n = radii.len();
    if n < 5 {
        return;
    }
    let sector = TAU / n as f32;
    let cos_s = sector.cos().max(0.1);
    for _ in 0..4 {
        for i in 0..n {
            let prev = radii[(i + n - 1) % n];
            let next = radii[(i + 1) % n];
            // A symmetric triangle condition r[i] > (prev * next) * ...; raising
            // sunken vertices toward the chord radius guarantees convexity.
            let min_convex = (prev.min(next) * 0.98) / cos_s.max(0.5);
            let safe_floor = ((prev + next) * 0.5) * 0.96;
            radii[i] = radii[i].max(safe_floor.min(min_convex)).clamp(0.22, 0.36);
        }
    }
}

/// Generate deterministic crystal geometry parameters from `seed`, blending
/// 40% of `parent_key`'s parameters when spawned from a parent context.
pub fn crystal_params(seed: &str, parent_key: Option<&str>) -> CrystalParams {
    let mut child = base_crystal_params(seed);
    if let Some(pk) = parent_key {
        let parent = base_crystal_params(pk);
        let blended_sides = ((0.4 * parent.sides as f32 + 0.6 * child.sides as f32).round()
            as usize)
            .clamp(5, 7);
        let mut radii = Vec::with_capacity(blended_sides);
        for i in 0..blended_sides {
            let p_r = parent.radii[i % parent.radii.len()];
            let c_r = child.radii[i % child.radii.len()];
            radii.push(0.4 * p_r + 0.6 * c_r);
        }
        enforce_convex_radii(&mut radii);
        child = CrystalParams {
            top_height: 0.4 * parent.top_height + 0.6 * child.top_height,
            bottom_height: 0.4 * parent.bottom_height + 0.6 * child.bottom_height,
            sides: blended_sides,
            radii,
            twist: 0.4 * parent.twist + 0.6 * child.twist,
            bob_amp: 0.4 * parent.bob_amp + 0.6 * child.bob_amp,
            bob_freq: 0.4 * parent.bob_freq + 0.6 * child.bob_freq,
            tilt_phase: child.tilt_phase,
        };
    } else {
        enforce_convex_radii(&mut child.radii);
    }
    child
}

fn push_faceted_tri(
    pos: &mut Vec<[f32; 3]>,
    norm: &mut Vec<[f32; 3]>,
    col: &mut Vec<[f32; 4]>,
    idx: &mut Vec<u32>,
    a: Vec3,
    b: Vec3,
    c: Vec3,
    light_dir: Vec3,
) {
    let n = (b - a).cross(c - a).normalize_or(Vec3::Y);
    let shade = 0.60 + 0.50 * n.dot(light_dir).max(0.0);
    let base = pos.len() as u32;
    for p in [a, b, c] {
        pos.push(p.to_array());
        norm.push(n.to_array());
        col.push([shade, shade, shade, 1.0]);
    }
    idx.extend_from_slice(&[base, base + 1, base + 2]);
}

/// Build `(solid_bipyramid, wireframe_cage)` meshes for a performer crystal.
///
/// The solid mesh uses outward-winding flat-shaded triangles with facet lighting
/// baked into vertex colors so crystals read with sharp planes without scene lights.
pub fn build_crystal_meshes(params: &CrystalParams) -> (Mesh, Mesh) {
    let n = params.sides.max(5);
    let top = Vec3::new(0.0, params.top_height, 0.0);
    let bot = Vec3::new(0.0, -params.bottom_height, 0.0);

    let mut ring = Vec::with_capacity(n);
    for i in 0..n {
        let a = (i as f32 / n as f32) * TAU + params.twist;
        let r = params.radii[i % params.radii.len()];
        ring.push(Vec3::new(r * a.cos(), 0.0, r * a.sin()));
    }

    let light_dir = Vec3::new(0.42, 0.84, 0.34).normalize();
    let mut pos = Vec::new();
    let mut norm = Vec::new();
    let mut col = Vec::new();
    let mut idx = Vec::new();

    for i in 0..n {
        let next = (i + 1) % n;
        // Angle increases clockwise around +Y in XZ, so (top, ring[next], ring[i])
        // has outward-pointing normals on top and (bot, ring[i], ring[next]) on bottom.
        push_faceted_tri(&mut pos, &mut norm, &mut col, &mut idx, top, ring[next], ring[i], light_dir);
        push_faceted_tri(&mut pos, &mut norm, &mut col, &mut idx, bot, ring[i], ring[next], light_dir);
    }

    let solid = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default())
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, pos)
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, norm)
        .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, col)
        .with_inserted_indices(Indices::U32(idx));

    // Wireframe cage at 1.0x radius; runtime scales via `[heroic.sizes].cage_scale`.
    let c_top = top;
    let c_bot = bot;
    let mut wire_pos = Vec::new();
    let mut wire_col = Vec::new();
    let mut wire_idx = Vec::new();
    let wc = [1.0, 1.0, 1.0, 1.0];
    let mut push_edge = |a: Vec3, b: Vec3| {
        let base = wire_pos.len() as u32;
        wire_pos.push(a.to_array());
        wire_pos.push(b.to_array());
        wire_col.push(wc);
        wire_col.push(wc);
        wire_idx.push(base);
        wire_idx.push(base + 1);
    };
    for i in 0..n {
        let next = (i + 1) % n;
        let ri = ring[i];
        let rn = ring[next];
        push_edge(c_top, ri);
        push_edge(c_bot, ri);
        push_edge(ri, rn);
    }

    let cage = Mesh::new(PrimitiveTopology::LineList, RenderAssetUsages::default())
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, wire_pos)
        .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, wire_col)
        .with_inserted_indices(Indices::U32(wire_idx));

    (solid, cage)
}

#[derive(Component)]
pub struct HeroicPerformersRoot;

#[derive(Component)]
#[allow(dead_code)]
pub struct HeroicPerformerCrystal {
    pub context_id: ContextId,
}

#[derive(Component)]
#[allow(dead_code)]
pub struct HeroicPerformerCage {
    pub context_id: ContextId,
}

#[derive(Component)]
pub struct HeroicPerformerRetryCage;

#[derive(Component)]
pub struct HeroicPhraseSlab;

#[derive(Component)]
pub struct HeroicSlackFloor;

#[derive(Debug, Clone)]
pub struct PerformerVisualSlot {
    pub params: CrystalParams,
    pub crystal_entity: Entity,
    pub cage_entity: Entity,
    pub retry_cage_entity: Entity,
    pub slab_entity: Entity,
    pub slack_entity: Entity,
    pub crystal_mat: Handle<StandardMaterial>,
    pub cage_mat: Handle<StandardMaterial>,
    pub retry_cage_mat: Handle<StandardMaterial>,
    pub slab_mat: Handle<StandardMaterial>,
    pub slack_mat: Handle<StandardMaterial>,
    pub world_pos: Vec3,
    pub smooth_z: f32,
}

/// Pool of live performer entities keyed by `ContextId`, plus cached last positions
/// for tether lines, text ribbons, and candidate bars.
#[derive(Resource, Default)]
pub struct HeroicPerformerPool {
    pub root: Option<Entity>,
    pub slots: HashMap<ContextId, PerformerVisualSlot>,
}

/// Spawn parent container for performer crystals under `root`.
pub fn spawn_performers_root(commands: &mut Commands, root: Entity) -> Entity {
    commands
        .spawn((
            HeroicPerformersRoot,
            Transform::default(),
            Visibility::Inherited,
            Name::new("HeroicPerformersRoot"),
            ChildOf(root),
        ))
        .id()
}

/// Compute world-space crystal position `(x, y, z)` including phrase hop arc.
pub fn performer_world_pos(
    state: &HeroicState,
    lane_idx: usize,
    perf_idx: usize,
    perf: &HeroicPerformer,
    palette: &HeroicPalette,
    params: &CrystalParams,
    now_secs: f32,
) -> Vec3 {
    let lane = &state.lanes[lane_idx];
    let lane_x = state.performer_lane_center_x(lane_idx, perf_idx, palette.lane_width);
    let road_len = (palette.road_seconds * palette.units_per_second).max(10.0);
    let target_beats = perf
        .statuses
        .iter()
        .filter(|w| w.disposition.is_none())
        .map(|w| (w.start.0 as f64 - lane.playhead_beats) as f32)
        .filter(|b| *b > 0.0)
        .fold(f32::MIN, f32::max);
    let ahead_beats = if target_beats > 0.0 {
        target_beats
    } else {
        perf.lead_beats.max(4.0)
    };
    let lead_z = (-ahead_beats * lane.period_secs * palette.units_per_second)
        .clamp(-road_len * 0.82, -1.4);

    let hop_y = if let (Some(_from), Some(_to)) = (perf.hop_from_phrase, perf.hop_to_phrase) {
        let hop_dur = (palette.durations_ms.hop as f32 / 1000.0).max(0.05);
        let u = (perf.hop_elapsed / hop_dur).clamp(0.0, 1.0);
        4.0 * u * (1.0 - u) * palette.sizes.hop_arc
    } else {
        0.0
    };

    let bob = if palette.reduced_motion {
        0.0
    } else {
        (now_secs * params.bob_freq + params.tilt_phase).sin() * params.bob_amp
    };
    let base_y = palette.sizes.hover_height.max(0.18);
    Vec3::new(lane_x, base_y + bob + hop_y, lead_z)
}

fn spawn_performer_slot(
    commands: &mut Commands,
    parent: Entity,
    perf: &HeroicPerformer,
    parent_key: Option<&str>,
    meshes: &mut Assets<Mesh>,
    mats: &mut Assets<StandardMaterial>,
) -> PerformerVisualSlot {
    let seed = format!("{}:{}", perf.character, perf.context_id);
    let params = crystal_params(&seed, parent_key);
    let (solid_mesh, cage_mesh) = build_crystal_meshes(&params);
    let solid_h = meshes.add(solid_mesh);
    let cage_h = meshes.add(cage_mesh);

    let crystal_mat = mats.add(StandardMaterial {
        base_color: Color::WHITE,
        unlit: true,
        ..default()
    });
    let cage_mat = mats.add(StandardMaterial {
        base_color: Color::WHITE,
        unlit: true,
        ..default()
    });
    let retry_cage_mat = mats.add(StandardMaterial {
        base_color: Color::WHITE,
        unlit: true,
        ..default()
    });
    let slab_mat = mats.add(StandardMaterial {
        base_color: Color::WHITE,
        unlit: true,
        alpha_mode: AlphaMode::Blend,
        ..default()
    });
    let slack_mat = mats.add(StandardMaterial {
        base_color: Color::WHITE,
        unlit: true,
        alpha_mode: AlphaMode::Blend,
        ..default()
    });

    let crystal_entity = commands
        .spawn((
            HeroicPerformerCrystal {
                context_id: perf.context_id,
            },
            Mesh3d(solid_h),
            MeshMaterial3d(crystal_mat.clone()),
            Transform::default(),
            Visibility::Inherited,
            Name::new(format!("Crystal:{}", perf.display_name())),
            ChildOf(parent),
        ))
        .id();

    let cage_entity = commands
        .spawn((
            HeroicPerformerCage {
                context_id: perf.context_id,
            },
            Mesh3d(cage_h.clone()),
            MeshMaterial3d(cage_mat.clone()),
            Transform::default(),
            Visibility::Inherited,
            Name::new(format!("Cage:{}", perf.display_name())),
            ChildOf(crystal_entity),
        ))
        .id();

    let retry_cage_entity = commands
        .spawn((
            HeroicPerformerRetryCage,
            Mesh3d(cage_h),
            MeshMaterial3d(retry_cage_mat.clone()),
            Transform::from_scale(Vec3::splat(1.16)),
            Visibility::Hidden,
            Name::new(format!("RetryCage:{}", perf.display_name())),
            ChildOf(crystal_entity),
        ))
        .id();

    let slab_entity = commands
        .spawn((
            HeroicPhraseSlab,
            Mesh3d(meshes.add(Cuboid::new(1.0, 0.012, 1.0))),
            MeshMaterial3d(slab_mat.clone()),
            Transform::default(),
            Visibility::Inherited,
            Name::new(format!("PhraseSlab:{}", perf.display_name())),
            ChildOf(parent),
        ))
        .id();

    let slack_entity = commands
        .spawn((
            HeroicSlackFloor,
            Mesh3d(meshes.add(Cuboid::new(0.28, 0.016, 1.0))),
            MeshMaterial3d(slack_mat.clone()),
            Transform::default(),
            Visibility::Inherited,
            Name::new(format!("SlackFloor:{}", perf.display_name())),
            ChildOf(parent),
        ))
        .id();

    PerformerVisualSlot {
        params,
        crystal_entity,
        cage_entity,
        retry_cage_entity,
        slab_entity,
        slack_entity,
        crystal_mat,
        cage_mat,
        retry_cage_mat,
        slab_mat,
        slack_mat,
        world_pos: Vec3::ZERO,
        smooth_z: 0.0,
    }
}

/// Reconcile and animate performer crystals, cages, phrase slabs, and slack floors.
pub fn sync_performers(
    mut commands: Commands,
    time: Res<Time>,
    palette: Res<HeroicPalette>,
    state: Res<HeroicState>,
    mut pool: ResMut<HeroicPerformerPool>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut mats: ResMut<Assets<StandardMaterial>>,
    mut transforms: Query<&mut Transform>,
    mut visibilities: Query<&mut Visibility>,
) {
    let Some(root) = pool.root else {
        return;
    };
    let dt = time.delta_secs();
    let now = time.elapsed_secs();

    // Map ContextId -> display key for parent lineage crystal blending.
    let mut key_by_id = HashMap::new();
    let mut live_ids = HashSet::new();
    for lane in &state.lanes {
        for perf in &lane.performers {
            live_ids.insert(perf.context_id);
            key_by_id.insert(perf.context_id, format!("{}:{}", perf.character, perf.context_id));
        }
    }

    // Despawn performers no longer present.
    let stale: Vec<ContextId> = pool
        .slots
        .keys()
        .copied()
        .filter(|id| !live_ids.contains(id))
        .collect();
    for id in stale {
        if let Some(slot) = pool.slots.remove(&id) {
            commands.entity(slot.crystal_entity).despawn();
            commands.entity(slot.slab_entity).despawn();
            commands.entity(slot.slack_entity).despawn();
        }
    }

    let any_alarm = state.any_alarm();
    let selected_id = state.selected_performer;

    for (li, lane) in state.lanes.iter().enumerate() {
        for (pi, perf) in lane.performers.iter().enumerate() {
            let parent_key = perf
                .parent_context_id
                .and_then(|pid| key_by_id.get(&pid))
                .map(String::as_str);

            if !pool.slots.contains_key(&perf.context_id) {
                let slot = spawn_performer_slot(
                    &mut commands,
                    root,
                    perf,
                    parent_key,
                    &mut meshes,
                    &mut mats,
                );
                pool.slots.insert(perf.context_id, slot);
            }

            let Some(slot) = pool.slots.get_mut(&perf.context_id) else {
                continue;
            };

            let target = performer_world_pos(&state, li, pi, perf, &palette, &slot.params, now);
            if slot.smooth_z == 0.0 && slot.world_pos == Vec3::ZERO {
                slot.smooth_z = target.z;
            } else {
                let lerp_k = (1.0 - (-dt * 8.0).exp()).clamp(0.0, 1.0);
                slot.smooth_z += (target.z - slot.smooth_z) * lerp_k;
            }
            let pos = Vec3::new(target.x, target.y, slot.smooth_z);
            slot.world_pos = pos;

            let dim = match selected_id {
                Some(sel) if sel != perf.context_id => palette.unselected_opacity,
                _ => 1.0,
            };

            let fallback = perf.statuses.iter().any(|w| {
                matches!(
                    w.disposition,
                    Some(Disposition::Fallback { reason, .. })
                        if reason != FallbackReason::Requested
                )
            });
            let retrying = perf
                .statuses
                .iter()
                .any(|w| w.readiness == Readiness::Running && w.attempt > 1);
            let error_now = perf.slack.state == SlackState::Alarm || fallback;

            let base_col = heroic_identity_color(perf.lane_name());
            // Core crystal stays white normally and on clean fallback; turns trouble-red on alarm/trouble.
            let mut core_col = palette.now;
            if perf.slack.state == SlackState::Alarm || fallback {
                let alarm_pulse = if palette.reduced_motion {
                    0.85
                } else {
                    0.55 + 0.45 * (now * TAU * 2.0).sin()
                };
                core_col = LinearRgba::rgb(
                    core_col.red * (1.0 - alarm_pulse) + palette.trouble.red * alarm_pulse,
                    core_col.green * (1.0 - alarm_pulse) + palette.trouble.green * alarm_pulse,
                    core_col.blue * (1.0 - alarm_pulse) + palette.trouble.blue * alarm_pulse,
                );
            }

            // Trouble quiets the world by ~1/3: non-alarm performers gently dim when any lane alarms.
            let trouble_dim = if any_alarm && !error_now { 0.67 } else { 1.0 };

            let flare_dur = (palette.durations_ms.performer_flare as f32 / 1000.0).max(0.05);
            let flare = if perf.flare_remaining > 0.0 {
                1.0 + 0.9 * (perf.flare_remaining / flare_dur).clamp(0.0, 1.0)
            } else {
                1.0
            };
            let gain = dim * trouble_dim * flare * (0.95 + 0.25 * palette.intensity);

            if let Some(mut m) = mats.get_mut(&slot.crystal_mat) {
                m.base_color = Color::LinearRgba(LinearRgba::rgb(
                    core_col.red * gain,
                    core_col.green * gain,
                    core_col.blue * gain,
                ));
            }
            if let Ok(mut tf) = transforms.get_mut(slot.crystal_entity) {
                tf.translation = pos;
                let s = (palette.sizes.performer_radius / 0.30).max(0.4);
                tf.scale = Vec3::splat(s);
                let tilt = if palette.reduced_motion {
                    0.0
                } else {
                    0.12 * (now * 1.4 + slot.params.tilt_phase).sin()
                };
                let spin = if palette.reduced_motion {
                    slot.params.tilt_phase
                } else {
                    now * 0.55 + slot.params.tilt_phase
                };
                tf.rotation = Quat::from_rotation_y(spin) * Quat::from_rotation_z(tilt);
            }

            // Outer wire cage is always visible in the performer's identity color.
            if let Ok(mut vis) = visibilities.get_mut(slot.cage_entity) {
                *vis = Visibility::Inherited;
            }
            if let Some(mut m) = mats.get_mut(&slot.cage_mat) {
                let k = dim * trouble_dim * 1.15;
                m.base_color = Color::LinearRgba(LinearRgba::rgb(
                    base_col.red * k,
                    base_col.green * k,
                    base_col.blue * k,
                ));
            }
            if let Ok(mut tf) = transforms.get_mut(slot.cage_entity) {
                let rate = if palette.reduced_motion { 0.0 } else { 0.45 };
                tf.rotation = Quat::from_rotation_y(-now * rate);
                tf.scale = Vec3::splat(palette.sizes.cage_scale.max(1.05));
            }

            // Second fast-spinning cage appears during RunningRetry (attempt > 1).
            if let Ok(mut vis) = visibilities.get_mut(slot.retry_cage_entity) {
                *vis = if retrying {
                    Visibility::Inherited
                } else {
                    Visibility::Hidden
                };
            }
            if retrying {
                if let Some(mut m) = mats.get_mut(&slot.retry_cage_mat) {
                    let k = dim * 1.35;
                    m.base_color = Color::LinearRgba(LinearRgba::rgb(
                        base_col.red * k,
                        base_col.green * k,
                        base_col.blue * k,
                    ));
                }
                if let Ok(mut tf) = transforms.get_mut(slot.retry_cage_entity) {
                    let rate = if palette.reduced_motion { 0.0 } else { 2.4 };
                    tf.rotation = Quat::from_rotation_y(-now * rate)
                        * Quat::from_rotation_x(now * rate * 0.65);
                    tf.scale = Vec3::splat(palette.sizes.cage_scale.max(1.05) * 1.18);
                }
            }

            // Dim phrase slab spanning the performer's current phrase on the road floor.
            let phrase_len_z =
                (lane.beats_per_phrase.max(1) as f32) * lane.period_secs * palette.units_per_second;
            let phrase_pos = lane.playhead_beats.rem_euclid(lane.beats_per_phrase.max(1) as f64)
                as f32
                * lane.period_secs
                * palette.units_per_second;
            let slab_center_z = (pos.z - phrase_len_z * 0.5 + phrase_pos * 0.25).min(-0.2);
            if let Ok(mut tf) = transforms.get_mut(slot.slab_entity) {
                tf.translation = Vec3::new(pos.x, 0.006, slab_center_z);
                tf.scale = Vec3::new(palette.lane_width * 0.86, 1.0, phrase_len_z.min(18.0));
            }
            if let Some(mut m) = mats.get_mut(&slot.slab_mat) {
                m.base_color = Color::LinearRgba(LinearRgba::new(
                    base_col.red * 0.22 * dim,
                    base_col.green * 0.22 * dim,
                    base_col.blue * 0.22 * dim,
                    0.35 * dim,
                ));
            }

            // Slack floor strip extending ahead of strike line by banked `slack_beats`.
            let road_len = (palette.road_seconds * palette.units_per_second).max(10.0);
            let slack_len = (perf.slack.slack_beats * lane.period_secs * palette.units_per_second)
                .clamp(0.05, road_len);
            if let Ok(mut tf) = transforms.get_mut(slot.slack_entity) {
                tf.translation = Vec3::new(pos.x, 0.011, -slack_len * 0.5);
                tf.scale = Vec3::new(1.0, 1.0, slack_len);
            }
            if let Some(mut m) = mats.get_mut(&slot.slack_mat) {
                let sc = match perf.slack.state {
                    SlackState::Alarm => LinearRgba::new(
                        palette.trouble.red * 1.4,
                        palette.trouble.green * 1.4,
                        palette.trouble.blue * 1.4,
                        0.72 * dim,
                    ),
                    SlackState::Working => LinearRgba::new(
                        base_col.red * 0.65,
                        base_col.green * 0.65,
                        base_col.blue * 0.65,
                        0.45 * dim,
                    ),
                    SlackState::Idle => LinearRgba::new(
                        base_col.red * 0.30,
                        base_col.green * 0.30,
                        base_col.blue * 0.30,
                        0.22 * dim,
                    ),
                };
                m.base_color = Color::LinearRgba(sc);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::mesh::VertexAttributeValues;

    #[test]
    fn crystal_params_is_deterministic_and_bounded() {
        let a = crystal_params("kaijutsu:ctx-1", None);
        let b = crystal_params("kaijutsu:ctx-1", None);
        assert_eq!(a, b);
        assert!((5..=7).contains(&a.sides));
        assert_eq!(a.radii.len(), a.sides);
        assert!(a.top_height > a.bottom_height);
        for &r in &a.radii {
            assert!((0.20..=0.40).contains(&r), "radius out of range: {r}");
        }
    }

    #[test]
    fn child_crystal_blends_parent_shape() {
        let parent = crystal_params("kaijutsu:parent", None);
        let orphan = crystal_params("kaijutsu:child", None);
        let child = crystal_params("kaijutsu:child", Some("kaijutsu:parent"));

        // Top height is exact 40/60 blend between parent and unparented seed.
        let expected_top = 0.4 * parent.top_height + 0.6 * orphan.top_height;
        assert!((child.top_height - expected_top).abs() < 1e-5);
    }

    #[test]
    fn build_crystal_mesh_is_closed_convex_and_positive_volume() {
        let p = crystal_params("amethyst:ctx-42", None);
        let (solid, cage) = build_crystal_meshes(&p);

        let Some(VertexAttributeValues::Float32x3(pos)) =
            solid.attribute(Mesh::ATTRIBUTE_POSITION)
        else {
            panic!("solid mesh missing Float32x3 positions");
        };
        let Some(Indices::U32(idx)) = solid.indices() else {
            panic!("solid mesh missing U32 indices");
        };

        assert_eq!(idx.len(), p.sides * 2 * 3);

        // Signed volume > 0 confirms outward winding and closed bipyramid.
        let mut vol6 = 0.0f32;
        for tri in idx.chunks_exact(3) {
            let a = Vec3::from_array(pos[tri[0] as usize]);
            let b = Vec3::from_array(pos[tri[1] as usize]);
            let c = Vec3::from_array(pos[tri[2] as usize]);
            vol6 += a.dot(b.cross(c));
        }
        assert!(vol6 > 0.01, "expected positive volume, got {vol6}");

        // Closedness: every quantized directed edge has an exact reverse twin.
        let q = |v: [f32; 3]| -> (i32, i32, i32) {
            (
                (v[0] * 10000.0).round() as i32,
                (v[1] * 10000.0).round() as i32,
                (v[2] * 10000.0).round() as i32,
            )
        };
        let mut edges: HashMap<((i32, i32, i32), (i32, i32, i32)), i32> = HashMap::new();
        for tri in idx.chunks_exact(3) {
            let v = [q(pos[tri[0] as usize]), q(pos[tri[1] as usize]), q(pos[tri[2] as usize])];
            for k in 0..3 {
                let a = v[k];
                let b = v[(k + 1) % 3];
                *edges.entry((a, b)).or_insert(0) += 1;
            }
        }
        for (&(a, b), &count) in &edges {
            assert_eq!(count, 1, "non-manifold edge");
            assert_eq!(edges.get(&(b, a)).copied().unwrap_or(0), 1, "open boundary edge");
        }

        // Waist polygon XZ convexity: same-sign cross products around ring.
        let n = p.sides;
        let pts: Vec<Vec2> = (0..n)
            .map(|i| {
                let ang = (i as f32 / n as f32) * TAU + p.twist;
                Vec2::new(p.radii[i] * ang.cos(), p.radii[i] * ang.sin())
            })
            .collect();
        for i in 0..n {
            let a = pts[i];
            let b = pts[(i + 1) % n];
            let c = pts[(i + 2) % n];
            let cross = (b - a).perp_dot(c - b);
            assert!(cross > 0.0, "waist polygon non-convex at vertex {i}: {cross}");
        }

        // Cage mesh has 3 edges per side (top-to-ring, bot-to-ring, ring perimeter).
        let Some(Indices::U32(cage_idx)) = cage.indices() else {
            panic!("cage mesh missing U32 indices");
        };
        assert_eq!(cage_idx.len(), p.sides * 3 * 2);
    }
}
