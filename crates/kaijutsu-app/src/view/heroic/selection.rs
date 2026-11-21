//! Performer selection, dark-glass detail panel, tether, and flat chips
//! (`docs/newui.md`, "Selection").
//!
//! - Keyboard: `StepNext` / `StepPrev` (`Left`/`Right`/`Tab`), `JumpSeat(n)` (`0-9`),
//!   and `PopLevel` (`Esc` once clears selection; `Esc` with no selection exits to
//!   `Screen::Conversation`).
//! - Unselected lanes dim to `[heroic] unselected_opacity`.
//! - Selected performer opens an opaque dark-glass MSDF panel on the opposite side
//!   of the road with a thin tether line to the crystal and flat bottom chips.

use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use kaijutsu_present::action::Action;
use peniko::Brush;

use super::performers::HeroicPerformerPool;
use super::state::HeroicState;
use crate::input::ActionFired;
use crate::shaders::WellCardMaterial;
use crate::text::ShapingFonts;
use crate::text::components::bevy_color_to_brush;
use crate::text::msdf::{FontDataMap, MsdfAtlas, MsdfBlockGlyphs, collect_msdf_glyphs};
use crate::text::shaping::{VelloFont, VelloTextAlign, VelloTextStyle};
use crate::ui::screen::Screen;
use crate::view::components::ContextSwitchRequested;
use crate::view::scene_palette::{HeroicPalette, heroic_identity_color};
use crate::view::time_well::panel::{commit_panel_glyphs, create_msdf_panel};

const PANEL_TEX_W: u32 = 540;
const PANEL_TEX_H: u32 = 400;
const PANEL_PAD: f32 = 14.0;

#[derive(Component)]
pub struct HeroicSelectionPanel;

#[derive(Component)]
pub struct HeroicSelectionTether;

#[derive(Component)]
pub struct HeroicChipsRoot;

#[derive(Resource, Default)]
pub struct HeroicSelectionRuntime {
    pub panel_entity: Option<Entity>,
    pub panel_mat: Option<Handle<WellCardMaterial>>,
    pub tether_entity: Option<Entity>,
    pub chips_root: Option<Entity>,
    last_panel_text: String,
    last_chip_sig: String,
}

/// Spawn the reusable opaque dark-glass detail panel, tether line, and bottom chip container.
pub fn spawn_selection_ui(
    commands: &mut Commands,
    root: Entity,
    images: &mut Assets<Image>,
    meshes: &mut Assets<Mesh>,
    card_mats: &mut Assets<WellCardMaterial>,
    std_mats: &mut Assets<StandardMaterial>,
    sel: &mut HeroicSelectionRuntime,
) {
    let (tex, panel_bundle) = create_msdf_panel(images, PANEL_TEX_W, PANEL_TEX_H);
    let mat = card_mats.add(WellCardMaterial {
        texture: tex,
        accent: Vec4::new(0.025, 0.022, 0.06, 0.94),
        params: Vec4::ZERO,
        shape: Vec4::new(PANEL_TEX_W as f32 / PANEL_TEX_H as f32, 0.06, 0.025, 0.0),
        border: Vec4::new(0.0, 0.8, 1.0, 0.8),
        dim: Vec4::new(0.85, 0.0, 0.0, 0.0),
    });
    let panel_entity = commands
        .spawn((
            HeroicSelectionPanel,
            panel_bundle,
            Mesh3d(meshes.add(Rectangle::new(1.98, 1.48))),
            MeshMaterial3d(mat.clone()),
            Transform::from_xyz(1.55, 1.38, -0.55),
            Visibility::Hidden,
            Name::new("HeroicSelectionPanel"),
            ChildOf(root),
        ))
        .id();

    let empty_tether = Mesh::new(PrimitiveTopology::LineList, RenderAssetUsages::default())
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, Vec::<[f32; 3]>::new())
        .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, Vec::<[f32; 4]>::new())
        .with_inserted_indices(Indices::U32(Vec::new()));
    let tether_mat = std_mats.add(StandardMaterial {
        base_color: Color::WHITE,
        unlit: true,
        ..default()
    });
    let tether_entity = commands
        .spawn((
            HeroicSelectionTether,
            Mesh3d(meshes.add(empty_tether)),
            MeshMaterial3d(tether_mat),
            Transform::default(),
            Visibility::Hidden,
            Name::new("HeroicSelectionTether"),
            ChildOf(root),
        ))
        .id();

    let chips_root = commands
        .spawn((
            HeroicChipsRoot,
            Node {
                position_type: PositionType::Absolute,
                // Elevated cleanly above the bottom HUD/dock mode strip (~28px tall).
                bottom: Val::Px(34.0),
                left: Val::Px(0.0),
                width: Val::Percent(100.0),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                column_gap: Val::Px(6.0),
                ..default()
            },
            Visibility::Inherited,
            Name::new("HeroicChipsRoot"),
        ))
        .id();

    sel.panel_entity = Some(panel_entity);
    sel.panel_mat = Some(mat);
    sel.tether_entity = Some(tether_entity);
    sel.chips_root = Some(chips_root);
    sel.last_panel_text.clear();
    sel.last_chip_sig.clear();
}

