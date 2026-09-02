//! Per-track beat phasors — the beat made visible.
//!
//! Stance: *distribute tempo, not pulses* (`docs/midi.md`). The kernel ships
//! low-rate beat references keyed by a track's **score context**; each becomes
//! a local phasor here, and the pulse a client draws is derived from that
//! phasor every frame. Nothing streams per-beat over the wire.
//!
//! [`WellBeats`] holds one phasor per rolling track and answers three
//! questions a renderer asks: the beat envelope for one track
//! ([`WellBeats::envelope`]), the raw beat position for scroll math that must
//! freeze exactly when a track stops ([`WellBeats::beat_position`]), and the
//! loudest envelope across every track ([`WellBeats::global_envelope`]).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use kaijutsu_audio::{BeatRef, LocalBeat, Slew};
use kaijutsu_types::ContextId;

/// Beat-envelope decay: `exp(-DECAY × beat_fraction)` — 1.0 on the beat,
/// ~0.08 by the half-beat. Sharp enough to read as a pulse, soft enough not
/// to strobe. **Amy-tunable.**
const BEAT_ENVELOPE_DECAY: f64 = 5.0;

/// Drop a phasor that hasn't seen a reference for this long. References
/// arrive every 8 beats while a clock rolls (4s at 120 BPM), and stop/pause
/// sends an explicit flush — this guard only catches the abnormal paths
/// (kernel restart mid-play, dropped stream) so a dead track can't pulse
/// forever.
pub const PHASOR_STALE: Duration = Duration::from_secs(30);

/// A phasor + when it last saw a wire reference (staleness guard).
struct Phasor {
    beat: LocalBeat,
    last_ref: Instant,
}

/// Per-track beat phasors, keyed by the track's **score context** (the id
/// both `BeatSync` and `RenderCue` carry). Multi-track from day one — this is
/// the generalization `metronome.rs` deferred.
#[derive(Default)]
pub struct WellBeats {
    phasors: HashMap<ContextId, Phasor>,
}

/// Pure envelope shape: 1.0 on the beat, decaying exponentially through the
/// beat. `frac` is the fractional position within the current beat (0..1).
pub fn beat_envelope_at(frac: f64) -> f32 {
    (-BEAT_ENVELOPE_DECAY * frac).exp() as f32
}

impl WellBeats {
    /// Fold a wire reference into the phasor for `ctx` (anchoring on first).
    ///
    /// `at` is the reference's own back-dated emission instant
    /// (`BeatRef::backdated_at`) — what the phasor folds against, so a flood
    /// of buffered refs settles at the newest ref's true position instead of
    /// walking several beats at one shared receipt `now`. `received` is the
    /// *actual* receipt instant, separate from `at`: it is what `last_ref`
    /// (the [`Self::prune_stale`] liveness clock) stamps. These differ on
    /// purpose — folding at a back-dated `at` can leave `at` seconds behind
    /// `received` on a delivery flood, and stamping liveness from the
    /// (older) `at` instead of the (fresher) `received` would let
    /// `prune_stale` kill a phasor that is, in wall-clock reality, still
    /// live (a sustained backlog of old-but-not-stale refs proves the track
    /// is alive even while every individual ref reads a bit behind).
    /// Returns the [`Slew`] report on an ongoing correction (the Slice 4
    /// telemetry rider records it, `consumer=time_well`); `None` on the
    /// anchoring first observe for this context (no prior phasor state to
    /// report a slew against).
    pub fn observe(
        &mut self,
        ctx: ContextId,
        reference: BeatRef,
        at: Instant,
        received: Instant,
    ) -> Option<Slew> {
        match self.phasors.get_mut(&ctx) {
            Some(p) => {
                let slew = p.beat.observe(reference, at);
                p.last_ref = received;
                Some(slew)
            }
            None => {
                self.phasors.insert(
                    ctx,
                    Phasor { beat: LocalBeat::new(reference, at), last_ref: received },
                );
                None
            }
        }
    }

    /// Bump the liveness clock for an EXISTING phasor without touching its
    /// beat position — the arm for a reference that arrived but was too
    /// stale to fold (`BeatRef::backdated_at` returned `None`). A stale ref
    /// still proves the track is alive (something arrived), so `prune_stale`
    /// must not reap it; but folding it would anchor the phasor's position in
    /// the past, so the beat itself is left untouched. A no-op if `ctx` has
    /// no phasor yet — there is nothing to keep alive, and this must never
    /// create one (that would anchor a fresh phasor with no position at all).
    pub fn touch(&mut self, ctx: &ContextId, received: Instant) {
        if let Some(p) = self.phasors.get_mut(ctx) {
            p.last_ref = received;
        }
    }

