//! Live kernel data driver for the heroic view (`docs/newui.md`, "The live driver").
//!
//! Polls `listTracks` and `kj transport work --track <id>` (with `execute_kj_quiet`
//! so polling never authors blocks) at ~1 Hz while the heroic view is active,
//! reads beat phasors from `WellBeatsRes` with `[heroic] display_offset_ms`,
//! joins attached contexts with `DriftState` and `ContextTails`, and derives
//! onset band energies from beat crossings and `RenderCue` events.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use bevy::prelude::*;
use kaijutsu_audio::RENDER_FLUSH_MIME;
use kaijutsu_client::ServerEvent;
use kaijutsu_hyoushigi::{Disposition, FallbackReason, WorkStatus};
use kaijutsu_present::work::compute_performer_slack;
use kaijutsu_types::ContextId;

use super::state::{
    ConstellationStar, HeroicLane, HeroicPerformer, HeroicState, PadFlashEvent,
};
use crate::connection::actor_plugin::{RpcActor, RpcResultChannel, RpcResultMessage, ServerEventMessage};
use crate::connection::drift::DriftState;
use crate::view::scene_palette::{HeroicPalette, heroic_identity_color};
use crate::view::time_well::live::{ContextTails, WellBeatsRes, tail_lines};
use crate::view::time_well::rays::WellTracks;

const LIVE_POLL_INTERVAL_SECS: f64 = 1.0;

/// Live poll cache for per-track `WorkStatus` records and onset energy state.
#[derive(Resource, Default)]
pub struct HeroicLiveCache {
    pub work_by_track: HashMap<String, Vec<WorkStatus>>,
    in_flight_tracks: HashMap<String, f64>,
    last_track_poll: f64,
    last_work_poll: f64,
    last_beat_by_track: HashMap<String, i64>,
}

/// Fire a one-shot track listing when entering `Screen::Heroic` so live
/// kernels populate without waiting a full second for the periodic poll.
pub fn request_initial_tracks(actor: Option<&RpcActor>, result_channel: &RpcResultChannel) {
    let Some(actor) = actor else {
        return;
    };
    let handle = actor.handle.clone();
    let tx = result_channel.sender();
    bevy::tasks::IoTaskPool::get()
        .spawn(async move {
            match handle.list_tracks().await {
                Ok(tracks) => {
                    let _ = tx.send(RpcResultMessage::TracksReceived { tracks });
                }
                Err(e) => log::debug!("heroic: initial list_tracks failed: {e}"),
            }
        })
        .detach();
}

/// Poll `listTracks` and `kj transport work --track <id>` while `Screen::Heroic`
/// is active (`execute_kj_quiet` so no blocks are authored).
pub fn poll_heroic_live_data(
    actor: Option<Res<RpcActor>>,
    tracks: Res<WellTracks>,
    time: Res<Time>,
    mut cache: ResMut<HeroicLiveCache>,
    result_channel: Res<RpcResultChannel>,
) {
    let Some(actor) = actor else {
        return;
    };
    let now = time.elapsed_secs_f64();

    if now - cache.last_track_poll >= LIVE_POLL_INTERVAL_SECS {
        cache.last_track_poll = now;
        request_initial_tracks(Some(&actor), &result_channel);
    }

    // Drop stale in-flight entries (> 5s) in case a task dropped without sending.
    cache.in_flight_tracks.retain(|_, started| now - *started < 5.0);

    if now - cache.last_work_poll >= LIVE_POLL_INTERVAL_SECS {
        cache.last_work_poll = now;
        for track in tracks.tracks.iter().filter(|t| t.playing) {
            if cache.in_flight_tracks.contains_key(&track.id) {
                continue;
            }
            cache.in_flight_tracks.insert(track.id.clone(), now);
            let handle = actor.handle.clone();
            let tx = result_channel.sender();
            let track_id = track.id.clone();
            let ctx_id = track.score_context_id;
            bevy::tasks::IoTaskPool::get()
                .spawn(async move {
                    let argv = vec![
                        "transport".to_string(),
                        "work".to_string(),
                        "--track".to_string(),
                        track_id.clone(),
                    ];
                    match handle.execute_kj_quiet(ctx_id, argv).await {
                        Ok(res) if res.exit_code == 0 => {
                            if let Some(data) = res.data {
                                match serde_json::from_value::<Vec<WorkStatus>>(data) {
                                    Ok(statuses) => {
                                        let _ = tx.send(RpcResultMessage::TrackWorkReceived {
                                            track_id,
                                            statuses,
                                        });
                                    }
                                    Err(e) => {
                                        log::debug!("heroic: decode work JSON for {track_id}: {e}");
                                    }
                                }
                            }
                        }
                        Ok(res) => {
                            log::debug!(
                                "heroic: transport work --track {track_id} exit {}: {}",
                                res.exit_code,
                                res.stderr
                            );
                        }
                        Err(e) => log::debug!("heroic: execute_kj_quiet failed: {e}"),
                    }
                })
                .detach();
        }
    }
}

