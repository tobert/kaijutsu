//! `DjCore` — the DJ thread's pure clock + click decision core (`docs/midi.md`
//! "The DJ thread", "The clock is modal, and transitions are first-class").
//!
//! HARD RULE: no bevy, no channels, no threads, no ALSA/rodio types in this
//! file. Every entry point takes an explicit `now: Instant` /
//! `now_epoch_ns: u64` rather than reading a clock itself, so the whole
//! ladder — mode transitions AND the click policy — is unit-testable with
//! hand-picked instants, no schedule, no device, no flakiness. `main.rs`
//! wires this into the real DJ thread starting in Task #2; this task is
//! clock + clicks only, no cue dispatch (see the `DjAction` sketch at the
//! bottom).
//!
//! **Two things compose here, both ported rather than newly invented:**
//! - The **click policy** is the deleted `metronome.rs::Metronome::schedule_due`
//!   ported verbatim (never-replay overdue collapse, un-strand re-seed,
//!   first-call next-whole-beat). Task #4 (the demolition) deleted
//!   `metronome.rs` for good — this is now the ONLY copy of the click policy
//!   in the codebase, not a parallel one kept in sync by hand.
//! - The **clock mode machine** is new: `docs/midi.md`'s Wallclock/BeatGrid
//!   ladder has no existing implementation to port from (the metronome is a
//!   single always-on phasor with no notion of "fall back to wallclock cue
//!   placement"). `RefDisposition::Touch`'s doc already frames its job as
//!   "liveness only" — `DjCore` is the first consumer that actually *uses*
//!   Touch as a liveness signal (the metronome's `ingest_beat_signals` treats
//!   Touch as a pure no-op, by its own doc comment, because a single-phasor
//!   design has no separate liveness clock to bump).

// Plan-phase scaffolding (DJ-thread arc Task #1 of 4, docs/midi.md "The DJ
// thread"): nothing outside this module constructs a `DjCore` yet — Task #2
// wires it into the real thread and starts calling every method here. Not a
// "might be useful later" annotation; the plan names the exact consumer.
#![allow(dead_code)]

use std::time::{Duration, Instant};

use kaijutsu_audio::{BeatRef, LocalBeat, REF_STALE_MAX, RefDisposition, Slew};

/// How long BeatGrid may free-run on liveness alone — `Touch`-aged references
/// proving the master is alive without ever correcting phase — before the DJ
/// stops trusting the phasor (2026-07-18 gemini-pro deliberation, finding 2).
/// Sustained Touch means every reference is 1–5 s late (persistent pipe
/// backpressure): the liveness clock stays fresh forever while the phasor
/// free-runs on its last feedforward tempo, drifting from kernel truth at
/// crystal-offset rates (~ppm — roughly a millisecond per ten seconds at the
/// bad end). 10 s caps that drift around the just-audible threshold; the
/// deadband doctrine ("the local clock is the truth between references")
/// spans reference *gaps*, not an unbounded correction embargo. Tune from
/// the `kaijutsu.dj.clock_transition{reason="free_run_cap"}` rate, not vibes.
pub const MAX_FREE_RUN: Duration = Duration::from_secs(10);

/// Which timebase the DJ currently places cues/clicks against
/// (`docs/midi.md` "The clock is modal, and transitions are first-class").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClockMode {
    /// Anchor state: cue placement by the emission-stamp backdating ladder
    /// (`audio_sched::effective_deadline` / `midi::backdate_events`, Task #1
    /// keeps them where they are); clicks silent — no phasor to click from.
    #[default]
    Wallclock,
    /// Dialed in: a [`LocalBeat`] phasor is running. Clicks fire at its
    /// predicted beat time; a cue carrying an onset-beat stamp will place
    /// against the phasor once Task #2 adds that field. Held through
    /// `Touch`-aged references (the free-running deadband doctrine,
    /// `docs/midi.md` "The one timebase") — only silence past
    /// [`REF_STALE_MAX`], a flush, or a disconnect drops back out.
    BeatGrid,
}

impl ClockMode {
    /// The telemetry `to` attribute value (`kaijutsu.dj.clock_transition`).
    pub fn as_str(self) -> &'static str {
        match self {
            ClockMode::Wallclock => "wallclock",
            ClockMode::BeatGrid => "beat_grid",
        }
    }
}

/// Why a [`ClockMode`] transition happened — the telemetry `reason`
/// attribute (`kaijutsu.dj.clock_transition`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionReason {
    /// Wallclock → BeatGrid: a `Fold`-fresh `BeatSync` anchored (or, if
    /// already in BeatGrid, corrected) the phasor. Only the *anchoring* Fold
    /// — the one that actually changes `mode` — reports a transition; every
    /// later Fold while already dialed in is silent (see
    /// [`DjCore::observe_beat_sync`]'s doc).
    Fold,
    /// BeatGrid → Wallclock: the last live reference (`Fold` or `Touch`)
    /// aged past [`REF_STALE_MAX`] before a fresher one arrived, caught at a
    /// click-check (`docs/midi.md`: "reference age past `REF_STALE_MAX`
    /// observed at a tick/click check" — a proactive check, since silence
    /// alone never *arrives* as an event to react to).
    Stale,
    /// BeatGrid → Wallclock: a transport flush (`RENDER_FLUSH_MIME`) — the
    /// take ended, so the phasor and click cursor reset with it (mirrors
    /// `Metronome::reset`'s flush caller).
    Flush,
    /// BeatGrid → Wallclock: the connection dropped (kernel restart/crash/
    /// network) — mirrors `Metronome::halt_on_connection_loss`: the kernel
    /// is gone and can send no flush, so this is a separate trigger.
    Disconnect,
    /// BeatGrid → Wallclock: liveness stayed fresh but no `Fold` corrected
    /// the phasor for [`MAX_FREE_RUN`] — sustained `Touch` under persistent
    /// pipe backpressure would otherwise keep an uncorrected, drifting
    /// phasor trusted forever (see `MAX_FREE_RUN`'s doc). Caught at the same
    /// proactive click-check site as `Stale`.
    FreeRunCap,
}

