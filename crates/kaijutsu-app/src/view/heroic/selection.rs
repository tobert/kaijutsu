//! Performer selection, dark-glass detail panel, and tether
//! (`docs/newui.md`, "Selection").
//!
//! - Keyboard: `StepNext` / `StepPrev` (`Left`/`Right`/`Tab`), `JumpSeat(n)` (`0-9`),
//!   and `PopLevel` (`Esc` once clears selection; `Esc` with no selection exits to
//!   `Screen::Conversation`).
//! - Unselected lanes dim to `[heroic] unselected_opacity`.
//! - Selected performer opens an opaque dark-glass MSDF panel on the opposite side
//!   of the road with a thin tether line to the crystal.

use std::sync::{mpsc, Mutex};

use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use kaijutsu_cas::ContentHash;
use kaijutsu_client::SftpClient;
use kaijutsu_hyoushigi::{Disposition, WorkStatus};
use kaijutsu_present::action::Action;
use kaijutsu_types::Tick;
use peniko::Brush;

use super::performers::HeroicPerformerPool;
use super::state::HeroicState;
use crate::connection::actor_plugin::RpcActor;
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

#[derive(Resource, Default)]
pub struct HeroicSelectionRuntime {
    pub panel_entity: Option<Entity>,
    pub panel_mat: Option<Handle<WellCardMaterial>>,
    pub tether_entity: Option<Entity>,
    last_panel_text: String,
    score_request: Option<ContentHash>,
    score_receiver: Option<Mutex<mpsc::Receiver<Option<ScoreDetails>>>>,
    score_excerpt: Option<ScoreDetails>,
    score_retry_at: f64,
    score_attempts: u8,
}

/// Spawn the reusable detail panel and tether.
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

    sel.panel_entity = Some(panel_entity);
    sel.panel_mat = Some(mat);
    sel.tether_entity = Some(tether_entity);
    sel.last_panel_text.clear();
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
    for ActionFired { action, context } in actions.read() {
        if !matches!(context, crate::input::InputContext::Heroic | crate::input::InputContext::Global) {
            continue;
        }
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

#[derive(Debug, Clone, PartialEq)]
struct ScoreDetails {
    excerpt: String,
    midi: String,
}

fn midi_summary(tune: &kaijutsu_abc::Tune) -> String {
    let events = kaijutsu_abc::midi::events(tune, &kaijutsu_abc::MidiParams::default());
    let notes: Vec<_> = events.iter().filter_map(|event| match event.data.as_slice() {
        [status, pitch, velocity, ..] if status & 0xf0 == 0x90 && *velocity != 0 => {
            Some((status & 0x0f, *pitch))
        }
        _ => None,
    }).collect();
    let Some(&(channel, _)) = notes.first() else {
        return "rest".into();
    };
    if channel == 9 {
        let mut drums: Vec<_> = notes.iter().map(|&(_, pitch)| pitch).collect();
        drums.sort_unstable();
        drums.dedup();
        let names = drums.into_iter().map(drum_name).collect::<Vec<_>>().join(", ");
        return format!("ch 10 | {} hits | {names}", notes.len());
    }

    let pitch_name = |pitch: u8| {
        let names = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
        format!("{}{}", names[(pitch % 12) as usize], pitch as i16 / 12 - 1)
    };
    let low = notes.iter().map(|&(_, pitch)| pitch).min().unwrap();
    let high = notes.iter().map(|&(_, pitch)| pitch).max().unwrap();
    let range = if low == high { pitch_name(low) } else { format!("{}-{}", pitch_name(low), pitch_name(high)) };

    // A multitrack tune without a channel directive assigns channels by voice.
    let channel_label = if tune.header.midi_channel.is_some()
        || tune.voices.iter().filter(|voice| !voice.elements.is_empty()).count() <= 1
    {
        format!("ch {} | ", channel + 1)
    } else {
        String::new()
    };
    format!("{channel_label}{} notes | {range}", notes.len())
}

fn drum_name(pitch: u8) -> String {
    match pitch {
        35 | 36 => "kick".into(),
        38 | 40 => "snare".into(),
        42 => "closed hi-hat".into(),
        44 => "pedal hi-hat".into(),
        45 => "low tom".into(),
        46 => "open hi-hat".into(),
        47 => "low-mid tom".into(),
        48 => "high-mid tom".into(),
        _ => format!("note {pitch}"),
    }
}

pub(super) fn verified_abc<'a>(bytes: &'a [u8], hash: &ContentHash) -> Result<&'a str, &'static str> {
    if ContentHash::from_data(bytes) != *hash {
        return Err("content hash mismatch");
    }
    std::str::from_utf8(bytes).map_err(|_| "invalid UTF-8")
}

