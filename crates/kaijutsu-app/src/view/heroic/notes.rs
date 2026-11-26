//! Notes and strike pads for the heroic view (`docs/newui.md`, "What a note shows").
//!
//! Renders every visible `WorkStatus` record using shared sloped-disc, wireframe,
//! and ring meshes, and drives strike-line pad flashes and expanding hit rings.

use std::collections::HashMap;
use std::f32::consts::TAU;

use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use kaijutsu_present::work::{NoteVisualState, classify_note};
use kaijutsu_types::{ContextId, Tick};

use super::state::HeroicState;
use crate::view::scene_palette::{HeroicPalette, heroic_identity_color};

#[derive(Component)]
pub struct HeroicNotesBatch;

#[derive(Component)]
pub struct HeroicWireNotesBatch;

#[derive(Default)]
pub struct PadPulseState {
    pub elapsed: f32,
    pub color: LinearRgba,
    pub hollow: bool,
}

#[derive(Resource, Default)]
pub struct HeroicNotesRuntime {
    pub pad_pulses: HashMap<ContextId, PadPulseState>,
}

/// Spawn batched note meshes and strike pads under `root`.
pub fn spawn_notes_and_pads(
    commands: &mut Commands,
    root: Entity,
    meshes: &mut Assets<Mesh>,
    mats: &mut Assets<StandardMaterial>,
) {
    let empty_tri = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default())
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, Vec::<[f32; 3]>::new())
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, Vec::<[f32; 3]>::new())
        .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, Vec::<[f32; 4]>::new())
        .with_inserted_indices(Indices::U32(Vec::new()));
    let empty_lines = Mesh::new(PrimitiveTopology::LineList, RenderAssetUsages::default())
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, Vec::<[f32; 3]>::new())
        .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, Vec::<[f32; 4]>::new())
        .with_inserted_indices(Indices::U32(Vec::new()));

    let mat = mats.add(StandardMaterial {
        base_color: Color::WHITE,
        unlit: true,
        ..default()
    });

    commands.spawn((
        HeroicNotesBatch,
        Mesh3d(meshes.add(empty_tri)),
        MeshMaterial3d(mat.clone()),
        Transform::default(),
        Visibility::Inherited,
        Name::new("HeroicNotesSolid"),
        ChildOf(root),
    ));

    commands.spawn((
        HeroicWireNotesBatch,
        Mesh3d(meshes.add(empty_lines)),
        MeshMaterial3d(mat),
        Transform::default(),
        Visibility::Inherited,
        Name::new("HeroicNotesWire"),
        ChildOf(root),
    ));
}

fn fog_apply(
    c: LinearRgba,
    world_pos: Vec3,
    cam_pos: Vec3,
    palette: &HeroicPalette,
    scale: f32,
) -> [f32; 4] {
    let dist = cam_pos.distance(world_pos);
    let f = ((dist - palette.fog.start) / (palette.fog.end - palette.fog.start).max(0.01))
        .clamp(0.0, 1.0);
    let fog = palette.fog_color;
    [
        (c.red * scale) * (1.0 - f) + fog.red * f,
        (c.green * scale) * (1.0 - f) + fog.green * f,
        (c.blue * scale) * (1.0 - f) + fog.blue * f,
        1.0,
    ]
}