impl TransitionReason {
    /// The telemetry `reason` attribute value.
    pub fn as_str(self) -> &'static str {
        match self {
            TransitionReason::Fold => "fold",
            TransitionReason::Stale => "stale",
            TransitionReason::Flush => "flush",
            TransitionReason::Disconnect => "disconnect",
            TransitionReason::FreeRunCap => "free_run_cap",
        }
    }
}

/// A [`ClockMode`] change the caller should feed to telemetry
/// (`kaijutsu_telemetry::record_dj_clock_transition`) — see `docs/midi.md`
/// "Every transition emits telemetry." Pure report, no side effect: `DjCore`
/// applies the mode change itself and hands this out for the CALLER to
/// record (same stance as `Slew`/`record_phasor_slew` — pure cores return
/// report data, the caller records telemetry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockTransition {
    pub to: ClockMode,
    pub reason: TransitionReason,
}

/// Click sound + gate config, resolved from the per-client `metronome.toml`
/// (`docs/config-crdt-ownership.md` "Per-client config") and applied via
/// `DjCtl::MetronomeConfig` (`dj::thread::forward_metronome_config_to_dj`).
/// Was a straight copy of the deleted `metronome.rs::MetronomeConfig` while
/// that type still existed side by side (Tasks #1–#3); Task #4 deleted the
/// original, so this is now the SOLE definition — same fields, same shipped
/// default, still validated against `assets/defaults/metronome.toml` by this
/// module's own tests (see `config_parses_the_shipped_default_and_fills_partials`
/// below; that coverage would otherwise have been lost along with
/// `metronome.rs`'s own copy of the same test).
///
/// The click is a *pitched* note on a dedicated channel, not a drum: GM
/// channel-9 percussion is silent under game soundfonts (the FF4 one on
/// zorak), so `dj::midi::MidiSink::click_at` gates a melodic note instead.
/// C6 (84) reads as a crisp tick.
#[derive(serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct MetronomeConfig {
    /// Whether the click sounds at all (on top of the always-on "only while a
    /// live clock rolls" gate — [`DjCore::due_clicks`] returns nothing while
    /// [`ClockMode::Wallclock`]).
    pub enabled: bool,
    /// MIDI note number for the click.
    pub note: u8,
    /// MIDI channel (0–15), off the music's channel 0.
    pub channel: u8,
    /// Note-on velocity (1–127).
    pub velocity: u8,
    /// Milliseconds the note sounds before note-off.
    pub gate_ms: u64,
}

impl Default for MetronomeConfig {
    fn default() -> Self {
        // Must match assets/defaults/metronome.toml — verified by
        // `config_parses_the_shipped_default_and_fills_partials` below.
        Self { enabled: true, note: 84, channel: 15, velocity: 110, gate_ms: 60 }
    }
}

/// One `observe_beat_sync` call's report: the [`Slew`] rider (telemetry,
/// `record_phasor_slew`) plus an optional [`ClockTransition`] — `Some` only
/// on the one call that actually changed [`ClockMode`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BeatObservation {
    /// Mirrors `Metronome::observe`'s return: `None` on the anchoring first
    /// fold (no prior phasor state to report a slew against) or on a
    /// `Touch`/`Drop` disposition (no fold happened at all); `Some` on an
    /// ongoing correction.
    pub slew: Option<Slew>,
    pub transition: Option<ClockTransition>,
}

/// One `due_clicks` call's report: the click offsets (mirrors
/// `Metronome::schedule_due`'s return exactly) plus an optional
/// [`ClockTransition`] from the staleness check this call also performs.
/// When `transition.is_some()`, `offsets` is always empty — a click-check
/// that just fell back to Wallclock has no phasor left to schedule from.
#[derive(Debug, Clone, PartialEq)]
pub struct DueClicks {
    pub offsets: Vec<Duration>,
    pub transition: Option<ClockTransition>,
}

/// The DJ thread's pure clock + click state machine.
///
/// State mirrors `Metronome` (the phasor, the click cursor) PLUS the modal
/// [`ClockMode`] `Metronome` has no notion of — the metronome is a single
/// always-on phasor; `DjCore` additionally decides WHETHER to trust it for
/// cue placement, falling back to the wallclock backdating ladder when the
/// clock master goes quiet, flushes, or disconnects.
pub struct DjCore {
    mode: ClockMode,
    /// The phasor, once a first `Fold`-disposition reference has anchored it.
    beat: Option<LocalBeat>,
    /// The next integer beat not yet scheduled into the sink queue — see
    /// `Metronome::schedule_due`'s doc for the invariant (each beat
    /// scheduled exactly once, ahead of time).
    next_beat: Option<i64>,
    /// Instant of the last LIVE reference — `Fold` or `Touch`, both count
    /// (Touch's entire reason to exist is liveness, per
    /// [`kaijutsu_audio::RefDisposition`]'s doc); `Drop` does not, since an
    /// already-stale-by-its-own-stamp packet proves nothing about "now".
    /// `due_clicks` compares `now` against this at every click-check —
    /// that's the proactive half of the staleness ladder: unlike a
    /// `BeatRef`'s own age (checked only when a ref actually arrives),
    /// silence never arrives as an event, so something has to poll for it.
    /// `None` before the first reference ever anchors the phasor, and reset
    /// to `None` on every exit back to Wallclock.
    last_ref_at: Option<Instant>,
    /// Instant of the last `Fold` — the last time the phasor was actually
    /// *corrected*, as opposed to merely proven alive (`last_ref_at`). The
    /// gap between the two is exactly the sustained-Touch failure mode
    /// [`MAX_FREE_RUN`] bounds: liveness fresh, correction ancient. Same
    /// lifecycle as `last_ref_at` (set on Fold, cleared on every exit).
    last_fold_at: Option<Instant>,
    /// Click sound + gate. `pub` like `Metronome::config` — applied from the
    /// outside (an RPC-fetched `metronome.toml`), not derived here.
    pub metronome: MetronomeConfig,
}

impl Default for DjCore {
    fn default() -> Self {
        Self {
            mode: ClockMode::default(),
            beat: None,
            next_beat: None,
            last_ref_at: None,
            last_fold_at: None,
            metronome: MetronomeConfig::default(),
        }
    }
}