fn score_from_bytes(bytes: &[u8], hash: &ContentHash) -> Result<ScoreDetails, &'static str> {
    let abc = verified_abc(bytes, hash)?;
    let parsed = kaijutsu_abc::parse_with_mode(abc, kaijutsu_abc::ParseMode::Strict);
    if parsed.has_errors() {
        return Err("invalid ABC");
    }
    let tune = parsed.value.first().ok_or("no ABC tune")?;
    let body = abc.lines().map(str::trim).filter(|line| {
        let bytes = line.as_bytes();
        !line.is_empty() && !line.starts_with('%')
            && !(bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
    }).collect::<Vec<_>>().join(" ");
    let excerpt = if body.is_empty() {
        abc.split_whitespace().collect::<Vec<_>>().join(" ")
    } else {
        body
    };
    Ok(ScoreDetails { excerpt, midi: midi_summary(tune) })
}

fn board_note_argv(track: &str, performer: kaijutsu_types::PrincipalId, at: i64, score: Option<(Tick, &ContentHash)>) -> Vec<String> {
    let mut args = vec!["board".into(), "add-note".into(), "--track".into(), track.into(),
        "--performer".into(), performer.to_string(), "--at".into(), at.to_string()];
    if let Some((start, hash)) = score {
        args.extend(["--phrase-start".into(), start.0.to_string(), "--score".into(), hash.to_string()]);
    }
    args.push("--vi".into());
    args
}

pub fn open_board_note(
    mut actions: MessageReader<ActionFired>,
    state: Res<HeroicState>,
    actor: Option<Res<RpcActor>>,
    documents: Res<crate::view::DocumentCache>,
    results: Res<crate::connection::actor_plugin::RpcResultChannel>,
) {
    for event in actions.read() {
        if !matches!(event.action, Action::HeroicBoardNote) { continue; }
        let Some((li, pi)) = state.selected_location() else { continue; };
        let lane = &state.lanes[li];
        let performer = &lane.performers[pi];
        let (Some(principal), Some(actor), Some(context)) = (performer.principal_id, actor.as_ref(), documents.active_id()) else {
            let _ = results.sender().send(crate::connection::actor_plugin::RpcResultMessage::RpcError {
                operation: "board note".into(),
                error: "performer identity or connection is unavailable".into(),
            });
            continue;
        };
        let at = lane.playhead_beats.floor() as i64;
        let argv = board_note_argv(&lane.track_id, principal, at, score_at_playhead(&performer.statuses, lane.playhead_beats));
        let handle = actor.handle.clone();
        let tx = results.sender();
        bevy::tasks::IoTaskPool::get().spawn(async move {
            let message = match handle.execute_kj(context, argv).await {
                Ok(result) => crate::connection::actor_plugin::RpcResultMessage::KjLineFinished { from: context, result },
                Err(e) => crate::connection::actor_plugin::RpcResultMessage::RpcError { operation: "board note".into(), error: e.to_string() },
            };
            let _ = tx.send(message);
        }).detach();
    }
}

