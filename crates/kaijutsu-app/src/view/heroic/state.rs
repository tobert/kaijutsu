//! Scene-state layer (`HeroicState`) decoupling kernel track state (`live`)
//! from the visual systems (`road`, `notes`, `performers`, `environment`, `text`,
//! `selection`).

use std::collections::VecDeque;

use bevy::prelude::*;
use kaijutsu_hyoushigi::WorkStatus;
use kaijutsu_present::work::{PerformerSlack, SlackState};
use kaijutsu_types::ContextId;

/// One candidate probability bar for a decision-player performer.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateChoice {
    pub label: String,
    pub probability: f32,
    pub picked: bool,
}

/// An instruction sent by the player to a performer (`docs/newui.md`, "On the road").
#[derive(Debug, Clone, PartialEq)]
pub struct HeroicInstruction {
    pub id: u64,
    pub label: String,
    /// Target beat on the track clock where the instruction lands.
    pub target_beat: f64,
    /// App elapsed seconds when the marker appeared (drives the 0.7s fall).
    pub spawned_at_secs: f64,
    /// Set true once the performer's playhead/crystal reaches it.
    pub heard: bool,
}

/// One attached performer context on a track.
#[derive(Debug, Clone)]
pub struct HeroicPerformer {
    pub context_id: ContextId,
    pub principal_id: Option<kaijutsu_types::PrincipalId>,
    pub parent_context_id: Option<ContextId>,
    pub character: String,
    pub label: String,
    pub chair: Option<String>,
    pub statuses: Vec<WorkStatus>,
    pub lead_beats: f32,
    pub slack: PerformerSlack,
    pub candidates: Vec<CandidateChoice>,
    pub recent_picks: Vec<String>,
    pub tail_lines: Vec<String>,
    pub instructions: Vec<HeroicInstruction>,
    /// Remaining flare timer in seconds after receiving an instruction.
    pub flare_remaining: f32,
    /// Phrase hop state (start phrase -> target phrase, elapsed seconds).
    pub hop_from_phrase: Option<i64>,
    pub hop_to_phrase: Option<i64>,
    pub hop_elapsed: f32,
}

impl HeroicPerformer {
    /// Display title for chips and the performer panel.
    pub fn display_name(&self) -> &str {
        if !self.character.is_empty() {
            &self.character
        } else {
            &self.label
        }
    }

    pub fn lane_name(&self) -> &str {
        self.chair.as_deref().unwrap_or_else(|| self.display_name())
    }
}

/// One track and its performer lanes on the road.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct HeroicLane {
    pub track_id: String,
    pub score_context_id: ContextId,
    pub period_secs: f32,
    pub beats_per_phrase: u64,
    pub playhead_beats: f64,
    pub playing: bool,
    pub commit_margin_ticks: i64,
    pub performers: Vec<HeroicPerformer>,
}

/// One pad-hit flash event produced by a note crossing the strike line.
#[derive(Debug, Clone, Copy)]
pub struct PadFlashEvent {
    pub context_id: ContextId,
    pub color: LinearRgba,
    pub hollow: bool,
}

/// One context star in the optional sky constellation.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ConstellationStar {
    pub context_id: ContextId,
    pub character: String,
    pub pos: Vec3,
    pub active: bool,
}

/// Single shared scene state read by all heroic visual systems.
#[derive(Resource)]
pub struct HeroicState {
    pub lanes: Vec<HeroicLane>,
    /// 16-band audio/onset energy, `[0.0, 1.5]`.
    pub band_energies: [f32; 16],
    /// Rolling 32-row eighth-note peak history for the spectrogram hills.
    pub hill_history: VecDeque<[f32; 16]>,
    /// Current eighth-note accumulator `[0, 1)` for smooth row blending.
    pub hill_phase: f32,
    /// Peak buffer accumulated across the current eighth-note window.
    pub hill_peak_accum: [f32; 16],
    /// Currently selected performer context (`ContextId` keeps selection stable across polls).
    pub selected_performer: Option<ContextId>,
    /// Pending pad flashes emitted this frame for `notes.rs` to consume.
    pub pending_flashes: Vec<PadFlashEvent>,
    /// Round length in beats and current round phase `[0, 1)`.
    pub round_beats: f64,
    pub round_shares: Vec<(String, f32)>,
    /// Optional context constellation stars in the sky.
    pub constellation: Vec<ConstellationStar>,
    /// Current horizontal orbit offset in radians.
    pub orbit_angle: f32,
}