impl DjCore {
    /// The current clock mode.
    pub fn mode(&self) -> ClockMode {
        self.mode
    }

    /// The phasor's fractional beat position at `now`, or `None` while no
    /// phasor is anchored (Wallclock with nothing ever folded, or just
    /// reset). Exposed for the eventual cue-placement wiring (Task #2/#3)
    /// and for tests.
    pub fn beat_position(&self, now: Instant) -> Option<f64> {
        self.beat.as_ref().map(|b| b.position(now))
    }

    /// Fold one reference into the phasor, creating it on the first call —
    /// ported verbatim from `Metronome::observe`.
    fn fold(&mut self, reference: BeatRef, at: Instant) -> Option<Slew> {
        match &mut self.beat {
            Some(beat) => Some(beat.observe(reference, at)),
            None => {
                self.beat = Some(LocalBeat::new(reference, at));
                None
            }
        }
    }

    /// Reset the phasor + click cursor + liveness clock — ported from
    /// `Metronome::reset`, plus `last_ref_at` (the field `Metronome` doesn't
    /// have, since it has no liveness clock to clear).
    fn reset_clock(&mut self) {
        self.beat = None;
        self.next_beat = None;
        self.last_ref_at = None;
        self.last_fold_at = None;
    }

    /// Enter `BeatGrid` if not already there; report a transition only on
    /// the actual change (repeated Folds while already dialed in are
    /// silent — the whole point of "reported once, not on every fold").
    fn enter_beat_grid(&mut self, reason: TransitionReason) -> Option<ClockTransition> {
        if self.mode == ClockMode::BeatGrid {
            return None;
        }
        self.mode = ClockMode::BeatGrid;
        Some(ClockTransition { to: ClockMode::BeatGrid, reason })
    }

    /// Reset the clock and fall back to `Wallclock` if not already there;
    /// report a transition only on the actual change. Resetting
    /// unconditionally (even when `mode` is already `Wallclock`) mirrors
    /// `Metronome::reset`'s idempotence — a second flush/disconnect while
    /// already silent is a harmless no-op, not an error.
    fn exit_beat_grid(&mut self, reason: TransitionReason) -> Option<ClockTransition> {
        self.reset_clock();
        if self.mode == ClockMode::Wallclock {
            return None;
        }
        self.mode = ClockMode::Wallclock;
        Some(ClockTransition { to: ClockMode::Wallclock, reason })
    }

    /// Ingest one `BeatSync`-carried [`BeatRef`], dispositioned exactly as
    /// `kaijutsu_audio::BeatRef::disposition` ladders it
    /// (`docs/midi.md` "The one timebase"):
    ///
    /// - **`Fold`** (fresh, age ≤ `REF_FOLD_MAX`, or unstamped): back-dated
    ///   to its own emission instant and folded into the phasor exactly as
    ///   `Metronome::observe` does; marks liveness; enters `BeatGrid` if
    ///   this is the reference that first anchors the phasor (Wallclock →
    ///   BeatGrid, reason `Fold`) — silent on every subsequent Fold while
    ///   already dialed in, so telemetry sees one row per anchoring, not one
    ///   per reference.
    /// - **`Touch`** (older, but within `REF_STALE_MAX`): marks liveness
    ///   ONLY — never folds the phasor, never changes `mode`. This is the
    ///   free-running deadband doctrine made modal: BeatGrid holds through a
    ///   Touch-aged reference exactly as the phasor itself would free-run
    ///   through one.
    /// - **`Drop`** (stale past `REF_STALE_MAX` by its own stamp): a pure
    ///   no-op — doesn't fold, doesn't mark liveness (an already-ancient
    ///   packet proves nothing about "is the master still talking").
    pub fn observe_beat_sync(
        &mut self,
        beat_ref: BeatRef,
        now: Instant,
        now_epoch_ns: u64,
    ) -> BeatObservation {
        match beat_ref.disposition(now, now_epoch_ns) {
            RefDisposition::Fold(at) => {
                self.last_ref_at = Some(now);
                self.last_fold_at = Some(now);
                let slew = self.fold(beat_ref, at);
                let transition = self.enter_beat_grid(TransitionReason::Fold);
                BeatObservation { slew, transition }
            }
            RefDisposition::Touch => {
                self.last_ref_at = Some(now);
                BeatObservation { slew: None, transition: None }
            }
            RefDisposition::Drop => BeatObservation { slew: None, transition: None },
        }
    }

    /// A transport flush (`RENDER_FLUSH_MIME`): stop/pause ended the take, so
    /// the phasor and click cursor reset. `now` is accepted for symmetry
    /// with every other entry point here (`docs/midi.md`'s "everything takes
    /// an explicit `now`") even though the reset itself is time-independent
    /// — mirrors `Metronome::reset`'s own unconditional semantics.
    pub fn on_flush(&mut self, now: Instant) -> Option<ClockTransition> {
        let _ = now;
        self.exit_beat_grid(TransitionReason::Flush)
    }

    /// The connection dropped (kernel restart/crash/network) — the kernel is
    /// gone and can send no flush, so this is a separate trigger, mirroring
    /// `Metronome::halt_on_connection_loss`. Same `now`-for-symmetry note as
    /// [`Self::on_flush`].
    pub fn on_disconnect(&mut self, now: Instant) -> Option<ClockTransition> {
        let _ = now;
        self.exit_beat_grid(TransitionReason::Disconnect)
    }

