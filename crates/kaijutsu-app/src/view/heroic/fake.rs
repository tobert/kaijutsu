//! Deterministic fake data driver (`docs/newui.md`, "The fake driver").
//!
//! Produces plausible multi-track `WorkStatus` streams, candidate choices,
//! streaming text ribbons, instruction markers, and onset band energies from a
//! fixed seed so every screenshot and test run is reproducible and always
//! includes a struggling performer entering alarm + missing a deadline.

use std::collections::HashMap;
use std::time::Duration;

use bevy::prelude::*;
use kaijutsu_hyoushigi::{
    ContentRef, Disposition, Fallback, FallbackReason, Readiness, Timing, WorkId, WorkStatus,
};
use kaijutsu_present::work::compute_performer_slack;
use kaijutsu_types::{ContextId, PrincipalId, Tick, TickDelta, TrackId};

use super::state::{
    CandidateChoice, ConstellationStar, HeroicInstruction, HeroicLane, HeroicPerformer,
    HeroicState, PadFlashEvent,
};
use crate::view::scene_palette::{HeroicPalette, heroic_identity_color};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FakeMode {
    Write,
    Revise,
}

#[derive(Debug, Clone)]
struct FakeTrackSpec {
    track_id: &'static str,
    character: &'static str,
    rc_dir: &'static str,
    base_speed: f64,
    density: f64,
    period_secs: f32,
    beats_per_phrase: u64,
    is_decision: bool,
    lead_beats: f32,
}

const FAKE_SPECS: [FakeTrackSpec; 4] = [
    FakeTrackSpec {
        track_id: "bass",
        character: "omni-bass",
        rc_dir: "musician",
        base_speed: 3.4,
        density: 0.68,
        period_secs: 0.5,
        beats_per_phrase: 8,
        is_decision: true,
        lead_beats: 4.0,
    },
    FakeTrackSpec {
        track_id: "drums",
        character: "drummer",
        rc_dir: "musician",
        base_speed: 3.9,
        density: 0.74,
        period_secs: 0.5,
        beats_per_phrase: 8,
        is_decision: false,
        lead_beats: 4.0,
    },
    FakeTrackSpec {
        track_id: "synth",
        character: "synth-pad",
        rc_dir: "musician",
        base_speed: 2.5, // struggling performer: triggers alarm and deadline misses
        density: 0.55,
        period_secs: 0.5,
        beats_per_phrase: 8,
        is_decision: false,
        lead_beats: 6.0,
    },
    FakeTrackSpec {
        track_id: "lead",
        character: "lead-vox",
        rc_dir: "musician",
        base_speed: 3.2,
        density: 0.45,
        period_secs: 0.5,
        beats_per_phrase: 8,
        is_decision: false,
        lead_beats: 3.5,
    },
];

struct FakePerformerSim {
    spec: FakeTrackSpec,
    context_id: ContextId,
    parent_context_id: Option<ContextId>,
    principal_id: PrincipalId,
    now_beats: f64,
    write_head: f64,
    mode: FakeMode,
    revise_start: f64,
    records: HashMap<i64, WorkStatus>,
    last_played_phrase: i64,
    last_beat_crossed: i64,
    instructions: Vec<HeroicInstruction>,
    next_instruction_at: f64,
    flare_remaining: f32,
    hop_from_phrase: Option<i64>,
    hop_to_phrase: Option<i64>,
    hop_elapsed: f32,
}

/// Deterministic simulation driver for the heroic view.
#[derive(Resource)]
pub struct FakeDriver {
    seed: u64,
    elapsed_secs: f64,
    sims: Vec<FakePerformerSim>,
    next_instr_id: u64,
}

impl Default for FakeDriver {
    fn default() -> Self {
        Self::with_seed(0x4B41_494A_5554_5355)
    }
}