    /// Transport flush (stop/pause): drop the phasor so the pulse halts —
    /// same contract as `Metronome::reset`, but per track.
    pub fn reset(&mut self, ctx: &ContextId) {
        self.phasors.remove(ctx);
    }

    /// Drop phasors that stopped receiving references without a flush
    /// (kernel restart, dropped stream) so a dead track can't pulse forever.
    pub fn prune_stale(&mut self, now: Instant) {
        self.phasors
            .retain(|_, p| now.duration_since(p.last_ref) < PHASOR_STALE);
    }

    /// The beat envelope (0..1) for the phasor keyed by `ctx`; 0.0 when no
    /// track is rolling under that key.
    pub fn envelope(&self, ctx: &ContextId, now: Instant) -> f32 {
        self.envelope_and_frac(ctx, now).0
    }

    /// The beat envelope plus the fractional position within the current beat
    /// (0..1 — the track-ray pulse's position along the beam); `(0.0, 0.0)`
    /// when no track is rolling under that key.
    pub fn envelope_and_frac(&self, ctx: &ContextId, now: Instant) -> (f32, f32) {
        self.phasors
            .get(ctx)
            .map(|p| {
                let pos = p.beat.position(now);
                let frac = pos - pos.floor();
                (beat_envelope_at(frac), frac as f32)
            })
            .unwrap_or((0.0, 0.0))
    }

    /// The phasor's raw beat position (unbounded, NOT wrapped to `0..1` the
    /// way [`Self::envelope_and_frac`]'s `frac` is) for the track keyed by
    /// `ctx` — `None` when no phasor is live under that key. This is the
    /// **freeze signal** the tracker station's scroll math anchors on
    /// (`tracker::grid::row_offset`'s `p` argument): `Some` while a track's
    /// clock is rolling, `None` the instant a transport flush drops the
    /// phasor ([`Self::reset`]). A caller scrolling rows on this position
    /// caches the last `Some` value and simply stops writing on `None` —
    /// exact freeze, not a fallback to `0.0` (which would snap the grid back
    /// to the playhead instead of holding still).
    pub fn beat_position(&self, ctx: &ContextId, now: Instant) -> Option<f64> {
        self.phasors.get(ctx).map(|p| p.beat.position(now))
    }

    /// The loudest envelope across every rolling track — the well's shared
    /// heartbeat (phase across independent clock domains is meaningless, so
    /// max, not sum).
    pub fn global_envelope(&self, now: Instant) -> f32 {
        self.phasors
            .values()
            .map(|p| {
                let pos = p.beat.position(now);
                beat_envelope_at(pos - pos.floor())
            })
            .fold(0.0, f32::max)
    }