    /// Click offsets due within `horizon` of `now`, PLUS the staleness check
    /// that can fall BeatGrid back to Wallclock. The staleness check runs
    /// first and short-circuits: a click-check that discovers the last live
    /// reference is older than [`REF_STALE_MAX`] resets and reports the
    /// transition instead of scheduling (there is no phasor left to trust by
    /// the time this returns).
    ///
    /// The click policy itself (once past the staleness gate, or while
    /// already Wallclock — schedule_due is a no-op with no phasor either
    /// way) is [`Self::schedule_due`], ported verbatim from
    /// `Metronome::schedule_due`.
    pub fn due_clicks(&mut self, now: Instant, horizon: Duration) -> DueClicks {
        if self.mode == ClockMode::BeatGrid {
            // Stale first (total silence — the tighter signal), then the
            // free-run cap (alive but never corrected, `MAX_FREE_RUN`'s
            // sustained-Touch failure mode). Both are proactive: neither
            // condition ever *arrives* as an event.
            if let Some(last) = self.last_ref_at {
                if now.saturating_duration_since(last) > REF_STALE_MAX {
                    let transition = self.exit_beat_grid(TransitionReason::Stale);
                    return DueClicks { offsets: Vec::new(), transition };
                }
            }
            if let Some(folded) = self.last_fold_at {
                if now.saturating_duration_since(folded) > MAX_FREE_RUN {
                    let transition = self.exit_beat_grid(TransitionReason::FreeRunCap);
                    return DueClicks { offsets: Vec::new(), transition };
                }
            }
        }
        DueClicks { offsets: self.schedule_due(now, horizon), transition: None }
    }

    /// The instant at which BeatGrid would fall back to Wallclock if no
    /// further reference arrived — `min(last_ref_at + REF_STALE_MAX,
    /// last_fold_at + MAX_FREE_RUN)`; `None` while not in BeatGrid (nothing
    /// to fall back from). The thread bounds its `select!` sleep by this
    /// (2026-07-18 deliberation, finding 5: with the staleness check living
    /// inside `due_clicks`, a slow tempo would otherwise leave the mode
    /// machine on a dead grid — placing cues against it, stamping telemetry
    /// late — until the next click happened to wake the loop).
    pub fn next_stale_deadline(&self) -> Option<Instant> {
        if self.mode != ClockMode::BeatGrid {
            return None;
        }
        let stale = self.last_ref_at.map(|t| t + REF_STALE_MAX);
        let capped = self.last_fold_at.map(|t| t + MAX_FREE_RUN);
        match (stale, capped) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Return the offsets-from-`now` at which to schedule a click for every
    /// integer beat whose predicted time falls within `horizon` — each beat
    /// returned exactly once across calls. PORTED VERBATIM from
    /// `Metronome::schedule_due`; see that method's doc for the never-replay
    /// + un-strand policy derivation (this is the same policy, not a
    /// reimplementation — only `self`'s type differs).
    fn schedule_due(&mut self, now: Instant, horizon: Duration) -> Vec<Duration> {
        let Some(beat) = &self.beat else {
            return Vec::new();
        };
        let cur = beat.position(now);
        let tempo = beat.tempo_bps();
        if tempo <= 0.0 {
            return Vec::new();
        }
        let horizon_secs = horizon.as_secs_f64();
        let mut next = self.next_beat.unwrap_or_else(|| cur.floor() as i64 + 1);
        let mut offsets = Vec::new();

        if (next as f64) <= cur.floor() {
            offsets.push(Duration::ZERO);
            next = cur.floor() as i64 + 1;
        } else {
            let max_ahead = horizon_secs * tempo + 2.0;
            if next as f64 - cur > max_ahead {
                next = cur.floor() as i64 + 1;
            }
        }

        loop {
            let secs = (next as f64 - cur) / tempo;
            if secs > horizon_secs {
                break;
            }
            offsets.push(Duration::from_secs_f64(secs));
            next += 1;
        }
        self.next_beat = Some(next);
        offsets
    }

    /// When the DJ thread's click timer should next wake — the instant the
    /// next not-yet-scheduled beat ENTERS the pre-schedule horizon
    /// (`beat_instant − horizon`, clamped to `now`), or `None` while no
    /// phasor is running (nothing to wake for; the thread's `select!` then
    /// waits only on its other arms).
    ///
    /// The horizon lead is the point (2026-07-18 deliberation, finding 3 +
    /// lead's confirmation): waking AT the beat would hand `due_clicks` an
    /// offset of ~zero, scheduling the click into ALSA with no lead at all —
    /// exactly the dispatch-jitter exposure pre-scheduling exists to remove
    /// (`SCHEDULE_HORIZON`'s doc: "a beat is always queued *before* it
    /// sounds"). The old metronome got its lead for free by running every
    /// frame; a sleeping thread has to aim ahead deliberately. Waking as the
    /// beat crosses into the horizon hands `due_clicks` an offset of
    /// ~`horizon`, restoring the old path's full lead.
    pub fn next_wake(&self, now: Instant, horizon: Duration) -> Option<Instant> {
        let beat = self.beat.as_ref()?;
        let tempo = beat.tempo_bps();
        if tempo <= 0.0 {
            return None;
        }
        let cur = beat.position(now);
        let next = self.next_beat.map(|n| n as f64).unwrap_or_else(|| cur.floor() + 1.0);
        let secs = ((next - cur) / tempo - horizon.as_secs_f64()).max(0.0);
        Some(now + Duration::from_secs_f64(secs))
    }
}

// ── DjAction — sketch for Tasks #3/#4, not implemented here ────────────────
//
// This task is clock + clicks only: `DjCore` decides mode and click timing,
// nothing else. The eventual cue-dispatch decision core
// (`handle_event(now, now_epoch, event) -> Vec<DjAction>`, `docs/midi.md`
// "The DJ thread") will need a vocabulary roughly like:
//
// enum DjAction {
//     /// Fire a click now (or at a scheduled ALSA offset) — what
//     /// `due_clicks`'s offsets become once the audio dispatch lands.
//     Click { offset: Duration },
//     /// Place a RenderCue: either the wallclock-ladder deadline
//     /// (`audio_sched::effective_deadline`/`midi::backdate_events`, Task
//     /// #1's untouched machinery) or, once BeatGrid + an onset-beat stamp
//     /// are wired (Task #2), the phasor's predicted instant for that beat.
//     PlaceCue { cue: kaijutsu_audio::RenderCue, at: PlacementInstant },
//     /// A transport flush reached the sink layer: drop pending, silence
//     /// live.
//     FlushSinks,
//     /// Dispatch a CAS prefetch (folded in from `audio.rs::CasPrefetch`,
//     /// Task #4).
//     Prefetch { hash: kaijutsu_cas::ContentHash, .. },
// }
//
// `PlacementInstant` would be the Wallclock/BeatGrid split made concrete —
// exactly the choice `DjCore::mode()` already exposes today.

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_audio::REF_FOLD_MAX;

