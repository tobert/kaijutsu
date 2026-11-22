//! World-space MSDF text & 3D candidate bars (`docs/newui.md`, "Streaming text"
//! and "Lane names").
//!
//! - MSDF lane nameplates sit just behind the strike line (`y > 0`), tilted toward
//!   the camera, kept at HDR brightness `<= 0.85` so text never blooms.
//! - Streaming text ribbons sit above talking crystals and refresh at most ~4 Hz.
//! - Candidate probability bars float beside decision-player crystals, highlighting
//!   the winning choice in `now` white.

use std::collections::HashMap;

use bevy::prelude::*;
use kaijutsu_types::ContextId;
use peniko::Brush;

use super::performers::HeroicPerformerPool;
use super::state::HeroicState;
use crate::shaders::WellCardMaterial;
use crate::text::ShapingFonts;
use crate::text::components::bevy_color_to_brush;
use crate::text::msdf::{FontDataMap, MsdfAtlas, MsdfBlockGlyphs, collect_msdf_glyphs};
use crate::text::shaping::{VelloFont, VelloTextAlign, VelloTextStyle};
use crate::view::scene_palette::{HeroicPalette, heroic_identity_color};
use crate::view::time_well::panel::{commit_panel_glyphs, create_msdf_panel};

const LABEL_TEX_W: u32 = 240;
const LABEL_TEX_H: u32 = 88;
const RIBBON_TEX_W: u32 = 360;
const RIBBON_TEX_H: u32 = 88;
const PAD: f32 = 8.0;
const RIBBON_REFRESH_SECS: f64 = 0.25;

#[derive(Component)]
pub struct HeroicTextRoot;

#[derive(Component)]
pub struct HeroicLaneNameplate;

#[derive(Component)]
pub struct HeroicStreamRibbon;

#[derive(Component)]
pub struct HeroicCandidateBarsRoot;

#[derive(Debug, Clone)]
struct LanePlateSlot {
    entity: Entity,
    mat: Handle<WellCardMaterial>,
    last_label: String,
}

#[derive(Debug, Clone)]
struct StreamRibbonSlot {
    entity: Entity,
    mat: Handle<WellCardMaterial>,
    last_text: String,
    last_refresh: f64,
}

#[derive(Resource, Default)]
pub struct HeroicTextState {
    pub root: Option<Entity>,
    lane_plates: Vec<LanePlateSlot>,
    ribbons: HashMap<ContextId, StreamRibbonSlot>,
    bar_mesh: Option<Handle<Mesh>>,
    candidate_slots: HashMap<ContextId, Vec<(Entity, Handle<StandardMaterial>)>>,
}

/// Spawn text/bars root container under `root`.
pub fn spawn_text_root(commands: &mut Commands, root: Entity) -> Entity {
    commands
        .spawn((
            HeroicTextRoot,
            Transform::default(),
            Visibility::Inherited,
            Name::new("HeroicTextRoot"),
            ChildOf(root),
        ))
        .id()
}

fn layout_msdf_line(
    text: &str,
    font: &VelloFont,
    font_size: f32,
    max_w: f32,
    brush: &Brush,
    align: VelloTextAlign,
    atlas: &mut MsdfAtlas,
    font_data_map: &mut FontDataMap,
) -> Vec<crate::text::msdf::PositionedGlyph> {
    if text.is_empty() {
        return Vec::new();
    }
    let layout = font.layout(
        text,
        &VelloTextStyle {
            font_size,
            line_height: 1.15,
            brush: brush.clone(),
            ..default()
        },
        align,
        Some(max_w),
    );
    for line in layout.lines() {
        for item in line.items() {
            if let parley::PositionedLayoutItem::GlyphRun(gr) = item {
                font_data_map.register(gr.run().font());
            }
        }
    }
    collect_msdf_glyphs(&layout, &[], brush, (PAD as f64, PAD as f64), atlas)
}