fn append_disc(
    pos: &mut Vec<[f32; 3]>,
    norm: &mut Vec<[f32; 3]>,
    col: &mut Vec<[f32; 4]>,
    idx: &mut Vec<u32>,
    center: Vec3,
    r_bot: f32,
    r_top: f32,
    h: f32,
    c: [f32; 4],
) {
    let segs = 16usize;
    let top_center = pos.len() as u32;
    pos.push([center.x, center.y + h, center.z]);
    norm.push([0.0, 1.0, 0.0]);
    col.push(c);

    let top_ring = pos.len() as u32;
    for i in 0..segs {
        let a = (i as f32 / segs as f32) * TAU;
        let (s, cs) = a.sin_cos();
        pos.push([center.x + cs * r_top, center.y + h, center.z + s * r_top]);
        norm.push([0.0, 1.0, 0.0]);
        col.push(c);
    }
    for i in 0..segs as u32 {
        let next = (i + 1) % segs as u32;
        idx.extend_from_slice(&[top_center, top_ring + next, top_ring + i]);
    }

    let side_base = pos.len() as u32;
    for i in 0..segs {
        let a = (i as f32 / segs as f32) * TAU;
        let (s, cs) = a.sin_cos();
        pos.push([center.x + cs * r_top, center.y + h, center.z + s * r_top]);
        norm.push([cs, 0.35, s]);
        col.push(c);
        pos.push([center.x + cs * r_bot, center.y, center.z + s * r_bot]);
        norm.push([cs, 0.35, s]);
        col.push(c);
    }
    for i in 0..segs as u32 {
        let next = (i + 1) % segs as u32;
        let a0 = side_base + i * 2;
        let b0 = a0 + 1;
        let a1 = side_base + next * 2;
        let b1 = a1 + 1;
        idx.extend_from_slice(&[a0, a1, b0, b0, a1, b1]);
    }
}

fn append_ring_flat(
    pos: &mut Vec<[f32; 3]>,
    norm: &mut Vec<[f32; 3]>,
    col: &mut Vec<[f32; 4]>,
    idx: &mut Vec<u32>,
    center: Vec3,
    r_inner: f32,
    r_outer: f32,
    c: [f32; 4],
) {
    let segs = 20usize;
    let base = pos.len() as u32;
    for i in 0..segs {
        let a = (i as f32 / segs as f32) * TAU;
        let (s, cs) = a.sin_cos();
        pos.push([center.x + cs * r_inner, center.y, center.z + s * r_inner]);
        norm.push([0.0, 1.0, 0.0]);
        col.push(c);
        pos.push([center.x + cs * r_outer, center.y, center.z + s * r_outer]);
        norm.push([0.0, 1.0, 0.0]);
        col.push(c);
    }
    for i in 0..segs as u32 {
        let next = (i + 1) % segs as u32;
        let i0 = base + i * 2;
        let o0 = i0 + 1;
        let i1 = base + next * 2;
        let o1 = i1 + 1;
        idx.extend_from_slice(&[i0, i1, o0, o0, i1, o1]);
    }
}

fn append_wire_disc(
    pos: &mut Vec<[f32; 3]>,
    col: &mut Vec<[f32; 4]>,
    center: Vec3,
    r_bot: f32,
    r_top: f32,
    h: f32,
    c: [f32; 4],
) {
    let segs = 14usize;
    for i in 0..segs {
        let a0 = (i as f32 / segs as f32) * TAU;
        let a1 = ((i + 1) as f32 / segs as f32) * TAU;
        let (s0, c0) = a0.sin_cos();
        let (s1, c1) = a1.sin_cos();

        // Top loop
        pos.push([center.x + c0 * r_top, center.y + h, center.z + s0 * r_top]);
        pos.push([center.x + c1 * r_top, center.y + h, center.z + s1 * r_top]);
        col.push(c);
        col.push(c);

        // Bottom loop
        pos.push([center.x + c0 * r_bot, center.y, center.z + s0 * r_bot]);
        pos.push([center.x + c1 * r_bot, center.y, center.z + s1 * r_bot]);
        col.push(c);
        col.push(c);

        // Vertical ribs every 2nd segment
        if i % 2 == 0 {
            pos.push([center.x + c0 * r_top, center.y + h, center.z + s0 * r_top]);
            pos.push([center.x + c0 * r_bot, center.y, center.z + s0 * r_bot]);
            col.push(c);
            col.push(c);
        }
    }
}