/// Handle `StepNext`, `StepPrev`, `JumpSeat(n)`, `Activate`, and `PopLevel` in `Screen::Heroic`.
pub fn heroic_keyboard(
    mut actions: MessageReader<ActionFired>,
    mut state: ResMut<HeroicState>,
    saved: Res<super::HeroicSavedCamera>,
    mut next_screen: ResMut<NextState<Screen>>,
    mut switch: MessageWriter<ContextSwitchRequested>,
) {
    let ids = state.performer_ids();
    let count = ids.len();
    for ActionFired { action, .. } in actions.read() {
        match action {
            Action::StepNext if count > 0 => {
                let next_idx = match state
                    .selected_performer
                    .and_then(|sel| ids.iter().position(|id| *id == sel))
                {
                    Some(i) => (i + 1) % count,
                    None => 0,
                };
                state.selected_performer = Some(ids[next_idx]);
            }
            Action::StepPrev if count > 0 => {
                let prev_idx = match state
                    .selected_performer
                    .and_then(|sel| ids.iter().position(|id| *id == sel))
                {
                    Some(i) => (i + count - 1) % count,
                    None => count - 1,
                };
                state.selected_performer = Some(ids[prev_idx]);
            }
            Action::JumpSeat(n) if *n < count => {
                state.selected_performer = Some(ids[*n]);
            }
            Action::Activate => {
                if let Some(ctx_id) = state.selected_performer
                    && ids.contains(&ctx_id)
                {
                    switch.write(ContextSwitchRequested { context_id: ctx_id });
                    next_screen.set(Screen::Conversation);
                }
            }
            Action::PopLevel => {
                if state.selected_performer.is_some() {
                    state.selected_performer = None;
                } else {
                    let target = saved
                        .return_screen
                        .filter(|s| *s != Screen::Heroic)
                        .unwrap_or(Screen::Conversation);
                    next_screen.set(target);
                }
            }
            _ => {}
        }
    }
}