    /// Whether any track's clock is rolling (has a live phasor). A liveness
    /// probe for tests and status readouts, not a render input: a client
    /// showing "playing" reads the track roster's own flag, because a paused
    /// track still has a roster row and no phasor.
    pub fn any_rolling(&self) -> bool {
        !self.phasors.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(n: u8) -> ContextId {
        ContextId::from_bytes([n, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1])
    }

    #[test]
    fn envelope_peaks_on_the_beat_and_decays_through_it() {
        assert!((beat_envelope_at(0.0) - 1.0).abs() < 1e-6);
        assert!(beat_envelope_at(0.1) > beat_envelope_at(0.5));
        assert!(beat_envelope_at(0.9) < 0.05, "quiet by the next beat");
    }

    #[test]
    fn phasor_envelope_follows_position_and_reset_silences_it() {
        let mut beats = WellBeats::default();
        let t0 = Instant::now();
        assert_eq!(beats.envelope(&ctx(1), t0), 0.0, "no phasor yet");

        // 120 BPM (2 beats/sec): anchor exactly on a beat.
        beats.observe(ctx(1), BeatRef::new(8.0, 2.0), t0, t0);
        let on_beat = beats.envelope(&ctx(1), t0);
        assert!((on_beat - 1.0).abs() < 1e-3, "on the beat: {on_beat}");

        let off_beat = beats.envelope(&ctx(1), t0 + Duration::from_millis(250));
        assert!(off_beat < on_beat, "decays mid-beat: {off_beat}");

        // Next beat (500ms) peaks again.
        let next = beats.envelope(&ctx(1), t0 + Duration::from_millis(500));
        assert!(next > off_beat, "re-peaks on the next beat: {next}");

        beats.reset(&ctx(1));
        assert_eq!(beats.envelope(&ctx(1), t0), 0.0, "flush silences the pulse");
    }

    #[test]
    fn beat_position_is_some_while_rolling_and_advances() {
        let mut beats = WellBeats::default();
        let t0 = Instant::now();
        assert_eq!(beats.beat_position(&ctx(1), t0), None, "no phasor yet");

        beats.observe(ctx(1), BeatRef::new(0.0, 2.0), t0, t0);
        let p0 = beats.beat_position(&ctx(1), t0).expect("phasor now live");
        assert!((p0 - 0.0).abs() < 1e-6, "anchored at beat 0: {p0}");

        let p1 = beats
            .beat_position(&ctx(1), t0 + Duration::from_millis(500))
            .expect("still rolling");
        assert!(p1 > p0, "advances with time: {p0} -> {p1}");
    }

    #[test]
    fn beat_position_is_none_after_reset() {
        let mut beats = WellBeats::default();
        let t0 = Instant::now();
        beats.observe(ctx(1), BeatRef::new(0.0, 2.0), t0, t0);
        assert!(beats.beat_position(&ctx(1), t0).is_some());

        beats.reset(&ctx(1));
        assert_eq!(beats.beat_position(&ctx(1), t0), None, "flush drops the phasor");
    }

    #[test]
    fn global_envelope_is_the_loudest_track() {
        let mut beats = WellBeats::default();
        let t0 = Instant::now();
        assert_eq!(beats.global_envelope(t0), 0.0);
        // Track A anchored on-beat, track B anchored mid-beat.
        beats.observe(ctx(1), BeatRef::new(4.0, 2.0), t0, t0);
        beats.observe(ctx(2), BeatRef::new(4.5, 2.0), t0, t0);
        let g = beats.global_envelope(t0);
        let a = beats.envelope(&ctx(1), t0);
        assert!((g - a).abs() < 1e-6, "global = loudest (on-beat) track");
        assert!(beats.any_rolling());
    }

    #[test]
    fn stale_phasor_is_pruned_without_a_flush() {
        let mut beats = WellBeats::default();
        let t0 = Instant::now();
        beats.observe(ctx(1), BeatRef::new(0.0, 2.0), t0, t0);
        beats.prune_stale(t0 + PHASOR_STALE / 2);
        assert!(beats.any_rolling(), "fresh phasor survives");
        beats.prune_stale(t0 + PHASOR_STALE + Duration::from_secs(1));
        assert!(!beats.any_rolling(), "stale phasor dropped");
    }

    /// A stale-but-received reference (`backdated_at` returned `None`) still
    /// proves the track is alive — `touch` must bump the liveness clock (so
    /// `prune_stale` doesn't reap a phasor that's still receiving, just
    /// receiving old references) WITHOUT moving the beat position (folding a
    /// stale ref would anchor the phasor in the past).
    #[test]
    fn touch_keeps_a_phasor_alive_without_moving_its_position() {
        let mut beats = WellBeats::default();
        let t0 = Instant::now();
        beats.observe(ctx(1), BeatRef::new(0.0, 2.0), t0, t0);
        let p_before = beats.beat_position(&ctx(1), t0).expect("phasor live");

        // Without a touch, the phasor goes stale at PHASOR_STALE.
        let t_touch = t0 + PHASOR_STALE - Duration::from_millis(1);
        beats.touch(&ctx(1), t_touch);
        // Now well past the ORIGINAL anchor's staleness window, but only
        // just past the touch — must survive because touch reset the clock.
        beats.prune_stale(t_touch + PHASOR_STALE / 2);
        assert!(beats.any_rolling(), "touch kept the phasor alive past the original window");

        let p_after = beats.beat_position(&ctx(1), t0).expect("still live");
        assert_eq!(p_after, p_before, "touch must not move the beat position");

        // Far enough past the touch, it still eventually prunes.
        beats.prune_stale(t_touch + PHASOR_STALE + Duration::from_secs(1));
        assert!(!beats.any_rolling(), "touch delays but does not prevent eventual pruning");
    }

    /// `touch` on a context with no phasor is a no-op — it must never create
    /// one (that would anchor a fresh phasor with no beat position at all).
    #[test]
    fn touch_on_an_unknown_context_is_a_no_op() {
        let mut beats = WellBeats::default();
        let t0 = Instant::now();
        beats.touch(&ctx(1), t0);
        assert!(!beats.any_rolling(), "touch must not create a phasor");
    }
}