impl Default for HeroicState {
    fn default() -> Self {
        let mut hill_history = VecDeque::with_capacity(32);
        for _ in 0..32 {
            hill_history.push_back([0.0; 16]);
        }
        Self {
            lanes: Vec::new(),
            band_energies: [0.0; 16],
            hill_history,
            hill_phase: 0.0,
            hill_peak_accum: [0.0; 16],
            selected_performer: None,
            pending_flashes: Vec::new(),
            round_beats: 64.0,
            round_shares: Vec::new(),
            constellation: Vec::new(),
            orbit_angle: 0.0,
        }
    }
}

impl HeroicState {
    /// Ordered list of all currently attached performer `ContextId`s.
    pub fn performer_ids(&self) -> Vec<ContextId> {
        self.lanes
            .iter()
            .flat_map(|l| l.performers.iter().map(|p| p.context_id))
            .collect()
    }

    /// Resolve the currently selected performer to `(lane_idx, performer_idx)`.
    pub fn selected_location(&self) -> Option<(usize, usize)> {
        let target = self.selected_performer?;
        for (li, lane) in self.lanes.iter().enumerate() {
            for (pi, perf) in lane.performers.iter().enumerate() {
                if perf.context_id == target {
                    return Some((li, pi));
                }
            }
        }
        None
    }

    /// Reset driver-owned live/fake state on exit/re-entry while keeping `driver_mode`.
    pub fn clear_driver_state(&mut self) {
        self.lanes.clear();
        self.band_energies = [0.0; 16];
        for row in &mut self.hill_history {
            *row = [0.0; 16];
        }
        self.hill_phase = 0.0;
        self.hill_peak_accum = [0.0; 16];
        self.selected_performer = None;
        self.pending_flashes.clear();
        self.round_shares.clear();
        self.constellation.clear();
    }

    /// True if any performer across any lane is currently in `SlackState::Alarm`.
    pub fn any_alarm(&self) -> bool {
        self.lanes
            .iter()
            .flat_map(|l| l.performers.iter())
            .any(|p| p.slack.state == SlackState::Alarm)
    }

    /// Number of road lanes, reserving one for a track without attachments.
    pub fn road_lane_count(&self) -> usize {
        self.lanes.iter().map(|track| track.performers.len().max(1)).sum()
    }

    /// Track and performer for each road lane, in road order.
    pub fn road_lanes(&self) -> impl Iterator<Item = (&HeroicLane, Option<&HeroicPerformer>, bool)> {
        self.lanes.iter().flat_map(|track| {
            (0..track.performers.len().max(1))
                .map(move |i| (track, track.performers.get(i), i == 0))
        })
    }

    /// Total width of the road in world units.
    pub fn road_width(&self, lane_width: f32) -> f32 {
        (self.road_lane_count().max(1) as f32) * lane_width
    }

    /// Center X coordinate of `lane_idx` on the road.
    pub fn lane_center_x(&self, lane_idx: usize, lane_width: f32) -> f32 {
        let n = self.road_lane_count().max(1) as f32;
        (lane_idx as f32 - (n - 1.0) * 0.5) * lane_width
    }

    pub fn performer_lane_center_x(&self, track_idx: usize, performer_idx: usize, lane_width: f32) -> f32 {
        let preceding: usize = self.lanes[..track_idx]
            .iter()
            .map(|track| track.performers.len().max(1))
            .sum();
        self.lane_center_x(preceding + performer_idx, lane_width)
    }

    /// Push a new eighth-note peak row into `hill_history` and reset the accumulator.
    pub fn step_hill_history(&mut self) {
        if self.hill_history.len() >= 32 {
            self.hill_history.pop_back();
        }
        self.hill_history.push_front(self.hill_peak_accum);
        self.hill_peak_accum = [0.0; 16];
    }

    /// Update energy decay and accumulate eighth-note peaks for the hills.
    pub fn tick_energy(&mut self, dt: f32, beats_per_sec: f32) {
        let decay = (-4.0 * dt).exp();
        for (i, e) in self.band_energies.iter_mut().enumerate() {
            *e = (*e * decay).clamp(0.0, 1.5);
            if *e > self.hill_peak_accum[i] {
                self.hill_peak_accum[i] = *e;
            }
        }

        // Eighth-note step = 2 steps per beat.
        self.hill_phase += dt * beats_per_sec.max(0.25) * 2.0;
        while self.hill_phase >= 1.0 {
            self.hill_phase -= 1.0;
            self.step_hill_history();
        }
    }
}