fn hash01(seed: u64, a: i64, b: i64) -> f64 {
    let mut x = seed
        ^ (a as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (b as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    ((x >> 11) as f64) / ((1u64 << 53) as f64)
}

fn context_id_for(seed: u64, idx: usize) -> ContextId {
    let mut bytes = [0u8; 16];
    let v0 = seed.wrapping_add(idx as u64 + 1).to_le_bytes();
    let v1 = seed.wrapping_mul(0x517C_C1B7_2722_0A95).wrapping_add(idx as u64).to_le_bytes();
    bytes[..8].copy_from_slice(&v0);
    bytes[8..].copy_from_slice(&v1);
    ContextId::from_bytes(bytes)
}

impl FakeDriver {
    pub fn with_seed(seed: u64) -> Self {
        let bass_ctx = context_id_for(seed, 0);
        let sims = FAKE_SPECS
            .iter()
            .enumerate()
            .map(|(i, spec)| {
                let ctx = context_id_for(seed, i);
                let mut pid_bytes = [0u8; 16];
                pid_bytes[0] = (i + 1) as u8;
                pid_bytes[1..9].copy_from_slice(&seed.to_le_bytes());
                FakePerformerSim {
                    spec: spec.clone(),
                    context_id: ctx,
                    // Give `lead` lineage from `bass` so crystal resemblance is testable.
                    parent_context_id: if i == 3 { Some(bass_ctx) } else { None },
                    principal_id: PrincipalId::from_bytes(pid_bytes),
                    now_beats: 0.0,
                    // Start struggling performer (`synth`, i==2) tight on slack,
                    // others comfortably ahead.
                    write_head: if i == 2 { 4.0 } else { 14.0 },
                    mode: FakeMode::Write,
                    revise_start: 16.0,
                    records: HashMap::new(),
                    last_played_phrase: -8,
                    last_beat_crossed: -1,
                    instructions: Vec::new(),
                    next_instruction_at: 3.0 + i as f64 * 4.5,
                    flare_remaining: 0.0,
                    hop_from_phrase: None,
                    hop_to_phrase: None,
                    hop_elapsed: 0.0,
                }
            })
            .collect();

        let mut driver = Self {
            seed,
            elapsed_secs: 0.0,
            sims,
            next_instr_id: 1,
        };
        // Prime initial 1.0s so frame 0 has ready, running, queued, and settled notes.
        for _ in 0..10 {
            driver.step_sim(0.1);
        }
        driver
    }

    /// Advance the deterministic simulation by `dt` seconds and populate `state`.
    pub fn advance(&mut self, dt: f32, palette: &HeroicPalette, state: &mut HeroicState) {
        let clamped_dt = dt.clamp(0.0, 0.1) as f64;
        self.step_sim(clamped_dt);
        self.write_state(clamped_dt as f32, palette, state);
    }

    fn step_sim(&mut self, dt: f64) {
        self.elapsed_secs += dt;
        let t = self.elapsed_secs;

        for (i, sim) in self.sims.iter_mut().enumerate() {
            let p_len = sim.spec.beats_per_phrase as i64;
            let beats_per_sec = 1.0 / (sim.spec.period_secs as f64);
            sim.now_beats += beats_per_sec * dt;

            // Speculate/write head motion from `docs/newui.md` "The fake driver".
            // Struggling performer (i==2) stalls more severely so alarm and
            // DeadlineMissed appear reliably.
            let stall_wave = (t * 0.25 + i as f64 * 2.1).sin();
            let stall_factor = if i == 2 {
                if stall_wave > 0.2 { 0.15 } else { 0.82 }
            } else if stall_wave > 0.6 {
                0.30
            } else {
                1.0
            };

            match sim.mode {
                FakeMode::Write => {
                    sim.write_head += sim.spec.base_speed * stall_factor * dt;
                    if sim.write_head < sim.now_beats {
                        sim.write_head = sim.now_beats;
                    }
                    if sim.write_head - sim.now_beats > 20.0 {
                        sim.mode = FakeMode::Revise;
                        sim.revise_start = ((sim.now_beats + 10.0) / p_len as f64).floor() * p_len as f64;
                    }
                }
                FakeMode::Revise => {
                    if sim.write_head - sim.now_beats < 12.0 {
                        sim.mode = FakeMode::Write;
                    }
                    if sim.revise_start < sim.now_beats + 2.0 {
                        sim.revise_start = ((sim.now_beats + 10.0) / p_len as f64).floor() * p_len as f64;
                    }
                }
            }

            if sim.flare_remaining > 0.0 {
                sim.flare_remaining = (sim.flare_remaining - dt as f32).max(0.0);
            }

            // Spawn periodic sample instructions so the marker + flare visuals show.
            if t >= sim.next_instruction_at {
                let target_phrase = ((sim.now_beats + 14.0) / p_len as f64).ceil() as i64 * p_len;
                let samples = [
                    "drop to half-time",
                    "lean into root-fifth",
                    "open filter on turn",
                    "hold space on beat 4",
                ];
                let id = self.next_instr_id;
                self.next_instr_id += 1;
                sim.instructions.push(HeroicInstruction {
                    id,
                    label: samples[(id as usize) % samples.len()].to_string(),
                    target_beat: target_phrase as f64,
                    spawned_at_secs: t,
                    heard: false,
                });
                sim.next_instruction_at = t + 9.0 + (i as f64) * 2.0;
            }
            for instr in &mut sim.instructions {
                if !instr.heard && sim.write_head >= instr.target_beat {
                    instr.heard = true;
                    sim.flare_remaining = 0.8;
                }
            }
            sim.instructions
                .retain(|instr| instr.target_beat >= sim.now_beats - 4.0);

            // Populate/update records in the visible window `[now - 8, now + 48]`.
            let now_tick = sim.now_beats.floor() as i64;
            let min_phrase = ((now_tick - 8).div_euclid(p_len)) * p_len;
            let max_phrase = ((now_tick + 48).div_euclid(p_len)) * p_len;
            let lock_tick = now_tick + 1;

            let mut phrase = min_phrase;
            while phrase <= max_phrase {
                let h_exist = hash01(self.seed, i as i64 * 1009, phrase);
                let exists = phrase >= 0 && (h_exist < sim.spec.density || phrase % (p_len * 2) == 0);
                if exists {
                    let rec = sim.records.entry(phrase).or_insert_with(|| {
                        let mut id_bytes = [0u8; 16];
                        id_bytes[0] = i as u8;
                        id_bytes[1..9].copy_from_slice(&phrase.to_le_bytes());
                        WorkStatus {
                            id: WorkId(uuid::Uuid::from_bytes(id_bytes)),
                            track: TrackId::new(sim.spec.track_id).unwrap(),
                            played_by: sim.principal_id,
                            start: Tick::new(phrase),
                            admitted_at: Tick::new((phrase - 16).max(0)),
                            estimate: TickDelta::new(sim.spec.lead_beats.round() as i64),
                            estimate_wall: Duration::from_millis(
                                (sim.spec.lead_beats * 250.0) as u64,
                            ),
                            prepare_at: Tick::new((phrase - 12).max(0)),
                            attempt: 1,
                            started_at: None,
                            ready_at: None,
                            readiness: Readiness::Queued,
                            predicted: None,
                            actual: None,
                            valid: Some(true),
                            error: None,
                            timing: Some(Timing {
                                queued: Duration::from_millis(12 + (i as u64) * 5),
                                compute: Duration::from_millis(
                                    (320.0 / sim.spec.base_speed * 3.0) as u64,
                                ),
                            }),
                            settled_at: None,
                            disposition: None,
                        }
                    });

                    let pf = phrase as f64;
                    let in_revise = sim.mode == FakeMode::Revise
                        && (pf - sim.revise_start).abs() < p_len as f64;

                    if rec.disposition.is_none() {
                        if in_revise && pf > sim.now_beats + 2.0 {
                            rec.attempt = 2;
                            rec.readiness = Readiness::Running;
                        } else if phrase <= lock_tick {
                            // Past commit deadline!
                            let ready = sim.write_head >= pf + p_len as f64 * 0.5
                                || (i != 2 && h_exist < 0.88);
                            if ready && h_exist > 0.04 {
                                rec.readiness = Readiness::Ready;
                                rec.ready_at.get_or_insert(Tick::new(phrase - 2));
                                rec.settled_at = Some(Tick::new(phrase - 1));
                                rec.disposition = Some(Disposition::Committed {
                                    content: ContentRef::of(b"|: G2 B2 d2 :|", "text/vnd.abc"),
                                });
                            } else {
                                rec.settled_at = Some(Tick::new(phrase - 1));
                                rec.disposition = Some(Disposition::Fallback {
                                    reason: FallbackReason::DeadlineMissed,
                                    policy: Fallback::Skip,
                                    content: None,
                                });
                            }
                        } else if sim.write_head >= pf + p_len as f64 {
                            // Prepared ahead of playhead!
                            let fail_sample = i == 2 && hash01(self.seed + 7, i as i64, phrase) < 0.14;
                            if fail_sample {
                                rec.readiness = Readiness::Failed;
                            } else {
                                rec.readiness = Readiness::Ready;
                                rec.ready_at.get_or_insert(Tick::new(now_tick));
                            }
                        } else if sim.write_head >= pf {
                            rec.readiness = Readiness::Running;
                            rec.started_at.get_or_insert(Tick::new(now_tick));
                        } else {
                            rec.readiness = Readiness::Queued;
                        }
                    }
                }
                phrase += p_len;
            }

            // Prune very old history outside window.
            sim.records.retain(|&p, _| p >= now_tick - 64);
        }
    }

    fn write_state(&mut self, dt: f32, palette: &HeroicPalette, state: &mut HeroicState) {
        state.using_fake = true;
        state.pending_flashes.clear();

        // Decay + accumulate band energies before adding this frame's hits.
        let bps = 2.0;
        state.tick_energy(dt, bps);

        let mut lanes = Vec::with_capacity(self.sims.len());
        let mut constellation = Vec::with_capacity(self.sims.len());

        for (li, sim) in self.sims.iter_mut().enumerate() {
            let beat_idx = sim.now_beats.floor() as i64;
            if beat_idx > sim.last_beat_crossed {
                sim.last_beat_crossed = beat_idx;
                state.band_energies[0] = (state.band_energies[0] + 0.5).min(1.5);
                state.band_energies[1] = (state.band_energies[1] + 0.25).min(1.5);
            }

            let p_len = sim.spec.beats_per_phrase as i64;
            let cur_phrase = beat_idx.div_euclid(p_len) * p_len;
            if cur_phrase > sim.last_played_phrase {
                let prev = sim.last_played_phrase;
                sim.last_played_phrase = cur_phrase;
                if prev >= 0 {
                    sim.hop_from_phrase = Some(prev);
                    sim.hop_to_phrase = Some(cur_phrase);
                    sim.hop_elapsed = 0.0;
                }
                if let Some(rec) = sim.records.get(&cur_phrase) {
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
                    let char_col = heroic_identity_color(sim.spec.character);
                    match &rec.disposition {
                        Some(Disposition::Committed { .. }) => {
                            state.pending_flashes.push(PadFlashEvent {
                                lane_idx: li,
                                color: palette.now,
                                hollow: false,
                            });
                        }
                        Some(Disposition::Fallback {
                            reason: FallbackReason::Requested,
                            ..
                        }) => {
                            state.pending_flashes.push(PadFlashEvent {
                                lane_idx: li,
                                color: char_col,
                                hollow: false,
                            });
                        }
                        Some(Disposition::Fallback { content, .. }) => {
                            state.pending_flashes.push(PadFlashEvent {
                                lane_idx: li,
                                color: palette.trouble,
                                hollow: content.is_none(),
                            });
                        }
                        _ => {}
                    }
                }
            }

            let mut statuses: Vec<WorkStatus> = sim.records.values().cloned().collect();
            statuses.sort_by_key(|s| s.start);
            let slack = compute_performer_slack(
                &statuses,
                sim.now_beats,
                sim.spec.beats_per_phrase,
                sim.spec.lead_beats,
                palette.alarm_beats,
            );

            let candidates = if sim.spec.is_decision {
                let phase = (sim.now_beats * 0.4).sin() as f32 * 0.08;
                vec![
                    CandidateChoice {
                        label: "E1".into(),
                        probability: (0.41 + phase).clamp(0.15, 0.7),
                        picked: true,
                    },
                    CandidateChoice {
                        label: "G1".into(),
                        probability: (0.24 - phase * 0.5).clamp(0.08, 0.5),
                        picked: false,
                    },
                    CandidateChoice {
                        label: "A1".into(),
                        probability: 0.14,
                        picked: false,
                    },
                    CandidateChoice {
                        label: "D2".into(),
                        probability: 0.09,
                        picked: false,
                    },
                ]
            } else {
                Vec::new()
            };

            let recent_picks = if sim.spec.is_decision {
                vec!["E1".into(), "G1".into(), "E1".into(), "D2".into(), "A1".into()]
            } else {
                Vec::new()
            };

            let tail_lines = if !sim.spec.is_decision {
                vec![
                    format!("♪ phrase {} ready", cur_phrase / p_len),
                    format!("▸ speculate {}:{}", sim.spec.track_id, cur_phrase + p_len),
                    format!("✦ committed bar {}", (cur_phrase / p_len).max(0)),
                    format!("❯ lead {:.1}b slack {:.1}b", sim.spec.lead_beats, slack.slack_beats),
                ]
            } else {
                Vec::new()
            };

            if sim.hop_from_phrase.is_some() {
                sim.hop_elapsed += dt;
                let hop_dur = (palette.durations_ms.hop as f32 / 1000.0).max(0.05);
                if sim.hop_elapsed >= hop_dur {
                    sim.hop_from_phrase = None;
                    sim.hop_to_phrase = None;
                    sim.hop_elapsed = 0.0;
                }
            }

            let performer = HeroicPerformer {
                context_id: sim.context_id,
                parent_context_id: sim.parent_context_id,
                character: sim.spec.character.to_string(),
                label: sim.spec.track_id.to_string(),
                rc_dir: sim.spec.rc_dir.to_string(),
                statuses,
                lead_beats: sim.spec.lead_beats,
                slack,
                candidates,
                recent_picks,
                tail_lines,
                instructions: sim.instructions.clone(),
                flare_remaining: sim.flare_remaining,
                hop_from_phrase: sim.hop_from_phrase,
                hop_to_phrase: sim.hop_to_phrase,
                hop_elapsed: sim.hop_elapsed,
            };

            constellation.push(ConstellationStar {
                context_id: sim.context_id,
                character: sim.spec.character.to_string(),
                pos: Vec3::new(
                    (li as f32 - 1.5) * 6.5,
                    16.5 + ((li as f32) * 1.7).sin() * 2.5,
                    -42.0 - (li as f32) * 3.0,
                ),
                active: slack.state != kaijutsu_present::work::SlackState::Idle,
            });

            lanes.push(HeroicLane {
                track_id: sim.spec.track_id.to_string(),
                score_context_id: sim.context_id,
                period_secs: sim.spec.period_secs,
                beats_per_phrase: sim.spec.beats_per_phrase,
                playhead_beats: sim.now_beats,
                playing: true,
                commit_margin_ticks: 1,
                performers: vec![performer],
            });
        }

        state.lanes = lanes;
        state.constellation = constellation;
        state.round_shares = vec![
            ("omni-bass".into(), 0.34),
            ("drummer".into(), 0.31),
            ("synth-pad".into(), 0.14),
            ("lead-vox".into(), 0.21),
        ];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_present::work::SlackState;

    #[test]
    fn fake_driver_is_deterministic_for_a_seed() {
        let palette = HeroicPalette::default();
        let mut d1 = FakeDriver::with_seed(12345);
        let mut d2 = FakeDriver::with_seed(12345);
        let mut s1 = HeroicState::default();
        let mut s2 = HeroicState::default();

        for _ in 0..40 {
            d1.advance(0.05, &palette, &mut s1);
            d2.advance(0.05, &palette, &mut s2);
        }

        assert_eq!(s1.lanes.len(), s2.lanes.len());
        for (l1, l2) in s1.lanes.iter().zip(s2.lanes.iter()) {
            assert_eq!(l1.playhead_beats, l2.playhead_beats);
            assert_eq!(l1.performers[0].slack, l2.performers[0].slack);
            assert_eq!(l1.performers[0].statuses.len(), l2.performers[0].statuses.len());
        }
    }

    #[test]
    fn fake_driver_includes_alarm_and_deadline_miss_within_fixed_run() {
        let palette = HeroicPalette::default();
        let mut driver = FakeDriver::default();
        let mut state = HeroicState::default();

        let mut saw_alarm = false;
        let mut saw_deadline_miss = false;

        for _ in 0..300 {
            driver.advance(0.1, &palette, &mut state);
            if state.any_alarm() {
                saw_alarm = true;
            }
            for lane in &state.lanes {
                for p in &lane.performers {
                    if p.statuses.iter().any(|s| {
                        matches!(
                            s.disposition,
                            Some(Disposition::Fallback {
                                reason: FallbackReason::DeadlineMissed,
                                ..
                            })
                        )
                    }) {
                        saw_deadline_miss = true;
                    }
                    if p.slack.state == SlackState::Alarm {
                        saw_alarm = true;
                    }
                }
            }
        }

        assert!(saw_alarm, "fake driver must enter alarm within 30s run");
        assert!(
            saw_deadline_miss,
            "fake driver must produce a DeadlineMissed fallback within 30s run"
        );
    }
}