/// Sync MSDF lane nameplates, throttled streaming text ribbons, and candidate bars.
#[allow(clippy::too_many_arguments)]
pub fn sync_heroic_text(
    mut commands: Commands,
    time: Res<Time>,
    palette: Res<HeroicPalette>,
    state: Res<HeroicState>,
    pool: Res<HeroicPerformerPool>,
    mut text_state: ResMut<HeroicTextState>,
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<ShapingFonts>,
    mut atlas: Option<ResMut<MsdfAtlas>>,
    mut font_data_map: ResMut<FontDataMap>,
    mut images: ResMut<Assets<Image>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut card_mats: ResMut<Assets<WellCardMaterial>>,
    mut std_mats: ResMut<Assets<StandardMaterial>>,
    mut msdf_q: Query<(Option<&mut MsdfBlockGlyphs>, &mut Transform, &mut Visibility)>,
) {
    let Some(root) = text_state.root else {
        return;
    };
    let now = time.elapsed_secs_f64();
    let sel_id = state.selected_performer;
    let sel_lane = state.selected_location().map(|(li, _)| li);

    // 1. Ensure one MSDF lane nameplate per lane near strike line.
    while text_state.lane_plates.len() < state.lanes.len() {
        let li = text_state.lane_plates.len();
        let (tex, panel_bundle) = create_msdf_panel(&mut images, LABEL_TEX_W, LABEL_TEX_H);
        let mat = card_mats.add(WellCardMaterial {
            texture: tex,
            accent: Vec4::new(0.03, 0.02, 0.08, 0.78),
            params: Vec4::ZERO,
            shape: Vec4::new(LABEL_TEX_W as f32 / LABEL_TEX_H as f32, 0.14, 0.04, 0.0),
            border: Vec4::ZERO,
            // Keep <= 0.85 so text stays cleanly below the 1.0 bloom threshold.
            dim: Vec4::new(0.85, 0.0, 0.0, 0.0),
        });
        let quad = meshes.add(Rectangle::new(0.78, 0.22));
        let entity = commands
            .spawn((
                HeroicLaneNameplate,
                panel_bundle,
                Mesh3d(quad),
                MeshMaterial3d(mat.clone()),
                Transform::default(),
                Visibility::Inherited,
                Name::new(format!("HeroicLaneNameplate:{li}")),
                ChildOf(root),
            ))
            .id();
        text_state.lane_plates.push(LanePlateSlot {
            entity,
            mat,
            last_label: String::new(),
        });
    }
    while text_state.lane_plates.len() > state.lanes.len() {
        if let Some(old) = text_state.lane_plates.pop() {
            commands.entity(old.entity).despawn();
        }
    }

    let font = fonts.get(&font_handles.mono);
    let mut atlas_opt = atlas.as_deref_mut();
    let brush = bevy_color_to_brush(Color::srgba(0.92, 0.96, 1.0, 0.95));

    for (li, lane) in state.lanes.iter().enumerate() {
        let slot = &mut text_state.lane_plates[li];
        let lane_x = state.lane_center_x(li, palette.lane_width);
        let dim = match sel_lane {
            Some(s) if s != li => palette.unselected_opacity,
            _ => 1.0,
        };
        let char_col = lane
            .single_character_name()
            .map(heroic_identity_color)
            .unwrap_or(palette.rail);
        if let Some(mut m) = card_mats.get_mut(&slot.mat) {
            m.border = Vec4::new(char_col.red, char_col.green, char_col.blue, 0.65);
            m.dim.x = 0.85 * dim;
        }
        if let Ok((Some(mut msdf), mut tf, _)) = msdf_q.get_mut(slot.entity) {
            // Sit cleanly between the strike pads and the elevated bottom chips + dock bar.
            tf.translation = Vec3::new(lane_x, 0.05, 0.26);
            tf.rotation = Quat::from_rotation_x(-1.18);
            let title = lane
                .single_character_name()
                .map(|c| format!("{}\n{}", lane.track_id, c))
                .unwrap_or_else(|| lane.track_id.clone());
            if title != slot.last_label
                && let (Some(f), Some(at)) = (font, atlas_opt.as_deref_mut())
            {
                let glyphs = layout_msdf_line(
                    &title,
                    f,
                    20.0,
                    LABEL_TEX_W as f32 - 2.0 * PAD,
                    &brush,
                    VelloTextAlign::Middle,
                    at,
                    &mut font_data_map,
                );
                commit_panel_glyphs(&mut msdf, glyphs);
                slot.last_label = title;
            }
        }
    }

    // 2. Streaming text ribbons above crystals (throttled ~4 Hz per performer).
    // Note: MSDF text panels remain crisp and non-bloomed with dim.x = 0.82.
    let stale_ribbons: Vec<ContextId> = text_state
        .ribbons
        .keys()
        .copied()
        .filter(|id| !pool.slots.contains_key(id))
        .collect();
    for id in stale_ribbons {
        if let Some(old) = text_state.ribbons.remove(&id) {
            commands.entity(old.entity).despawn();
        }
    }

    // Precompute non-overlapping world offsets for active streaming ribbons so
    // performers clustered at similar Z/lanes separate vertically.
    let active_ribbons: Vec<(ContextId, Vec3)> = state
        .lanes
        .iter()
        .flat_map(|lane| lane.performers.iter())
        .filter(|perf| perf.tail_lines.last().is_some_and(|s| !s.is_empty()))
        .filter_map(|perf| {
            pool.slots
                .get(&perf.context_id)
                .map(|slot| (perf.context_id, slot.world_pos))
        })
        .collect();
    let ribbon_positions = layout_ribbon_positions(&active_ribbons);

    for lane in &state.lanes {
        for perf in &lane.performers {
            let Some(perf_slot) = pool.slots.get(&perf.context_id) else {
                continue;
            };
            if !text_state.ribbons.contains_key(&perf.context_id) {
                let (tex, panel_bundle) = create_msdf_panel(&mut images, RIBBON_TEX_W, RIBBON_TEX_H);
                let mat = card_mats.add(WellCardMaterial {
                    texture: tex,
                    accent: Vec4::new(0.025, 0.02, 0.07, 0.56),
                    params: Vec4::ZERO,
                    shape: Vec4::new(RIBBON_TEX_W as f32 / RIBBON_TEX_H as f32, 0.12, 0.03, 0.0),
                    border: Vec4::ZERO,
                    dim: Vec4::new(0.82, 0.0, 0.0, 0.0),
                });
                let quad = meshes.add(Rectangle::new(1.18, 0.28));
                let entity = commands
                    .spawn((
                        HeroicStreamRibbon,
                        panel_bundle,
                        Mesh3d(quad),
                        MeshMaterial3d(mat.clone()),
                        Transform::default(),
                        Visibility::Hidden,
                        Name::new(format!("HeroicRibbon:{}", perf.display_name())),
                        ChildOf(root),
                    ))
                    .id();
                text_state.ribbons.insert(
                    perf.context_id,
                    StreamRibbonSlot {
                        entity,
                        mat,
                        last_text: String::new(),
                        last_refresh: -10.0,
                    },
                );
            }

            let Some(rslot) = text_state.ribbons.get_mut(&perf.context_id) else {
                continue;
            };
            let dim = match sel_id {
                Some(s) if s != perf.context_id => palette.unselected_opacity,
                _ => 1.0,
            };
            if let Some(mut m) = card_mats.get_mut(&rslot.mat) {
                m.dim.x = 0.82 * dim;
            }
            let snippet = perf.tail_lines.last().map(String::as_str).unwrap_or("");
            if let Ok((Some(mut msdf), mut tf, mut vis)) = msdf_q.get_mut(rslot.entity) {
                if snippet.is_empty() {
                    *vis = Visibility::Hidden;
                } else {
                    *vis = Visibility::Inherited;
                    tf.translation = ribbon_positions
                        .get(&perf.context_id)
                        .copied()
                        .unwrap_or(perf_slot.world_pos + Vec3::new(0.0, 0.76, 0.0));
                    tf.rotation = Quat::from_rotation_x(-0.14);
                    if snippet != rslot.last_text
                        && now - rslot.last_refresh >= RIBBON_REFRESH_SECS
                        && let (Some(f), Some(at)) = (font, atlas_opt.as_deref_mut())
                    {
                        let truncated: String = snippet.chars().take(54).collect();
                        let glyphs = layout_msdf_line(
                            &truncated,
                            f,
                            18.0,
                            RIBBON_TEX_W as f32 - 2.0 * PAD,
                            &brush,
                            VelloTextAlign::Left,
                            at,
                            &mut font_data_map,
                        );
                        commit_panel_glyphs(&mut msdf, glyphs);
                        rslot.last_text = snippet.to_string();
                        rslot.last_refresh = now;
                    }
                }
            }
        }
    }

    // 3. Cached 3D candidate probability bars beside decision-player crystals (zero per-frame alloc).
    let bar_mesh = text_state
        .bar_mesh
        .get_or_insert_with(|| meshes.add(Cuboid::new(0.08, 1.0, 0.08)))
        .clone();

    let stale_bars: Vec<ContextId> = text_state
        .candidate_slots
        .keys()
        .copied()
        .filter(|id| !pool.slots.contains_key(id))
        .collect();
    for id in stale_bars {
        if let Some(bars) = text_state.candidate_slots.remove(&id) {
            for (e, _) in bars {
                commands.entity(e).despawn();
            }
        }
    }

    for lane in &state.lanes {
        for perf in &lane.performers {
            let Some(perf_slot) = pool.slots.get(&perf.context_id) else {
                continue;
            };
            let slots = text_state
                .candidate_slots
                .entry(perf.context_id)
                .or_insert_with(|| {
                    (0..4)
                        .map(|ci| {
                            let mat = std_mats.add(StandardMaterial {
                                base_color: Color::WHITE,
                                unlit: true,
                                ..default()
                            });
                            let e = commands
                                .spawn((
                                    HeroicCandidateBarsRoot,
                                    Mesh3d(bar_mesh.clone()),
                                    MeshMaterial3d(mat.clone()),
                                    Transform::default(),
                                    Visibility::Hidden,
                                    Name::new(format!("CandidateBar:{}:{ci}", perf.display_name())),
                                    ChildOf(root),
                                ))
                                .id();
                            (e, mat)
                        })
                        .collect()
                });

            let dim = match sel_id {
                Some(s) if s != perf.context_id => palette.unselected_opacity,
                _ => 1.0,
            };
            let perf_col = heroic_identity_color(&perf.character);

            for (ci, (bar_e, bar_mat)) in slots.iter().enumerate() {
                if let Some(cand) = perf.candidates.get(ci) {
                    let h = (0.10 + cand.probability.clamp(0.0, 1.0) * 0.65).max(0.08);
                    let col = if cand.picked {
                        LinearRgba::rgb(
                            palette.now.red * 1.45 * dim,
                            palette.now.green * 1.45 * dim,
                            palette.now.blue * 1.45 * dim,
                        )
                    } else {
                        LinearRgba::rgb(
                            perf_col.red * 0.55 * dim,
                            perf_col.green * 0.55 * dim,
                            perf_col.blue * 0.55 * dim,
                        )
                    };
                    if let Some(mut m) = std_mats.get_mut(bar_mat) {
                        m.base_color = Color::LinearRgba(col);
                    }
                    if let Ok((_, mut tf, mut vis)) = msdf_q.get_mut(*bar_e) {
                        *vis = Visibility::Inherited;
                        tf.translation = perf_slot.world_pos
                            + Vec3::new(0.46 + (ci as f32) * 0.11, h * 0.5 - 0.12, 0.0);
                        tf.scale = Vec3::new(1.0, h, 1.0);
                    }
                } else if let Ok((_, _, mut vis)) = msdf_q.get_mut(*bar_e) {
                    *vis = Visibility::Hidden;
                }
            }
        }
    }
}