fn format_panel_text(state: &HeroicState, lane_idx: usize, perf_idx: usize) -> String {
    use kaijutsu_hyoushigi::{Disposition, FallbackReason, Readiness};

    let lane = &state.lanes[lane_idx];
    let perf = &lane.performers[perf_idx];
    let slack_lbl = format!("{:?}", perf.slack.state);
    let tail = perf
        .tail_lines
        .iter()
        .rev()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    let rc_line = if perf.rc_dir.is_empty() {
        "-"
    } else {
        &perf.rc_dir
    };
    let picks = if perf.recent_picks.is_empty() {
        "-".to_string()
    } else {
        perf.recent_picks.join(", ")
    };

    // Last 16 phrases disposition history strip.
    let mut history_strip = String::new();
    for st in perf.statuses.iter().rev().take(16).rev() {
        let ch = match &st.disposition {
            Some(Disposition::Committed { .. }) => 'C',
            Some(Disposition::Fallback {
                reason: FallbackReason::Requested,
                ..
            }) => 'R',
            Some(Disposition::Fallback { .. }) => '!',
            Some(Disposition::Cancelled | Disposition::Superseded { .. }) => 'x',
            None => match st.readiness {
                Readiness::Failed => '?',
                Readiness::Ready => '+',
                Readiness::Running => '~',
                Readiness::Queued => '.',
            },
        };
        history_strip.push(ch);
    }
    if history_strip.is_empty() {
        history_strip.push('-');
    }

    // Latest timing row (estimate / queue / compute in ms).
    let timing_row = perf
        .statuses
        .iter()
        .rev()
        .find(|s| s.timing.is_some())
        .map(|s| {
            let t = s.timing.unwrap();
            format!(
                "{:.0}/{:.0}/{:.0}ms",
                s.estimate_wall.as_secs_f64() * 1000.0,
                t.queued.as_secs_f64() * 1000.0,
                t.compute.as_secs_f64() * 1000.0,
            )
        })
        .unwrap_or_else(|| "-".to_string());

    // Candidate probability bars (if present).
    let cands_block = if perf.candidates.is_empty() {
        String::new()
    } else {
        let mut lines = Vec::new();
        for c in perf.candidates.iter().take(4) {
            let filled = ((c.probability.clamp(0.0, 1.0) * 8.0).round() as usize).min(8);
            let bar = format!("{}{}", "#".repeat(filled), ".".repeat(8 - filled));
            let mark = if c.picked { "*" } else { " " };
            lines.push(format!(
                "  {mark}{:<6} {bar} {:>2.0}%",
                c.label,
                c.probability * 100.0
            ));
        }
        format!("CANDIDATES\n{}\n", lines.join("\n"))
    };

    format!(
        "PERFORMER  {:<18}\n\
         TRACK      {:<18}\n\
         RC DIR     {:<18}\n\
         LEAD       {:>5.2} beats\n\
         SLACK      {:<9} ({:.1}/{:.1}b)\n\
         TIMING     {:<18}\n\
         PHRASES    [{}]\n\
         PICKS      {}\n\
         {cands_block}--------------------------------\n\
         {}",
        perf.display_name(),
        lane.track_id,
        rc_line,
        perf.lead_beats,
        slack_lbl,
        perf.slack.slack_beats,
        perf.slack.threshold_beats,
        timing_row,
        history_strip,
        picks,
        tail
    )
}