pub(super) fn score_at_playhead<'a>(
    statuses: impl IntoIterator<Item = &'a WorkStatus>,
    playhead: f64,
) -> Option<(Tick, &'a ContentHash)> {
    let latest = statuses.into_iter()
        .filter(|status| status.start.0 <= playhead.floor() as i64)
        .filter(|status| matches!(
            status.disposition,
            Some(Disposition::Committed { .. } | Disposition::Fallback { .. })
        ))
        .max_by_key(|status| status.start)?;
    let content = match &latest.disposition {
        Some(Disposition::Committed { content }) => content,
        Some(Disposition::Fallback { content: Some(content), .. }) => content,
        _ => return None,
    };
    (content.mime == "text/vnd.abc").then_some((latest.start, &content.hash))
}

fn update_score_excerpt(
    sel: &mut HeroicSelectionRuntime,
    request: Option<&ContentHash>,
    actor: Option<&RpcActor>,
    now: f64,
) {
    if request != sel.score_request.as_ref() {
        sel.score_request = request.cloned();
        sel.score_receiver = None;
        sel.score_excerpt = None;
        sel.score_retry_at = 0.0;
        sel.score_attempts = 0;
    }
    if let Some(receiver) = &sel.score_receiver {
        let result = receiver.lock().unwrap().try_recv();
        match result {
            Ok(excerpt) => {
                sel.score_excerpt = excerpt;
                sel.score_receiver = None;
            }
            Err(mpsc::TryRecvError::Disconnected) => sel.score_receiver = None,
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }
    if sel.score_excerpt.is_none() && sel.score_receiver.is_none() && sel.score_attempts < 3
        && now >= sel.score_retry_at
        && let (Some(hash), Some(actor)) = (request, actor)
    {
        sel.score_retry_at = now + 2.0;
        sel.score_attempts += 1;
        let handle = actor.handle.clone();
        let hash = hash.clone();
        let (tx, rx) = mpsc::channel();
        sel.score_receiver = Some(Mutex::new(rx));
        bevy::tasks::IoTaskPool::get().spawn(async move {
            let excerpt = match handle.vfs_read_all(SftpClient::object_path(&hash)).await {
                Ok(bytes) => score_from_bytes(&bytes, &hash).map_err(str::to_owned),
                Err(err) => Err(err.to_string()),
            };
            if let Err(err) = &excerpt {
                log::debug!("heroic: could not load score {hash}: {err}");
            }
            let _ = tx.send(excerpt.ok());
        }).detach();
    }
}

fn format_panel_text(state: &HeroicState, lane_idx: usize, perf_idx: usize, score_tick: Option<Tick>, score: Option<&ScoreDetails>) -> String {
    use kaijutsu_hyoushigi::{FallbackReason, Readiness};

    let lane = &state.lanes[lane_idx];
    let perf = &lane.performers[perf_idx];
    let slack_lbl = format!("{:?}", perf.slack.state);
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

    let timing_row = perf.statuses.iter().rev().find_map(|s| {
        let timing = s.timing?;
        Some(format!(
            "\nModel  {:.1}s compute · {:.1}s queued · {:.1}s est.",
            timing.compute.as_secs_f64(), timing.queued.as_secs_f64(), s.estimate_wall.as_secs_f64(),
        ))
    }).unwrap_or_default();

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

    let chair = perf.lane_name();
    let bpm = 60.0 / lane.period_secs;
    let mut text = format!("{chair}  ·  {}  ·  {bpm:.0} BPM\n", lane.track_id);
    if let Some(tick) = score_tick {
        text.push_str(&format!("\nABC  ·  beat {}\n", tick.0));
        if let Some(score) = score {
            text.push_str(&crate::text::truncate_chars(&score.excerpt, 74));
            text.push_str(&format!("\nMIDI  {}\n", score.midi));
        } else {
            text.push_str("Score unavailable\n");
        }
    } else {
        text.push_str("\nNo ABC at this beat\n");
    }
    text.push_str(&format!(
        "\nPhrases  {history_strip}\nSlack  {slack_lbl} · {:.0} beats · lead {:.0}{timing_row}",
        perf.slack.slack_beats, perf.lead_beats,
    ));
    if !perf.recent_picks.is_empty() {
        text.push_str(&format!("\nPicked  {}", perf.recent_picks.join(", ")));
    }
    if !cands_block.is_empty() {
        text.push('\n');
        text.push_str(cands_block.trim_end());
    } else if let Some(line) = perf.tail_lines.last() {
        text.push_str(&format!("\n\n{}", crate::text::truncate_chars(line, 56)));
    }
    text

}

/// Update the performer detail panel and crystal tether.
#[allow(clippy::too_many_arguments)]
pub fn sync_selection_ui(
    palette: Res<HeroicPalette>,
    state: Res<HeroicState>,
    actor: Option<Res<RpcActor>>,
    time: Res<Time>,
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
) {
    let selected = state.selected_location().and_then(|(li, pi)| {
        let lane = state.lanes.get(li)?;
        let perf = lane.performers.get(pi)?;
        Some((li, pi, perf))
    });
    let score = selected.and_then(|(li, _, perf)| score_at_playhead(&perf.statuses, state.lanes[li].playhead_beats));
    update_score_excerpt(&mut sel, score.map(|(_, hash)| hash), actor.as_deref(), time.elapsed_secs_f64());

    if let Some(panel_e) = sel.panel_entity
        && let Ok((mut tf, mut vis, mut msdf)) = panel_q.get_mut(panel_e)
    {
        if let Some((li, pi, perf)) = selected {
            *vis = Visibility::Inherited;
            let lane_x = state.performer_lane_center_x(li, pi, palette.lane_width);
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

            let perf_col = heroic_identity_color(perf.lane_name());
            if let Some(mat_h) = &sel.panel_mat
                && let Some(mut m) = card_mats.get_mut(mat_h)
            {
                m.accent = Vec4::new(0.025, 0.022, 0.06, 0.94);
                m.border = Vec4::new(perf_col.red, perf_col.green, perf_col.blue, 0.9);
            }

            let text = format_panel_text(&state, li, pi, score.map(|(tick, _)| tick), sel.score_excerpt.as_ref());
            if text != sel.last_panel_text
                && let Some(font) = fonts.get(&font_handles.mono)
                && let Some(at) = atlas.as_deref_mut()
            {
                let brush: Brush = bevy_color_to_brush(Color::srgba(0.92, 0.96, 1.0, 0.96));
                let layout = font.layout(
                    &text,
                    &VelloTextStyle {
                        font_size: 19.5,
                        line_height: 1.18,
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

}

#[cfg(test)]
mod score_tests {
    use super::*;
    #[test]
    fn boards_escape_cannot_clear_the_returning_heroic_seat() {
        let mut app = App::new();
        let selected = Some(kaijutsu_types::ContextId::new());
        app.insert_resource(HeroicState { selected_performer: selected, ..default() })
            .init_resource::<super::super::HeroicSavedCamera>()
            .init_resource::<NextState<Screen>>()
            .add_message::<ActionFired>()
            .add_message::<ContextSwitchRequested>()
            .add_systems(Update, heroic_keyboard);
        app.world_mut().write_message(ActionFired::new(Action::PopLevel, crate::input::InputContext::Boards));
        app.update();
        assert_eq!(app.world().resource::<HeroicState>().selected_performer, selected);
    }

    #[test]
    fn board_note_arguments_include_selected_phrase() {
        use kaijutsu_types::PrincipalId;
        let principal = PrincipalId::new();
        let hash = ContentHash::from_data(b"score");
        let args = board_note_argv("ensemble", principal, 12, Some((Tick(8), &hash)));
        assert_eq!(args, vec!["board", "add-note", "--track", "ensemble", "--performer", &principal.to_string(), "--at", "12", "--phrase-start", "8", "--score", &hash.to_string(), "--vi"]);
    }

    #[test]
    fn board_note_without_phrase_omits_score_flags() {
        let principal = kaijutsu_types::PrincipalId::new();
        assert_eq!(board_note_argv("main", principal, 3, None), vec![
            "board", "add-note", "--track", "main", "--performer", &principal.to_string(), "--at", "3", "--vi",
        ]);
    }

    fn summarize(abc: &str) -> String {
        let parsed = kaijutsu_abc::parse(abc);
        assert!(!parsed.has_errors());
        midi_summary(&parsed.value[0])
    }

    #[test]
    fn midi_summary_reports_channel_and_sounding_range() {
        let abc = "X:1\n%%MIDI channel 3\n%%MIDI transpose -12\nK:C\nC E G|\n";
        assert_eq!(summarize(abc), "ch 3 | 3 notes | C3-G3");
    }

    #[test]
    fn midi_summary_reports_rests() {
        assert_eq!(summarize("X:1\nK:C\nz4|\n"), "rest");
    }

    #[test]
    fn midi_summary_names_drum_hits_on_channel_ten() {
        let abc = "X:1\n%%MIDI channel 10\nK:C\n[C,,^F,,] D,,|\n";
        assert_eq!(summarize(abc), "ch 10 | 3 hits | kick, snare, closed hi-hat");
    }

    #[test]
    fn midi_summary_names_the_three_toms_used_in_fills() {
        let abc = "X:1\n%%MIDI channel 10\nK:C\nA,, B,, C,|\n";
        assert_eq!(summarize(abc), "ch 10 | 3 hits | low tom, low-mid tom, high-mid tom");
    }

    #[test]
    fn newer_settled_non_abc_content_clears_the_previous_score_at_its_beat() {
        use kaijutsu_hyoushigi::{ContentRef, Fallback, FallbackReason, Readiness, WorkId};
        use kaijutsu_types::{PrincipalId, TickDelta, TrackId};
        use std::time::Duration;

        let status = |start, disposition| WorkStatus {
            id: WorkId(uuid::Uuid::nil()),
            track: TrackId::new("ensemble").unwrap(),
            played_by: PrincipalId::beat(),
            start: Tick(start),
            admitted_at: Tick(start - 1),
            estimate: TickDelta::new(1),
            estimate_wall: Duration::from_secs(1),
            prepare_at: Tick(start - 1),
            attempt: 0,
            started_at: None,
            ready_at: None,
            readiness: Readiness::Ready,
            predicted: None,
            actual: None,
            valid: None,
            error: None,
            timing: None,
            settled_at: Some(Tick(start)),
            disposition: Some(disposition),
        };
        let score = ContentRef::of(b"X:1\nK:C\nC|", "text/vnd.abc");
        let first = status(4, Disposition::Committed { content: score.clone() });
        let silence = Disposition::Fallback {
            reason: FallbackReason::Requested,
            policy: Fallback::Skip,
            content: None,
        };
        let midi = Disposition::Committed { content: ContentRef::of(b"midi", "audio/midi") };

        for disposition in [silence, midi] {
            let statuses = [first.clone(), status(8, disposition)];
            assert_eq!(score_at_playhead(&statuses, 7.9), Some((Tick(4), &score.hash)));
            assert_eq!(score_at_playhead(&statuses, 8.0), None);
        }
    }

    #[test]
    fn score_bytes_must_match_the_hash_and_decode_as_utf8() {
        let abc = "X:1\nT:Phrase\n% comment\nK:C\nCDEF|\nGABc|";
        let hash = ContentHash::from_data(abc.as_bytes());
        assert_eq!(
            score_from_bytes(abc.as_bytes(), &hash),
            Ok(ScoreDetails { excerpt: "CDEF| GABc|".into(), midi: summarize(abc) }),
        );
        assert!(score_from_bytes(b"other", &hash).is_err());
        let invalid_utf8 = b"\xff";
        assert!(score_from_bytes(invalid_utf8, &ContentHash::from_data(invalid_utf8)).is_err());
    }
}