    const H: Duration = Duration::from_millis(250); // mirrors metronome.rs's SCHEDULE_HORIZON

    fn near(a: Duration, b_ms: f64) -> bool {
        (a.as_secs_f64() * 1000.0 - b_ms).abs() < 1.0
    }

    // ── Mode machine ─────────────────────────────────────────────────────

    #[test]
    fn starts_in_wallclock_mode() {
        let dj = DjCore::default();
        assert_eq!(dj.mode(), ClockMode::Wallclock);
    }

    #[test]
    fn a_fold_fresh_ref_enters_beat_grid_reported_once_not_every_fold() {
        let mut dj = DjCore::default();
        let t0 = Instant::now();

        let obs = dj.observe_beat_sync(BeatRef::new(0.0, 2.0), t0, 0);
        assert_eq!(dj.mode(), ClockMode::BeatGrid);
        assert_eq!(
            obs.transition,
            Some(ClockTransition { to: ClockMode::BeatGrid, reason: TransitionReason::Fold }),
            "the anchoring fold reports the transition"
        );

        // A second, agreeing Fold while already dialed in must NOT report a
        // transition again — telemetry sees one row per anchoring.
        let t1 = t0 + Duration::from_secs(1);
        let obs2 = dj.observe_beat_sync(BeatRef::new(2.0, 2.0), t1, 0);
        assert_eq!(dj.mode(), ClockMode::BeatGrid);
        assert_eq!(obs2.transition, None, "no repeat transition on an ordinary fold");
    }

    #[test]
    fn a_touch_aged_ref_holds_beat_grid_and_never_moves_the_phasor() {
        let mut dj = DjCore::default();
        let now_epoch_ns: u64 = 10_000_000_000;

        // Anchor with a fresh, stamped ref (age 0 -> Fold).
        let t0 = Instant::now();
        dj.observe_beat_sync(BeatRef { beat: 0.0, tempo_bps: 2.0, epoch_ns: now_epoch_ns }, t0, now_epoch_ns);
        assert_eq!(dj.mode(), ClockMode::BeatGrid);
        let anchored_pos = dj.beat_position(t0).unwrap();

        // A ref 2s old (past REF_FOLD_MAX=1s, within REF_STALE_MAX=5s) with a
        // wildly different beat value: if it folded, position would jump
        // toward 999.0.
        let touch_epoch_ns = now_epoch_ns + 2_000_000_000; // "later" wallclock, ref itself is 2s stale relative to it
        let obs = dj.observe_beat_sync(
            BeatRef { beat: 999.0, tempo_bps: 2.0, epoch_ns: now_epoch_ns },
            t0,
            touch_epoch_ns,
        );
        assert_eq!(obs.slew, None, "Touch never folds — no Slew report");
        assert_eq!(obs.transition, None, "Touch never changes mode");
        assert_eq!(dj.mode(), ClockMode::BeatGrid, "still dialed in");
        let after_pos = dj.beat_position(t0).unwrap();
        assert!(
            (after_pos - anchored_pos).abs() < 1.0,
            "a Touch-band ref must not step the phasor toward its beat: {anchored_pos} -> {after_pos}"
        );
    }

    #[test]
    fn stale_at_a_click_check_falls_back_to_wallclock_reason_stale() {
        let mut dj = DjCore::default();
        let t0 = Instant::now();
        dj.observe_beat_sync(BeatRef::new(0.0, 2.0), t0, 0);
        assert_eq!(dj.mode(), ClockMode::BeatGrid);

        // No further reference arrives; a click-check well past REF_STALE_MAX
        // after the last live signal must fall back on its own (nothing ever
        // "arrives" to report Drop — the check has to be proactive).
        let due = dj.due_clicks(t0 + REF_STALE_MAX + Duration::from_millis(1), H);
        assert_eq!(dj.mode(), ClockMode::Wallclock);
        assert_eq!(
            due.transition,
            Some(ClockTransition { to: ClockMode::Wallclock, reason: TransitionReason::Stale })
        );
        assert!(due.offsets.is_empty(), "a click-check that falls back schedules nothing");
    }

    #[test]
    fn exactly_at_the_stale_boundary_still_holds_beat_grid() {
        // Mirrors the inclusive-boundary convention the rest of the ladder
        // uses (REF_STALE_MAX itself is still trusted; only PAST it drops).
        let mut dj = DjCore::default();
        let t0 = Instant::now();
        dj.observe_beat_sync(BeatRef::new(0.0, 2.0), t0, 0);

        let due = dj.due_clicks(t0 + REF_STALE_MAX, H);
        assert_eq!(dj.mode(), ClockMode::BeatGrid, "exactly REF_STALE_MAX old is still accepted");
        assert!(due.transition.is_none());
    }

    #[test]
    fn a_touch_extends_liveness_past_what_a_single_fold_would_cover() {
        // A Touch-band ref (age > REF_FOLD_MAX) still refreshes last_ref_at,
        // pushing the staleness deadline out even though it never folds the
        // phasor — this is what makes Touch a genuine liveness signal rather
        // than a no-op.
        let mut dj = DjCore::default();
        let now_epoch_ns: u64 = 10_000_000_000;
        let t0 = Instant::now();
        dj.observe_beat_sync(BeatRef { beat: 0.0, tempo_bps: 2.0, epoch_ns: now_epoch_ns }, t0, now_epoch_ns);

        // At t0 + 3s, touch with a ref stamped 2s old relative to THIS receipt
        // (still within REF_STALE_MAX of the anchor too, so this alone
        // wouldn't prove much) — refresh liveness here.
        let t1 = t0 + Duration::from_secs(3);
        let touch_now_epoch_ns = now_epoch_ns + 3_000_000_000;
        let touch_epoch_ns = touch_now_epoch_ns - 2_000_000_000; // 2s old -> Touch band
        dj.observe_beat_sync(
            BeatRef { beat: 6.0, tempo_bps: 2.0, epoch_ns: touch_epoch_ns },
            t1,
            touch_now_epoch_ns,
        );
        assert_eq!(dj.mode(), ClockMode::BeatGrid);

        // At t0 + 4.5s (1.5s after the touch) a click-check must still hold —
        // without the touch's liveness refresh, t0+4.5s would be only 4.5s
        // past the ORIGINAL anchor, still under REF_STALE_MAX anyway, so
        // push further: t0 + 3s + REF_STALE_MAX - epsilon must still hold,
        // proving the deadline moved with the touch, not just the anchor.
        let checkpoint = t1 + REF_STALE_MAX - Duration::from_millis(1);
        let due = dj.due_clicks(checkpoint, H);
        assert_eq!(dj.mode(), ClockMode::BeatGrid, "the touch pushed the staleness deadline out");
        assert!(due.transition.is_none());
    }