/// Update dark-glass performer detail panel, crystal tether, and flat bottom chips.
#[allow(clippy::too_many_arguments)]
pub fn sync_selection_ui(
    mut commands: Commands,
    palette: Res<HeroicPalette>,
    state: Res<HeroicState>,
    pool: Res<HeroicPerformerPool>,
    mut sel: ResMut<HeroicSelectionRuntime>,
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<ShapingFonts>,
    mut atlas: Option<ResMut<MsdfAtlas>>,
    mut font_data_map: ResMut<FontDataMap>,
    mut card_mats: ResMut<Assets<WellCardMaterial>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut panel_q: Query<
        (&mut Transform, &mut Visibility, &mut MsdfBlockGlyphs),
        With<HeroicSelectionPanel>,
    >,
    mut tether_q: Query<(&Mesh3d, &mut Visibility), (With<HeroicSelectionTether>, Without<HeroicSelectionPanel>)>,
    children_q: Query<&Children>,
) {
    let selected = state.selected_location().and_then(|(li, pi)| {
        let perf = state.lanes.get(li)?.performers.get(pi)?;
        Some((li, pi, perf))
    });

    if let Some(panel_e) = sel.panel_entity
        && let Ok((mut tf, mut vis, mut msdf)) = panel_q.get_mut(panel_e)
    {
        if let Some((li, pi, perf)) = selected {
            *vis = Visibility::Inherited;
            let lane_x = state.lane_center_x(li, palette.lane_width);
            let road_w = state.road_width(palette.lane_width);
            let sign = if lane_x >= 0.0 { -1.0 } else { 1.0 };
            // Place high in the upper-left / upper-right sky/flank opposite the selected lane.
            let panel_w = palette.sizes.panel_size[0].max(0.5);
            let panel_h = palette.sizes.panel_size[1].max(0.5);
            let yaw = palette.sizes.panel_yaw_degrees.to_radians();
            let panel_x = sign * (road_w * 0.42).clamp(1.55, 2.25);
            let panel_pos = Vec3::new(panel_x, 2.02, -2.20);
            tf.translation = panel_pos;
            tf.rotation = Quat::from_rotation_y(-sign * yaw);
            tf.scale = Vec3::new(panel_w / 1.98, panel_h / 1.48, 1.0);

            let perf_col = heroic_identity_color(&perf.character);
            if let Some(mat_h) = &sel.panel_mat
                && let Some(mut m) = card_mats.get_mut(mat_h)
            {
                m.accent = Vec4::new(0.025, 0.022, 0.06, 0.94);
                m.border = Vec4::new(perf_col.red, perf_col.green, perf_col.blue, 0.9);
            }

            let text = format_panel_text(&state, li, pi);
            if text != sel.last_panel_text
                && let Some(font) = fonts.get(&font_handles.mono)
                && let Some(at) = atlas.as_deref_mut()
            {
                let brush: Brush = bevy_color_to_brush(Color::srgba(0.92, 0.96, 1.0, 0.96));
                let layout = font.layout(
                    &text,
                    &VelloTextStyle {
                        font_size: 18.0,
                        line_height: 1.25,
                        brush: brush.clone(),
                        ..default()
                    },
                    VelloTextAlign::Left,
                    Some(PANEL_TEX_W as f32 - 2.0 * PANEL_PAD),
                );
                for line in layout.lines() {
                    for item in line.items() {
                        if let parley::PositionedLayoutItem::GlyphRun(gr) = item {
                            font_data_map.register(gr.run().font());
                        }
                    }
                }
                let glyphs = collect_msdf_glyphs(
                    &layout,
                    &[],
                    &brush,
                    (PANEL_PAD as f64, PANEL_PAD as f64),
                    at,
                );
                commit_panel_glyphs(&mut msdf, glyphs);
                sel.last_panel_text = text;
            }

            // Update tether line from panel inner edge to crystal.
            if let Some(te) = sel.tether_entity
                && let Ok((mesh_h, mut t_vis)) = tether_q.get_mut(te)
            {
                if let Some(slot) = pool.slots.get(&perf.context_id)
                    && let Some(mut mesh) = meshes.get_mut(&mesh_h.0)
                {
                    *t_vis = Visibility::Inherited;
                    let start = panel_pos + Vec3::new(-sign * (panel_w * 0.46), -panel_h * 0.36, 0.0);
                    let end = slot.world_pos;
                    let c = [perf_col.red, perf_col.green, perf_col.blue, 0.9];
                    *mesh = Mesh::new(PrimitiveTopology::LineList, RenderAssetUsages::default())
                        .with_inserted_attribute(
                            Mesh::ATTRIBUTE_POSITION,
                            vec![start.to_array(), end.to_array()],
                        )
                        .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, vec![c, c])
                        .with_inserted_indices(Indices::U32(vec![0, 1]));
                } else {
                    *t_vis = Visibility::Hidden;
                }
            }
        } else {
            *vis = Visibility::Hidden;
            if let Some(te) = sel.tether_entity
                && let Ok((_, mut t_vis)) = tether_q.get_mut(te)
            {
                *t_vis = Visibility::Hidden;
            }
        }
    }

    // Flat bottom performer chips rebuilt only when performer list or selection changes.
    let mut sig = format!("{:?}:", state.selected_performer);
    for lane in &state.lanes {
        for p in &lane.performers {
            sig.push_str(&format!("{}:{};", p.display_name(), p.character));
        }
    }
    if sig != sel.last_chip_sig
        && let Some(chips_root) = sel.chips_root
    {
        sel.last_chip_sig = sig;
        if let Ok(kids) = children_q.get(chips_root) {
            for k in kids.iter() {
                commands.entity(k).despawn();
            }
        }
        let mut flat = 0usize;
        for lane in &state.lanes {
            for perf in &lane.performers {
                let is_sel = state.selected_performer == Some(perf.context_id);
                let c: Srgba = heroic_identity_color(&perf.character).into();
                let bg = if is_sel {
                    Color::srgba(0.14, 0.14, 0.28, 0.94)
                } else {
                    Color::srgba(0.05, 0.05, 0.12, 0.78)
                };
                let border = if is_sel {
                    Color::srgb(c.red, c.green, c.blue)
                } else {
                    Color::srgba(c.red, c.green, c.blue, 0.45)
                };
                let label = format!("[{flat}] {}", perf.display_name());
                let chip = commands
                    .spawn((
                        Node {
                            padding: UiRect::axes(Val::Px(8.0), Val::Px(3.0)),
                            border: UiRect::all(Val::Px(1.5)),
                            border_radius: BorderRadius::all(Val::Px(5.0)),
                            ..default()
                        },
                        BackgroundColor(bg),
                        BorderColor::all(border),
                        Text::new(label),
                        TextFont {
                            font_size: FontSize::Px(12.0),
                            ..default()
                        },
                        TextColor(Color::srgb(0.92, 0.95, 1.0)),
                        ChildOf(chips_root),
                    ))
                    .id();
                let _ = chip;
                flat += 1;
            }
        }
    }
}