/// Drain `TrackWorkReceived` into [`HeroicLiveCache`].
pub fn apply_heroic_work_results(
    mut events: MessageReader<RpcResultMessage>,
    mut cache: ResMut<HeroicLiveCache>,
) {
    for ev in events.read() {
        if let RpcResultMessage::TrackWorkReceived { track_id, statuses } = ev {
            cache.in_flight_tracks.remove(track_id);
            cache.work_by_track.insert(track_id.clone(), statuses.clone());
        }
    }
}

/// Ingest `RenderCue` onsets into `HeroicState::band_energies` and pad flashes
/// (`docs/newui.md`, "Band energies").
pub fn ingest_heroic_cues(
    mut events: MessageReader<ServerEventMessage>,
    tracks: Res<WellTracks>,
    palette: Res<HeroicPalette>,
    mut state: ResMut<HeroicState>,
) {
    for ServerEventMessage(ev) in events.read() {
        if let ServerEvent::RenderCue { context_id, cue } = ev {
            if cue.mime == RENDER_FLUSH_MIME {
                continue;
            }
            if let Some(track_id) = tracks.track_of.get(context_id)
                && let Some(li) = tracks
                    .tracks
                    .iter()
                    .filter(|t| t.playing)
                    .position(|t| &t.id == track_id)
            {
                let band = (li * 4).min(15);
                state.band_energies[band] = (state.band_energies[band] + 0.9).min(1.5);
                if band > 0 {
                    state.band_energies[band - 1] =
                        (state.band_energies[band - 1] + 0.45).min(1.5);
                }
                if band + 1 < 16 {
                    state.band_energies[band + 1] =
                        (state.band_energies[band + 1] + 0.45).min(1.5);
                }
                state.pending_flashes.push(PadFlashEvent {
                    lane_idx: li,
                    color: palette.now,
                    hollow: false,
                });
            }
        }
    }
}