    #[test]
    fn a_flush_falls_back_to_wallclock_reason_flush_and_resets_the_cursor() {
        let mut dj = DjCore::default();
        let t0 = Instant::now();
        dj.observe_beat_sync(BeatRef::new(0.0, 2.0), t0, 0);
        dj.due_clicks(t0, H); // seed next_beat
        assert_eq!(dj.mode(), ClockMode::BeatGrid);

        let transition = dj.on_flush(t0 + Duration::from_millis(10));
        assert_eq!(
            transition,
            Some(ClockTransition { to: ClockMode::Wallclock, reason: TransitionReason::Flush })
        );
        assert_eq!(dj.mode(), ClockMode::Wallclock);
        assert!(dj.beat_position(t0).is_none(), "phasor dropped");
        assert!(dj.due_clicks(t0, H).offsets.is_empty(), "cursor reset — nothing pending");
    }

    #[test]
    fn a_second_flush_while_already_wallclock_is_a_silent_no_op() {
        let mut dj = DjCore::default();
        let t0 = Instant::now();
        assert_eq!(dj.on_flush(t0), None, "flushing an already-silent DJ reports nothing");
        assert_eq!(dj.mode(), ClockMode::Wallclock);
    }

    #[test]
    fn a_disconnect_falls_back_to_wallclock_reason_disconnect_and_resets_the_cursor() {
        let mut dj = DjCore::default();
        let t0 = Instant::now();
        dj.observe_beat_sync(BeatRef::new(0.0, 2.0), t0, 0);
        dj.due_clicks(t0, H);
        assert_eq!(dj.mode(), ClockMode::BeatGrid);

        let transition = dj.on_disconnect(t0 + Duration::from_millis(10));
        assert_eq!(
            transition,
            Some(ClockTransition {
                to: ClockMode::Wallclock,
                reason: TransitionReason::Disconnect
            })
        );
        assert_eq!(dj.mode(), ClockMode::Wallclock);
        assert!(dj.beat_position(t0).is_none());
    }

    #[test]
    fn re_anchoring_after_a_fallback_returns_to_beat_grid() {
        let mut dj = DjCore::default();
        let t0 = Instant::now();
        dj.observe_beat_sync(BeatRef::new(0.0, 2.0), t0, 0);
        dj.on_flush(t0);
        assert_eq!(dj.mode(), ClockMode::Wallclock);

        // The next `play` re-anchors it — a fresh Fold from Wallclock reports
        // the transition again (it's a genuinely new anchoring, not a repeat).
        let t1 = t0 + Duration::from_secs(5);
        let obs = dj.observe_beat_sync(BeatRef::new(10.0, 2.0), t1, 0);
        assert_eq!(dj.mode(), ClockMode::BeatGrid);
        assert_eq!(
            obs.transition,
            Some(ClockTransition { to: ClockMode::BeatGrid, reason: TransitionReason::Fold })
        );
    }

    #[test]
    fn a_drop_disposition_ref_neither_folds_nor_extends_liveness() {
        // An already-ancient-by-its-own-stamp ref (age > REF_STALE_MAX) must
        // not reset the staleness clock either — otherwise a flood of junk
        // packets could keep BeatGrid alive forever with no real signal.
        let mut dj = DjCore::default();
        let now_epoch_ns: u64 = 10_000_000_000;
        let t0 = Instant::now();
        dj.observe_beat_sync(BeatRef { beat: 0.0, tempo_bps: 2.0, epoch_ns: now_epoch_ns }, t0, now_epoch_ns);

        // At t0 + 4.9s (just under REF_STALE_MAX), an ancient ref arrives
        // (dropped) — it must not refresh liveness.
        let t1 = t0 + REF_STALE_MAX - Duration::from_millis(100);
        let ancient_epoch_ns = now_epoch_ns; // stamped at anchor time, but "now" has moved far past REF_STALE_MAX
        let far_now_epoch_ns = now_epoch_ns + (REF_STALE_MAX.as_nanos() as u64) * 10;
        let obs = dj.observe_beat_sync(
            BeatRef { beat: 5.0, tempo_bps: 2.0, epoch_ns: ancient_epoch_ns },
            t1,
            far_now_epoch_ns,
        );
        assert_eq!(obs.transition, None);

        // A click-check just past the ORIGINAL anchor's staleness deadline
        // must still fall back — the dropped ref bought nothing.
        let due = dj.due_clicks(t0 + REF_STALE_MAX + Duration::from_millis(1), H);
        assert_eq!(dj.mode(), ClockMode::Wallclock);
        assert_eq!(due.transition.map(|t| t.reason), Some(TransitionReason::Stale));
    }

    // ── Click policy ports (from metronome.rs's Metronome::schedule_due) ──

    #[test]
    fn a_fresh_dj_schedules_nothing() {
        let mut dj = DjCore::default();
        assert!(dj.due_clicks(Instant::now(), H).offsets.is_empty(), "no reference yet");
    }

    #[test]
    fn the_first_reference_does_not_retro_schedule() {
        let mut dj = DjCore::default();
        let t0 = Instant::now();
        dj.observe_beat_sync(BeatRef::new(100.0, 2.0), t0, 0);
        assert!(dj.due_clicks(t0, H).offsets.is_empty(), "anchoring queues nothing");
    }