/// Compute non-overlapping world translations for active streaming ribbons.
///
/// Ribbons start 0.76 units above each performer crystal and step upward when
/// another visible ribbon sits within overlapping X/Y/Z bounds.
fn layout_ribbon_positions(active: &[(ContextId, Vec3)]) -> HashMap<ContextId, Vec3> {
    const BASE_Y: f32 = 0.76;
    const STEP_Y: f32 = 0.34;
    const MIN_DX: f32 = 1.35;
    const MIN_DY: f32 = 0.30;
    const MIN_DZ: f32 = 2.20;
    const MAX_TIERS: usize = 5;

    let mut ordered: Vec<(ContextId, Vec3)> = active.to_vec();
    ordered.sort_by(|a, b| {
        b.1.z
            .total_cmp(&a.1.z)
            .then_with(|| a.1.x.total_cmp(&b.1.x))
            .then_with(|| a.0.cmp(&b.0))
    });

    let mut placed: Vec<Vec3> = Vec::with_capacity(ordered.len());
    let mut out = HashMap::with_capacity(ordered.len());
    for (id, base_pos) in ordered {
        let mut candidate = base_pos + Vec3::new(0.0, BASE_Y, 0.0);
        for _ in 0..MAX_TIERS {
            let collides = placed.iter().any(|p| {
                (p.x - candidate.x).abs() < MIN_DX
                    && (p.y - candidate.y).abs() < MIN_DY
                    && (p.z - candidate.z).abs() < MIN_DZ
            });
            if !collides {
                break;
            }
            candidate.y += STEP_Y;
        }
        placed.push(candidate);
        out.insert(id, candidate);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearby_ribbons_separate_vertically() {
        let a = ContextId::new();
        let b = ContextId::new();
        let c = ContextId::new();
        let pos = layout_ribbon_positions(&[
            (a, Vec3::new(0.0, 0.0, -5.0)),
            (b, Vec3::new(0.8, 0.0, -5.2)),
            (c, Vec3::new(-4.0, 0.0, -5.0)),
        ]);
        assert!((pos[&a].y - pos[&b].y).abs() >= 0.30);
        assert!((pos[&a].y - 0.76).abs() < 1e-4);
        assert!((pos[&c].y - 0.76).abs() < 1e-4);
    }
}