/// Populate [`HeroicState`] from currently playing kernel tracks.
///
/// Dormant tracks (`playing == false`) are excluded by default. When no tracks
/// are playing, the road idles cleanly with no lanes while energy/history decay.
pub fn sync_heroic_state(
    time: Res<Time>,
    palette: Res<HeroicPalette>,
    tracks: Res<WellTracks>,
    beats: Res<WellBeatsRes>,
    drift: Res<DriftState>,
    tails: Res<ContextTails>,
    mut cache: ResMut<HeroicLiveCache>,
    mut state: ResMut<HeroicState>,
) {
    let playing_tracks: Vec<&kaijutsu_client::TrackInfo> =
        tracks.tracks.iter().filter(|t| t.playing).collect();
    let dt = time.delta_secs();
    let primary_bps = playing_tracks
        .first()
        .map(|t| (1_000_000.0 / t.period_us.max(1) as f32).max(0.25))
        .unwrap_or(2.0);
    state.tick_energy(dt, primary_bps);

    if playing_tracks.is_empty() {
        state.lanes.clear();
        return;
    }

    // Apply signed display offset to the visual clock (`docs/newui.md`, "Clock and latency").
    let now_inst = Instant::now();
    let shifted_inst = if palette.display_offset_ms >= 0 {
        now_inst + Duration::from_millis(palette.display_offset_ms as u64)
    } else {
        now_inst
            .checked_sub(Duration::from_millis((-palette.display_offset_ms) as u64))
            .unwrap_or(now_inst)
    };

    let ctx_map: HashMap<ContextId, &kaijutsu_client::ContextInfo> =
        drift.contexts.iter().map(|c| (c.id, c)).collect();

    let mut lanes = Vec::with_capacity(playing_tracks.len());
    for (li, track) in playing_tracks.into_iter().enumerate() {
        let period_secs = (track.period_us.max(1) as f32) / 1_000_000.0;
        let playhead_beats = if track.playing {
            beats
                .beat_position(&track.score_context_id, shifted_inst)
                .unwrap_or(track.playhead_tick as f64)
        } else {
            track.playhead_tick as f64
        };

        let beat_idx = playhead_beats.floor() as i64;
        let prev_beat = cache.last_beat_by_track.insert(track.id.clone(), beat_idx);
        if prev_beat.is_some_and(|p| beat_idx > p) {
            state.band_energies[0] = (state.band_energies[0] + 0.5).min(1.5);
            state.band_energies[1] = (state.band_energies[1] + 0.25).min(1.5);
        }

        let work = cache
            .work_by_track
            .get(&track.id)
            .cloned()
            .unwrap_or_default();

        let attached_ids: Vec<ContextId> = if track.attached.is_empty() {
            vec![track.score_context_id]
        } else {
            track.attached.clone()
        };

        let mut performers = Vec::with_capacity(attached_ids.len());
        for ctx_id in attached_ids {
            let info = ctx_map.get(&ctx_id).copied();
            let character = info
                .and_then(|c| c.performer.clone())
                .unwrap_or_else(|| track.id.clone());
            let label = info
                .map(|c| c.label.clone())
                .unwrap_or_else(|| track.id.clone());
            let parent_context_id = info.and_then(|c| c.forked_from);

            // Lead stand-in: newest record's `estimate` until the wire event carries measured lead.
            let lead_beats = work
                .iter()
                .max_by_key(|w| w.start)
                .map(|w| w.estimate.0.max(0) as f32)
                .unwrap_or(0.0);

            let slack = compute_performer_slack(
                &work,
                playhead_beats,
                track.beats_per_phrase.max(1),
                lead_beats,
                palette.alarm_beats,
            );

            // Emit trouble pad flash on newly settled fallback crossing.
            if prev_beat.is_some_and(|p| beat_idx > p)
                && let Some(rec) = work.iter().find(|r| r.start.0 == beat_idx)
            {
                let char_col = heroic_identity_color(&character);
                match &rec.disposition {
                    Some(Disposition::Fallback {
                        reason: FallbackReason::Requested,
                        ..
                    }) => state.pending_flashes.push(PadFlashEvent {
                        lane_idx: li,
                        color: char_col,
                        hollow: false,
                    }),
                    Some(Disposition::Fallback { content, .. }) => {
                        state.pending_flashes.push(PadFlashEvent {
                            lane_idx: li,
                            color: palette.trouble,
                            hollow: content.is_none(),
                        })
                    }
                    _ => {}
                }
            }

            let tail = tail_lines(&tails, ctx_id, 6, 48)
                .map(|s| s.lines().map(|l| l.to_string()).collect())
                .unwrap_or_default();

            performers.push(HeroicPerformer {
                context_id: ctx_id,
                parent_context_id,
                character,
                label,
                rc_dir: "musician".into(),
                statuses: work.clone(),
                lead_beats,
                slack,
                candidates: Vec::new(),
                recent_picks: Vec::new(),
                tail_lines: tail,
                instructions: Vec::new(),
                flare_remaining: 0.0,
                hop_from_phrase: None,
                hop_to_phrase: None,
                hop_elapsed: 0.0,
            });
        }

        lanes.push(HeroicLane {
            track_id: track.id.clone(),
            score_context_id: track.score_context_id,
            period_secs,
            beats_per_phrase: track.beats_per_phrase.max(1),
            playhead_beats,
            playing: track.playing,
            commit_margin_ticks: 1,
            performers,
        });
    }

    // Build optional sky constellation from DriftState contexts.
    state.constellation = drift
        .contexts
        .iter()
        .take(24)
        .enumerate()
        .map(|(i, c)| {
            let angle = (i as f32) * 0.65;
            let x = (angle.cos()) * (10.0 + (i as f32 % 4.0) * 4.5);
            let y = 12.0 + ((i as f32) * 1.1).sin().abs() * 10.0;
            let z = -38.0 - (i as f32 % 5.0) * 4.0;
            let active = tracks.track_of.contains_key(&c.id);
            ConstellationStar {
                context_id: c.id,
                character: c.performer.clone().unwrap_or_else(|| c.label.clone()),
                pos: Vec3::new(x, y, z),
                active,
            }
        })
        .collect();

    state.lanes = lanes;
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_client::TrackInfo;

    fn make_track(id: &str, playing: bool) -> TrackInfo {
        TrackInfo {
            id: id.into(),
            score_context_id: ContextId::new(),
            playing,
            playhead_tick: 0,
            period_us: 500_000,
            beats_per_phrase: 8,
            beat_count: 0,
            last_epoch_ns: 0,
            clock_kind: "system".into(),
            attached: Vec::new(),
        }
    }

    #[test]
    fn dormant_tracks_are_filtered_out_of_live_lanes() {
        let mut world = World::new();
        world.insert_resource(Time::<()>::default());
        world.insert_resource(HeroicPalette::default());
        let mut wt = WellTracks::default();
        wt.tracks = vec![make_track("dormant-smoke", false), make_track("live-bass", true)];
        world.insert_resource(wt);
        world.insert_resource(WellBeatsRes::default());
        world.insert_resource(DriftState::default());
        world.insert_resource(ContextTails::default());
        world.insert_resource(HeroicLiveCache::default());
        world.insert_resource(HeroicState::default());

        let mut schedule = Schedule::default();
        schedule.add_systems(sync_heroic_state);
        schedule.run(&mut world);

        let state = world.resource::<HeroicState>();
        assert_eq!(state.lanes.len(), 1);
        assert_eq!(state.lanes[0].track_id, "live-bass");
    }

    #[test]
    fn idle_kernel_leaves_lanes_empty() {
        let mut world = World::new();
        world.insert_resource(Time::<()>::default());
        world.insert_resource(HeroicPalette::default());
        let mut wt = WellTracks::default();
        wt.tracks = vec![make_track("dormant-only", false)];
        world.insert_resource(wt);
        world.insert_resource(WellBeatsRes::default());
        world.insert_resource(DriftState::default());
        world.insert_resource(ContextTails::default());
        world.insert_resource(HeroicLiveCache::default());
        let mut st = HeroicState::default();
        st.band_energies[0] = 1.0;
        world.insert_resource(st);

        let mut schedule = Schedule::default();
        schedule.add_systems(sync_heroic_state);
        schedule.run(&mut world);

        let state = world.resource::<HeroicState>();
        assert!(state.lanes.is_empty());
    }
}