    #[test]
    fn beats_are_scheduled_once_at_their_predicted_offset() {
        let mut dj = DjCore::default();
        let t0 = Instant::now();
        dj.observe_beat_sync(BeatRef::new(0.0, 2.0), t0, 0); // 120 BPM
        assert!(dj.due_clicks(t0, H).offsets.is_empty());

        let due = dj.due_clicks(t0 + Duration::from_millis(300), H);
        assert_eq!(due.offsets.len(), 1);
        assert!(near(due.offsets[0], 200.0), "beat 1 lands 200 ms out, got {:?}", due.offsets[0]);

        assert!(dj.due_clicks(t0 + Duration::from_millis(350), H).offsets.is_empty());

        let due2 = dj.due_clicks(t0 + Duration::from_millis(800), H);
        assert_eq!(due2.offsets.len(), 1, "beat 2 scheduled exactly once");
        assert!(near(due2.offsets[0], 200.0));
    }

    #[test]
    fn a_stalled_check_clicks_at_most_once_never_a_burst() {
        let mut dj = DjCore::default();
        let t0 = Instant::now();
        dj.observe_beat_sync(BeatRef::new(0.0, 2.0), t0, 0);
        dj.due_clicks(t0, H); // seeds next_beat = 1

        let due = dj.due_clicks(t0 + Duration::from_millis(1600), H);
        assert_eq!(due.offsets.len(), 1, "the whole missed backlog collapses to one click: {due:?}");
        assert!(due.offsets[0].is_zero(), "clamped to now, not scheduled into the past");
        assert!(due.transition.is_none(), "well within REF_STALE_MAX — no mode change");

        assert!(dj.due_clicks(t0 + Duration::from_millis(1600), H).offsets.is_empty(), "no replay");

        let resumed = dj.due_clicks(t0 + Duration::from_millis(1750), H);
        assert_eq!(resumed.offsets.len(), 1, "cadence resumed on the grid, not still catching up");
        assert!(near(resumed.offsets[0], 250.0));
    }

    /// Ported from the deleted `metronome.rs`'s
    /// `a_large_forward_jump_still_clicks_at_most_once` (not merely a
    /// duplicate of the 3-beat backlog above — a BIGGER backlog, 8 beats
    /// missed, must STILL collapse to exactly one click; the invariant is "at
    /// most one clamped click ever," not "one click per missed beat below
    /// some threshold").
    #[test]
    fn a_large_forward_jump_still_clicks_at_most_once() {
        let mut dj = DjCore::default();
        let t0 = Instant::now();
        dj.observe_beat_sync(BeatRef::new(0.0, 2.0), t0, 0);
        dj.due_clicks(t0, H); // seeds next_beat = 1

        let due = dj.due_clicks(t0 + Duration::from_millis(2000), H); // cur = 4.0
        assert_eq!(due.offsets.len(), 1, "still exactly one click regardless of backlog size: {due:?}");
        assert!(due.offsets[0].is_zero());
    }

    #[test]
    fn a_stranded_next_beat_recovers_within_bounded_slack() {
        let mut dj = DjCore::default();
        let t0 = Instant::now();
        dj.observe_beat_sync(BeatRef::new(0.0, 2.0), t0, 0);
        dj.due_clicks(t0, H); // seeds next_beat = 1

        // Directly strand next_beat (private field, same-module test access —
        // simulates what a multi-beat backward phase walk would leave behind).
        dj.next_beat = Some(9);

        let due = dj.due_clicks(t0, H); // cur is still ~0.0
        assert!(due.offsets.is_empty(), "detecting + re-seeding a strand is silent");

        let recovered = dj.due_clicks(t0 + Duration::from_millis(300), H);
        assert_eq!(recovered.offsets.len(), 1, "recovered near cur, not still waiting for beat 9");
        assert!(near(recovered.offsets[0], 200.0));
    }

    #[test]
    fn a_legitimate_backward_phase_step_does_not_reseed() {
        let mut dj = DjCore::default();
        let t0 = Instant::now();
        dj.observe_beat_sync(BeatRef::new(10.0, 2.0), t0, 0);
        let seeded = dj.due_clicks(t0, H); // seeds next_beat = 11
        assert!(seeded.offsets.is_empty());

        let t1 = t0 + Duration::from_millis(100); // free-run would read 10.2
        dj.observe_beat_sync(BeatRef::new(9.0, 2.0), t1, 0); // steps back <= 1 beat

        let due = dj.due_clicks(t1, H);
        assert!(due.offsets.is_empty(), "no un-strand click from a legitimate <=1-beat step");
        assert_eq!(dj.next_beat, Some(11), "next_beat must not be reseeded by a legitimate step");
    }

    // ── next_wake ───────────────────────────────────────────────────────

    #[test]
    fn next_wake_is_none_without_a_phasor() {
        let dj = DjCore::default();
        assert_eq!(dj.next_wake(Instant::now(), H), None);
    }

    /// The horizon lead (2026-07-18 deliberation finding 3): waking AT the
    /// beat would schedule the click with ~zero ALSA lead — the wake must
    /// come as the beat ENTERS the horizon so `due_clicks` hands the sink a
    /// full-lead offset.
    #[test]
    fn next_wake_leads_the_next_beat_by_the_horizon() {
        let mut dj = DjCore::default();
        let t0 = Instant::now();
        dj.observe_beat_sync(BeatRef::new(0.0, 2.0), t0, 0); // 120 BPM: 0.5s/beat
        let wake = dj.next_wake(t0, H).expect("phasor is running");
        // Beat 1 sounds at t0+500ms; the wake aims at t0+500ms−250ms.
        assert!(near(wake.duration_since(t0), 250.0), "wakes as beat 1 enters the horizon");
        // And the offset a due-click check AT that wake would hand the sink
        // is the full horizon — the lead pre-scheduling exists to provide.
        let due = dj.due_clicks(wake, H);
        assert_eq!(due.offsets.len(), 1);
        assert!(near(due.offsets[0], 250.0), "the click is scheduled a full horizon ahead");
    }