/// Update visible notes (all 10 rules) and strike pads / expanding rings.
pub fn sync_notes_and_pads(
    time: Res<Time>,
    palette: Res<HeroicPalette>,
    mut state: ResMut<HeroicState>,
    mut runtime: ResMut<HeroicNotesRuntime>,
    mut meshes: ResMut<Assets<Mesh>>,
    cam_q: Query<&Transform, With<super::HeroicCamera>>,
    solid_q: Query<&Mesh3d, With<HeroicNotesBatch>>,
    wire_q: Query<&Mesh3d, With<HeroicWireNotesBatch>>,
) {
    let dt = time.delta_secs();
    let now_secs = time.elapsed_secs_f64();
    let pulse_dur = (palette.durations_ms.pad_pulse as f32 / 1000.0).max(0.05);
    let cam_pos = cam_q
        .single()
        .map(|tf| tf.translation)
        .unwrap_or(Vec3::new(0.0, 2.25, 3.33));

    runtime.pad_pulses.retain(|_, pulse| {
        pulse.elapsed += dt;
        pulse.elapsed < pulse_dur
    });
    for flash in state.pending_flashes.drain(..) {
        runtime.pad_pulses.insert(flash.context_id, PadPulseState {
            elapsed: 0.0,
            color: flash.color,
            hollow: flash.hollow,
        });
    }

    let lane_dim = |performer: Option<&super::state::HeroicPerformer>| {
        match state.selected_performer {
            Some(selected) if performer.is_none_or(|p| p.context_id != selected) => {
                palette.unselected_opacity
            }
            _ => 1.0,
        }
    };
    let trouble_dim = if state.any_alarm() { 0.67 } else { 1.0 };

    let lane_w = palette.lane_width;
    let road_len = (palette.road_seconds * palette.units_per_second).max(10.0);
    let r_base = palette.sizes.note_radius;
    let h_base = palette.sizes.note_height;
    let pad_r = palette.sizes.pad_radius;

    let mut solid_pos = Vec::new();
    let mut solid_norm = Vec::new();
    let mut solid_col = Vec::new();
    let mut solid_idx = Vec::new();

    let mut wire_pos = Vec::new();
    let mut wire_col = Vec::new();

    // Strike pads at z = 0 (`docs/newui.md`, "Strike line and pads").
    for (li, (_, performer, _)) in state.road_lanes().enumerate() {
        let cx = state.lane_center_x(li, lane_w);
        let dim = lane_dim(performer);
        let color = performer
            .map(|p| heroic_identity_color(p.lane_name()))
            .unwrap_or(palette.rail);
        let base_pad_col = [
            color.red * 0.75 * dim,
            color.green * 0.75 * dim,
            color.blue * 0.75 * dim,
            1.0,
        ];
        append_ring_flat(
            &mut solid_pos,
            &mut solid_norm,
            &mut solid_col,
            &mut solid_idx,
            Vec3::new(cx, 0.02, 0.0),
            pad_r * 0.72,
            pad_r,
            base_pad_col,
        );

        if let Some(pulse) = performer.and_then(|p| runtime.pad_pulses.get(&p.context_id)) {
            let t = (pulse.elapsed / pulse_dur).clamp(0.0, 1.0);
            let fade = 1.0 - t;
            let flash_scale = (1.5 + 0.8 * fade) * dim;
            let flash_col = [
                pulse.color.red * flash_scale,
                pulse.color.green * flash_scale,
                pulse.color.blue * flash_scale,
                1.0,
            ];
            let swell_r = pad_r * (1.0 + 0.18 * fade);
            if pulse.hollow {
                append_ring_flat(
                    &mut solid_pos,
                    &mut solid_norm,
                    &mut solid_col,
                    &mut solid_idx,
                    Vec3::new(cx, 0.025, 0.0),
                    swell_r * 0.55,
                    swell_r,
                    flash_col,
                );
            } else {
                append_ring_flat(
                    &mut solid_pos,
                    &mut solid_norm,
                    &mut solid_col,
                    &mut solid_idx,
                    Vec3::new(cx, 0.025, 0.0),
                    0.02,
                    swell_r,
                    flash_col,
                );
            }
            // Expanding secondary ring over 0.3 s.
            let ring_r = pad_r * (1.0 + t * 1.45);
            let ring_col = [
                pulse.color.red * fade * 1.3 * dim,
                pulse.color.green * fade * 1.3 * dim,
                pulse.color.blue * fade * 1.3 * dim,
                1.0,
            ];
            append_ring_flat(
                &mut solid_pos,
                &mut solid_norm,
                &mut solid_col,
                &mut solid_idx,
                Vec3::new(cx, 0.028, 0.0),
                ring_r - 0.05,
                ring_r,
                ring_col,
            );
        }
    }

    // Notes across all lanes and performers.
    for (li, lane) in state.lanes.iter().enumerate() {
        let playhead = Tick::new(lane.playhead_beats.floor() as i64);
        let margin = kaijutsu_types::TickDelta::new(lane.commit_margin_ticks);

        for (pi, perf) in lane.performers.iter().enumerate() {
            let cx = state.performer_lane_center_x(li, pi, lane_w);
            let dim = lane_dim(Some(perf));
            let perf_col = heroic_identity_color(perf.lane_name());
            let is_perf_alarm = perf.slack.state == kaijutsu_present::work::SlackState::Alarm;
            let note_dim = dim * if is_perf_alarm { 1.0 } else { trouble_dim };

            for status in &perf.statuses {
                let dt_beats = status.start.0 as f64 - lane.playhead_beats;
                let z = -(dt_beats as f32) * lane.period_secs * palette.units_per_second;
                if z > 0.3 || z < -road_len {
                    continue;
                }

                let visual = classify_note(status, playhead, margin);
                let center = Vec3::new(cx, 0.02, z);

                match visual {
                    NoteVisualState::CancelledOrSuperseded => {
                        let age = (playhead.0 - status.start.0).max(0) as f32;
                        let fade = (0.30 * (-age * 0.5).exp()).clamp(0.06, 0.30);
                        let c = fog_apply(perf_col, center, cam_pos, &palette, fade * note_dim);
                        append_wire_disc(
                            &mut wire_pos,
                            &mut wire_col,
                            center,
                            r_base * 0.8,
                            r_base * 0.6,
                            h_base * 0.5,
                            c,
                        );
                    }
                    NoteVisualState::RequestedRepeat { at_start } => {
                        if !at_start {
                            let c = fog_apply(perf_col, center, cam_pos, &palette, 1.25 * note_dim);
                            append_ring_flat(
                                &mut solid_pos,
                                &mut solid_norm,
                                &mut solid_col,
                                &mut solid_idx,
                                Vec3::new(cx, 0.04, z),
                                r_base * 0.42,
                                r_base,
                                c,
                            );
                        }
                    }
                    NoteVisualState::TroubleFallback {
                        played_content,
                        in_lock_zone,
                        at_start,
                        ..
                    } => {
                        if !at_start && in_lock_zone {
                            let c = fog_apply(palette.trouble, center, cam_pos, &palette, 1.45 * dim);
                            let inner = if played_content { 0.04 } else { r_base * 0.55 };
                            append_ring_flat(
                                &mut solid_pos,
                                &mut solid_norm,
                                &mut solid_col,
                                &mut solid_idx,
                                Vec3::new(cx, 0.045, z),
                                inner,
                                r_base * 1.05,
                                c,
                            );
                        } else if !at_start {
                            let c = fog_apply(palette.trouble, center, cam_pos, &palette, 0.85 * dim);
                            append_wire_disc(
                                &mut wire_pos,
                                &mut wire_col,
                                center,
                                r_base * 0.9,
                                r_base * 0.72,
                                h_base * 0.8,
                                c,
                            );
                        }
                    }
                    NoteVisualState::Committed {
                        in_lock_zone,
                        at_start,
                    } => {
                        if !at_start {
                            let boost = if in_lock_zone { 1.45 } else { 1.2 };
                            let c = fog_apply(perf_col, center, cam_pos, &palette, boost * note_dim);
                            append_disc(
                                &mut solid_pos,
                                &mut solid_norm,
                                &mut solid_col,
                                &mut solid_idx,
                                center,
                                r_base,
                                r_base * 0.78,
                                h_base,
                                c,
                            );
                        }
                    }
                    NoteVisualState::FailedOpen => {
                        let c = fog_apply(palette.trouble, center, cam_pos, &palette, 1.35 * dim);
                        append_wire_disc(
                            &mut wire_pos,
                            &mut wire_col,
                            center,
                            r_base,
                            r_base * 0.8,
                            h_base,
                            c,
                        );
                    }
                    NoteVisualState::ReadyOpen { ready_at } => {
                        let age_beats = ready_at
                            .map(|r| (lane.playhead_beats - r.0 as f64).max(0.0) as f32)
                            .unwrap_or(99.0);
                        let pop = if !palette.reduced_motion && age_beats < 1.0 {
                            let u = age_beats.clamp(0.0, 1.0);
                            1.0 + 0.65 * (1.0 - u) * (1.0 - u)
                        } else {
                            1.0
                        };
                        let c = fog_apply(perf_col, center, cam_pos, &palette, 1.2 * note_dim);
                        append_disc(
                            &mut solid_pos,
                            &mut solid_norm,
                            &mut solid_col,
                            &mut solid_idx,
                            center,
                            r_base * pop,
                            r_base * 0.78 * pop,
                            h_base * pop,
                            c,
                        );
                    }
                    NoteVisualState::RunningRetry { .. } => {
                        // Flicker at 2.8 Hz (under the 3 Hz no-strobe cap).
                        let flicker = if palette.reduced_motion
                            || (now_secs * std::f64::consts::TAU * 2.8).sin() > 0.0
                        {
                            1.15
                        } else {
                            0.35
                        };
                        let c = fog_apply(perf_col, center, cam_pos, &palette, flicker * note_dim);
                        append_wire_disc(
                            &mut wire_pos,
                            &mut wire_col,
                            center,
                            r_base,
                            r_base * 0.78,
                            h_base,
                            c,
                        );
                    }
                    NoteVisualState::RunningOpen => {
                        let c = fog_apply(perf_col, center, cam_pos, &palette, 1.15 * note_dim);
                        append_wire_disc(
                            &mut wire_pos,
                            &mut wire_col,
                            center,
                            r_base,
                            r_base * 0.78,
                            h_base,
                            c,
                        );
                    }
                    NoteVisualState::QueuedOpen => {
                        let c = fog_apply(perf_col, center, cam_pos, &palette, 0.55 * note_dim);
                        append_wire_disc(
                            &mut wire_pos,
                            &mut wire_col,
                            center,
                            r_base * 0.8,
                            r_base * 0.62,
                            h_base * 0.8,
                            c,
                        );
                    }
                }
            }
        }
    }

    if let Ok(mesh_h) = solid_q.single()
        && let Some(mut mesh) = meshes.get_mut(&mesh_h.0)
    {
        *mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default())
            .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, solid_pos)
            .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, solid_norm)
            .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, solid_col)
            .with_inserted_indices(Indices::U32(solid_idx));
    }

    if let Ok(mesh_h) = wire_q.single()
        && let Some(mut mesh) = meshes.get_mut(&mesh_h.0)
    {
        let idx: Vec<u32> = (0..wire_pos.len() as u32).collect();
        *mesh = Mesh::new(PrimitiveTopology::LineList, RenderAssetUsages::default())
            .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, wire_pos)
            .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, wire_col)
            .with_inserted_indices(Indices::U32(idx));
    }
}