    /// An overdue target clamps to `now` (never a past instant for
    /// `sleep_until` to spin on) — and the overdue-collapse policy then owns
    /// "now" at the next click-check.
    #[test]
    fn next_wake_clamps_an_overdue_beat_to_now() {
        let mut dj = DjCore::default();
        let t0 = Instant::now();
        dj.observe_beat_sync(BeatRef::new(0.0, 2.0), t0, 0);
        // Well past beat 1 with nothing ever scheduled.
        let t1 = t0 + Duration::from_millis(900);
        let wake = dj.next_wake(t1, H).expect("still running");
        assert_eq!(wake, t1, "an already-due beat wakes immediately, never in the past");
    }

    #[test]
    fn next_wake_advances_after_a_click_check_seeds_the_cursor() {
        let mut dj = DjCore::default();
        let t0 = Instant::now();
        dj.observe_beat_sync(BeatRef::new(0.0, 2.0), t0, 0);
        dj.due_clicks(t0 + Duration::from_millis(300), H); // schedules beat 1

        // After the point beat 1 has fired, the next wake should target beat
        // 2's horizon entry: beat 2 sounds at t0+1s, wake at t0+750ms.
        let t1 = t0 + Duration::from_millis(600);
        let wake = dj.next_wake(t1, H).expect("still running");
        assert!(near(wake.duration_since(t0), 750.0), "targets beat 2's horizon entry, got {:?}", wake);
    }

    // ── The free-run cap + the stale deadline (deliberation findings 2+5) ──

    /// Sustained `Touch` keeps liveness fresh forever while the phasor never
    /// corrects — without the cap, BeatGrid would trust an uncorrected,
    /// drifting phasor indefinitely (`MAX_FREE_RUN`'s doc).
    #[test]
    fn sustained_touch_past_the_free_run_cap_falls_back_even_with_fresh_liveness() {
        let mut dj = DjCore::default();
        let t0 = Instant::now();
        dj.observe_beat_sync(BeatRef::new(0.0, 2.0), t0, 0); // anchoring Fold
        assert_eq!(dj.mode(), ClockMode::BeatGrid);

        // Touch-aged refs (2s stale by their own stamp) arriving at t0+6s and
        // t0+9s: liveness stays fresh, the phasor is never corrected again.
        let base: u64 = 100_000_000_000;
        for at_secs in [6u64, 9] {
            let obs = dj.observe_beat_sync(
                BeatRef { beat: 0.0, tempo_bps: 2.0, epoch_ns: base },
                t0 + Duration::from_secs(at_secs),
                base + 2_000_000_000, // ref is 2s old → Touch
            );
            assert_eq!(obs.transition, None, "Touch holds BeatGrid");
        }
        assert_eq!(dj.mode(), ClockMode::BeatGrid);

        // A click-check at t0+10.5s: liveness is 1.5s fresh (well within
        // REF_STALE_MAX) but the last FOLD is 10.5s old — past MAX_FREE_RUN.
        let due = dj.due_clicks(t0 + Duration::from_millis(10_500), H);
        assert_eq!(dj.mode(), ClockMode::Wallclock);
        assert_eq!(
            due.transition,
            Some(ClockTransition {
                to: ClockMode::Wallclock,
                reason: TransitionReason::FreeRunCap
            }),
            "fresh liveness must not outlive an uncorrected phasor"
        );
        assert!(due.offsets.is_empty(), "no clicks from a phasor just declared untrusted");
    }

    #[test]
    fn next_stale_deadline_is_none_in_wallclock_and_the_binding_ladder_in_beat_grid() {
        let mut dj = DjCore::default();
        assert_eq!(dj.next_stale_deadline(), None, "nothing to fall back from in Wallclock");

        let t0 = Instant::now();
        dj.observe_beat_sync(BeatRef::new(0.0, 2.0), t0, 0);
        // Freshly anchored: stale (t0+5s) binds before the cap (t0+10s).
        assert_eq!(dj.next_stale_deadline(), Some(t0 + REF_STALE_MAX));

        // A Touch at t0+7s pushes the stale deadline to t0+12s — now the
        // free-run cap (t0+10s) is the binding rung.
        let base: u64 = 100_000_000_000;
        dj.observe_beat_sync(
            BeatRef { beat: 0.0, tempo_bps: 2.0, epoch_ns: base },
            t0 + Duration::from_secs(7),
            base + 2_000_000_000,
        );
        assert_eq!(
            dj.next_stale_deadline(),
            Some(t0 + MAX_FREE_RUN),
            "sustained Touch hands the deadline to the free-run cap"
        );
    }

    #[test]
    fn ref_fold_max_is_tighter_than_stale_max_boundary_sanity() {
        // Pure sanity check that this file's staleness reasoning rests on:
        // if this ever inverted, the Touch band would be empty or negative.
        assert!(REF_FOLD_MAX < REF_STALE_MAX);
    }

    // ── MetronomeConfig: shipped-default + typo-rejection (ported from the
    // deleted metronome.rs — this module is now the SOLE owner of the type,
    // so this coverage would otherwise be lost entirely, not merely
    // duplicated) ───────────────────────────────────────────────────────

    /// The shipped `metronome.toml` seed must deserialize to exactly the
    /// compiled-in `MetronomeConfig::default()` — otherwise a fresh client and a
    /// no-config client would click differently. Partial files fill from default.
    #[test]
    fn config_parses_the_shipped_default_and_fills_partials() {
        let shipped: MetronomeConfig =
            toml::from_str(include_str!("../../../../assets/defaults/metronome.toml"))
                .expect("shipped metronome.toml parses");
        assert_eq!(shipped, MetronomeConfig::default(), "seed must match the Default impl");

        let partial: MetronomeConfig = toml::from_str("note = 60\n").expect("partial parses");
        assert_eq!(partial.note, 60);
        assert_eq!(partial.channel, MetronomeConfig::default().channel);
        assert_eq!(partial.velocity, MetronomeConfig::default().velocity);
    }

    /// A typo must fail loud, not silently default: `deny_unknown_fields` turns
    /// `volume` (meant `velocity`) into a parse error the apply path logs.
    #[test]
    fn config_rejects_a_typo_rather_than_silently_defaulting() {
        assert!(toml::from_str::<MetronomeConfig>("volume = 90\n").is_err());
    }
}
