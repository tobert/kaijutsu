//! The metronome — the app's continuous local timebase made audible
//! (`docs/midi.md` "The relative-lead timebase, analyzed").
//!
//! COMPOSES alongside the per-cue render path (`midi.rs`/`audio.rs`), it does not
//! replace it: the render cues own *sound onset* (scheduled ahead on a lead); the
//! metronome owns *"where's the beat now"*. It holds a [`LocalBeat`] phasor
//! (`kaijutsu-audio`) that free-runs and *slews* toward the low-rate
//! [`ServerEvent::BeatSync`] references the kernel emits while a track's clock
//! rolls, and clicks a 拍子木 (wood-block) on channel 9 each time the phasor
//! crosses a beat — through the SAME ALSA seq port the render cues use.
//!
//! The click is phasor-driven (fired at the beat's *current* position, direct,
//! not scheduled on a lead), so this is the empirical validator of "good enough":
//! run a track and compare the click against the per-cue MIDI notes (`aseqdump`).

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bevy::prelude::*;
use kaijutsu_audio::{LocalBeat, RefDisposition, Slew, RENDER_FLUSH_MIME};
use kaijutsu_client::{ConnectionStatus, ServerEvent};

use crate::connection::actor_plugin::{ConnectionStatusMessage, ServerEventMessage};

/// The click's sound + gate, resolved from the per-client `metronome.toml`
/// (`docs/config-crdt-ownership.md` "Per-client config"). Serde `default` makes
/// every field optional in the TOML and falls back to the shipped 拍子木 click,
/// so a partial file is valid and a missing/failed fetch keeps the default.
///
/// The click is a *pitched* note on a dedicated channel, not a drum: GM
/// channel-9 percussion is silent under game soundfonts (the FF4 one on zorak),
/// so `MidiSink::click_at` gates a melodic note instead. C6 (84) reads as a
/// crisp tick.
#[derive(serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct MetronomeConfig {
    /// Whether the click sounds at all (on top of the always-on "only while a
    /// live clock rolls" gate).
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
        // Must match assets/defaults/metronome.toml (the embedded seed).
        Self { enabled: true, note: 84, channel: 15, velocity: 110, gate_ms: 60 }
    }
}

/// How far ahead the phasor pre-schedules clicks into the ALSA queue. Must exceed
/// the app's frame interval so a beat is always queued *before* it sounds — the
/// click then lands at the ALSA-precise predicted time, independent of the
/// (irregular) Bevy frame cadence. Comfortably under one beat so a low-rate
/// reference's gentle slew barely moves an already-queued click.
const SCHEDULE_HORIZON: Duration = Duration::from_millis(250);

/// Consumes `ServerEvent::BeatSync` into a local phasor and clicks the beat.
pub struct MetronomePlugin;

impl Plugin for MetronomePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Metronome>();
        // Ingest references, halt on any loss of the live clock (flush handled in
        // ingest; connection drop handled here — reset wins over a same-frame
        // reference), then click off the (freshly corrected) phasor.
        app.add_systems(
            Update,
            (ingest_beat_signals, halt_on_connection_loss, click_on_beat).chain(),
        );
        // Per-client click config arrives over RPC on (re)connect; apply it
        // independently of the beat pipeline.
        app.add_systems(Update, apply_metronome_config);
    }
}

/// The app's continuous local beat: a phasor slaved to the kernel's low-rate
/// references, plus the last position we clicked from (so a beat crossing fires
/// exactly once). A single followed beat — the standalone slice tracks one
/// rolling track; a later cut keys per track/score context.
#[derive(Resource, Default)]
pub struct Metronome {
    /// The phasor, once a first reference has anchored it.
    beat: Option<LocalBeat>,
    /// The next integer beat not yet scheduled into the sink queue (monotonic;
    /// each beat is scheduled exactly once, ahead of time).
    next_beat: Option<i64>,
    /// Sound + gate for the click, from the per-client `metronome.toml` (applied
    /// by [`apply_metronome_config`]; the compiled-in default until it arrives).
    pub config: MetronomeConfig,
}

impl Metronome {
    /// Fold a reference into the phasor (creating it on the first one). `next_beat`
    /// is left untouched — it seeds lazily from the phasor in [`schedule_due`] and
    /// stays monotonic across corrections (which keep position continuous).
    /// Returns the [`Slew`] report on an ongoing correction (the Slice 4
    /// telemetry rider records it); `None` on the anchoring first observe
    /// (there's no prior phasor state to report a slew against).
    fn observe(&mut self, reference: kaijutsu_audio::BeatRef, at: Instant) -> Option<Slew> {
        match &mut self.beat {
            Some(beat) => Some(beat.observe(reference, at)),
            None => {
                self.beat = Some(LocalBeat::new(reference, at));
                None
            }
        }
    }

    /// Halt the metronome: drop the phasor so it stops free-running (and stops
    /// scheduling clicks). The phasor can't distinguish "clock stopped" from
    /// "gap between low-rate references" on its own, so it needs an explicit
    /// stop signal. Two callers provide one: a transport flush (graceful
    /// stop/pause) and a connection drop (kernel restart/crash/network — the
    /// kernel is *gone* and can send no flush, yet its references stop). A new
    /// reference (the next `play`, after reconnect) re-anchors it.
    fn reset(&mut self) {
        self.beat = None;
        self.next_beat = None;
    }

    /// Return the offsets-from-`now` at which to schedule a click for every
    /// integer beat whose predicted time falls within `horizon` — each beat
    /// returned exactly once across calls. Pure (no audio), so the schedule is
    /// unit-testable without ALSA. Scheduling *ahead* into the device queue is
    /// what makes the click land at the phasor's beat time rather than at the
    /// irregular frame that noticed it (docs/midi.md real-time stance).
    ///
    /// Policy: a metronome never stacks clicks, and never silences longer than
    /// a bounded slack.
    ///
    /// - **Never replay**: if `next_beat` has fallen at-or-behind the current
    ///   floor (a stalled frame, or several beats came due while nothing
    ///   polled), the WHOLE missed backlog collapses to exactly ONE
    ///   clamped-to-now click and `next_beat` skips straight to `floor + 1` —
    ///   missed beats are missed, not replayed (that replay was the burst:
    ///   `a_stalled_frame_catches_up_without_replaying_the_past` encoded the
    ///   old, wrong policy and was rewritten, not preserved). The loop below
    ///   then only ever schedules the *future*, so it needs no `.max(0.0)`
    ///   clamp of its own — the overdue branch owns "now".
    /// - **Un-strand**: if `next_beat` has instead drifted too far *ahead* of
    ///   `cur` (a backward phase lurch stranded it beyond the horizon — the
    ///   starve half of the bug), re-seed to `floor + 1` rather than sit
    ///   silent until wall-clock time arithmetically closes the whole gap.
    ///   The slack (`max_ahead = horizon_secs * tempo + 2.0` beats) is sized
    ///   so a legitimate ≤1-beat backward phase step (`LocalBeat::max_step`)
    ///   never trips it — that would double-click a beat already sitting in
    ///   the ALSA queue. A genuine strand still recovers within
    ///   `max_ahead / tempo` (≈1.25 s at 120 BPM), not the 5–6 s the live bug
    ///   measured.
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
        // First call: start at the next whole beat after the anchor (don't
        // retro-fire the beat we anchored on).
        let mut next = self.next_beat.unwrap_or_else(|| cur.floor() as i64 + 1);
        let mut offsets = Vec::new();

        if (next as f64) <= cur.floor() {
            // Overdue: the entire backlog is one click, clamped to now.
            offsets.push(Duration::ZERO);
            next = cur.floor() as i64 + 1;
        } else {
            let max_ahead = horizon_secs * tempo + 2.0;
            if next as f64 - cur > max_ahead {
                // Stranded: re-seed near cur instead of waiting out the gap.
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
}

/// Drive the phasor from the transport signals on the event stream: fold every
/// `BeatSync` reference in per its [`RefDisposition`] — `Fold` (fresh, age ≤
/// `REF_FOLD_MAX`, or unstamped) is back-dated to its own emission instant and
/// `observe`d (a delivery flood — several refs queued behind a turn's
/// streamed output, arriving in one frame — would otherwise walk the phasor
/// several beats at once if all folded at one shared frame `now`, the burst);
/// `Touch` (older, but within `REF_STALE_MAX`) is a no-op here — the
/// metronome is a single phasor with no separate liveness clock to bump, so
/// there's nothing to touch, just a debug trace; `Drop` (stale past
/// `REF_STALE_MAX`) is also a debug no-op — the phasor free-runs on its last
/// feedforward tempo rather than snapping backward. **Reset on a
/// `RENDER_FLUSH` cue** (stop/pause) so the metronome halts instead of
/// free-running past the end of the take.
fn ingest_beat_signals(
    mut messages: MessageReader<ServerEventMessage>,
    mut metronome: ResMut<Metronome>,
) {
    let now = Instant::now();
    let now_epoch_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    for ServerEventMessage(event) in messages.read() {
        match event {
            ServerEvent::BeatSync { beat_ref, .. } => match beat_ref.disposition(now, now_epoch_ns)
            {
                RefDisposition::Fold(at) => {
                    if let Some(slew) = metronome.observe(*beat_ref, at) {
                        kaijutsu_telemetry::record_phasor_slew(
                            "metronome",
                            slew.error_beats,
                            slew.deadbanded,
                        );
                    }
                }
                RefDisposition::Touch => {
                    log::debug!(
                        "metronome: touch-only beat ref (single-phasor has no liveness clock)"
                    );
                }
                RefDisposition::Drop => {
                    log::debug!("metronome: dropped a stale beat ref");
                }
            },
            ServerEvent::RenderCue { cue, .. } if cue.mime == RENDER_FLUSH_MIME => {
                metronome.reset()
            }
            _ => {}
        }
    }
}

/// Halt the metronome the moment the connection leaves `Connected`. A kernel
/// restart, crash, or network drop stops the beat references *without* a
/// `RENDER_FLUSH` — the kernel is simply gone and can send no cue — so the flush
/// reset alone can't catch it, and the phasor would free-run, clicking onto
/// whatever synth is wired to the render port (the "clicks forever after a
/// kernel restart" live bug, `docs/issues.md`). Resetting on any non-`Connected`
/// status makes the metronome silent *during* the outage, not just after the
/// reconnect. The next `play` re-anchors it from a fresh `BeatSync`.
///
/// A brief blip mid-jam (Cooldown → Connected) costs at most one reference
/// interval of clicks — `BeatSync` resumes on the re-subscribed stream — a fair
/// trade for never emitting a phantom beat onto a live synth.
fn halt_on_connection_loss(
    mut status: MessageReader<ConnectionStatusMessage>,
    mut metronome: ResMut<Metronome>,
) {
    for ConnectionStatusMessage(s) in status.read() {
        if !matches!(s, ConnectionStatus::Connected { .. }) {
            metronome.reset();
        }
    }
}

/// Pre-schedule a 拍子木 click into the sink queue for every beat coming due
/// within the horizon — so ALSA fires it at the beat's predicted time, not at
/// this (irregular) frame.
fn click_on_beat(
    mut metronome: ResMut<Metronome>,
    mut sink: NonSendMut<crate::midi::MidiSink>,
) {
    let click = metronome.config;
    if !click.enabled {
        return;
    }
    for offset in metronome.schedule_due(Instant::now(), SCHEDULE_HORIZON) {
        sink.click_at(click.note, click.channel, click.velocity, click.gate_ms, offset);
        kaijutsu_telemetry::record_metronome_click();
    }
}

/// Apply a per-client `metronome.toml` fetched over RPC (the bootstrap sends it
/// as [`RpcResultMessage::MetronomeConfigReceived`], resolved through the
/// `/etc/client/<id>/…` → `/etc/client/…` cascade). A parse failure keeps the
/// current config and logs loudly — never a silent revert to the shipped click.
/// Config-change *push* (re-applying on a live `kj config set` without a
/// reconnect) is a follow-up; today it applies once per (re)connect.
fn apply_metronome_config(
    mut results: MessageReader<crate::connection::actor_plugin::RpcResultMessage>,
    mut metronome: ResMut<Metronome>,
) {
    use crate::connection::actor_plugin::RpcResultMessage;
    for result in results.read() {
        if let RpcResultMessage::MetronomeConfigReceived(toml) = result {
            match toml::from_str::<MetronomeConfig>(toml) {
                Ok(cfg) => {
                    log::info!("applied metronome config: {cfg:?}");
                    metronome.config = cfg;
                }
                Err(e) => log::error!("metronome.toml is unparseable: {e}; keeping current config"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_audio::BeatRef;

    const H: Duration = SCHEDULE_HORIZON; // 250 ms

    // Assert two Durations are within 1 ms (predicted schedule offsets).
    fn near(a: Duration, b_ms: f64) -> bool {
        (a.as_secs_f64() * 1000.0 - b_ms).abs() < 1.0
    }

    #[test]
    fn a_fresh_metronome_schedules_nothing() {
        let mut m = Metronome::default();
        assert!(m.schedule_due(Instant::now(), H).is_empty(), "no reference yet");
    }

    #[test]
    fn the_first_reference_does_not_retro_schedule() {
        // A reference arriving at beat 100 must NOT queue clicks for beats 0..100
        // — scheduling starts at the next whole beat, and that beat is a full
        // period away (500 ms > 250 ms horizon), so nothing is due yet.
        let mut m = Metronome::default();
        let t0 = Instant::now();
        m.observe(BeatRef::new(100.0, 2.0), t0);
        assert!(m.schedule_due(t0, H).is_empty(), "anchoring queues nothing");
    }

    #[test]
    fn beats_are_scheduled_once_at_their_predicted_offset() {
        let mut m = Metronome::default();
        let t0 = Instant::now();
        m.observe(BeatRef::new(0.0, 2.0), t0); // 120 BPM: a beat every 0.5 s
        // At t0 the next beat (1) is 500 ms away — outside the 250 ms horizon.
        assert!(m.schedule_due(t0, H).is_empty());
        // 300 ms in, beat 1 (at t0+500ms) is 200 ms away → scheduled at +200 ms.
        let due = m.schedule_due(t0 + Duration::from_millis(300), H);
        assert_eq!(due.len(), 1);
        assert!(near(due[0], 200.0), "beat 1 lands 200 ms out, got {:?}", due[0]);
        // 350 ms in, beat 1 is already queued; beat 2 (at 1.0 s) is 650 ms away.
        assert!(m.schedule_due(t0 + Duration::from_millis(350), H).is_empty());
        // 800 ms in, beat 2 is 200 ms away → scheduled once.
        let due2 = m.schedule_due(t0 + Duration::from_millis(800), H);
        assert_eq!(due2.len(), 1, "beat 2 scheduled exactly once");
        assert!(near(due2[0], 200.0));
    }

    #[test]
    fn a_stalled_frame_clicks_at_most_once_never_a_burst() {
        // A long gap (a stalled frame) must NOT replay every beat that came due
        // in it — that was the burst bug (the OLD policy this test used to
        // encode: beats 1, 2, 3 each individually clamped to now and fired
        // back-to-back). The new policy collapses the whole missed backlog to
        // exactly ONE clamped-to-now click, then resumes cadence on the grid.
        let mut m = Metronome::default();
        let t0 = Instant::now();
        m.observe(BeatRef::new(0.0, 2.0), t0);
        m.schedule_due(t0, H); // start scheduling (seeds next_beat = 1)

        // A long stall to 1.6 s: beats 1 (0.5s), 2 (1.0s), 3 (1.5s) all came due
        // during the gap. At most one click, clamped to now.
        let due = m.schedule_due(t0 + Duration::from_millis(1600), H);
        assert_eq!(due.len(), 1, "the whole missed backlog collapses to one click: {due:?}");
        assert!(due[0].is_zero(), "the one click is clamped to now, not scheduled into the past");

        // Nothing replays on an immediate re-call.
        assert!(m.schedule_due(t0 + Duration::from_millis(1600), H).is_empty(), "no replay");

        // Cadence resumes on the normal grid: cur reads beat 3.2 at 1.6s, so
        // next_beat became 4 (due at 2.0s) — 150ms later, beat 4 is 250ms away
        // (right at the horizon edge) and gets scheduled exactly once, like any
        // ordinary upcoming beat.
        let resumed = m.schedule_due(t0 + Duration::from_millis(1750), H);
        assert_eq!(resumed.len(), 1, "cadence resumed on the grid, not still catching up");
        assert!(near(resumed[0], 250.0), "beat 4 lands at its normal predicted offset");
    }

    #[test]
    fn a_large_forward_jump_still_clicks_at_most_once() {
        // A bigger backlog than the 3-beat case above (here 8 beats missed)
        // must STILL collapse to exactly one click — the invariant is "at most
        // one clamped click ever", not "one click per missed beat below some
        // threshold".
        let mut m = Metronome::default();
        let t0 = Instant::now();
        m.observe(BeatRef::new(0.0, 2.0), t0);
        m.schedule_due(t0, H); // seeds next_beat = 1

        let due = m.schedule_due(t0 + Duration::from_millis(2000), H); // cur = 4.0
        assert_eq!(due.len(), 1, "still exactly one click regardless of backlog size: {due:?}");
        assert!(due[0].is_zero());
    }

    #[test]
    fn a_stranded_next_beat_recovers_within_bounded_slack() {
        // Mirrors the starve half of the live bug: a backward phase walk can
        // strand `next_beat` several beats ahead of `cur`. The OLD policy just
        // waited for wall-clock `cur` to arithmetically climb back up to
        // `next_beat` — measured live as 5-6 s of dead silence. The new
        // un-strand policy detects the gap (`next - cur > max_ahead`) and
        // re-seeds near `cur` instead, bounding recovery to roughly
        // `max_ahead / tempo` regardless of how far the strand was.
        let mut m = Metronome::default();
        let t0 = Instant::now();
        m.observe(BeatRef::new(0.0, 2.0), t0);
        m.schedule_due(t0, H); // seeds next_beat = 1

        // Directly strand next_beat 8 beats ahead of cur (private field,
        // testing from inside the module) — simulating what a multi-beat
        // backward walk would have left behind. At 2 bps this beat would be
        // 4.5 s away if we waited for cur to reach it arithmetically.
        m.next_beat = Some(9);

        // Detection: the strand itself must not emit a phantom click.
        let due = m.schedule_due(t0, H); // cur is still ~0.0
        assert!(due.is_empty(), "detecting + re-seeding a strand is silent, not a click");

        // Recovery lands on the ordinary grid near cur, nowhere near beat 9's
        // 4.5s-away arithmetic due time.
        let recovered = m.schedule_due(t0 + Duration::from_millis(300), H);
        assert_eq!(recovered.len(), 1, "recovered near cur, not still waiting for beat 9");
        assert!(near(recovered[0], 200.0), "beat 1 (re-seeded target) at its normal offset");
    }

    #[test]
    fn a_legitimate_backward_phase_step_does_not_reseed() {
        // LocalBeat's ordinary phase correction (bounded to `max_step`, ≤1
        // beat per reference by default) must NEVER trip the un-strand
        // re-seed — that would double-click a beat already sitting in the
        // ALSA device queue. The slack (`max_ahead ≈ 2.5 beats` at the test
        // horizon/tempo) is sized specifically to absorb this.
        let mut m = Metronome::default();
        let t0 = Instant::now();
        m.observe(BeatRef::new(10.0, 2.0), t0);
        let seeded = m.schedule_due(t0, H); // seeds next_beat = 11
        assert!(seeded.is_empty());

        let t1 = t0 + Duration::from_millis(100); // free-run would read 10.2
        // A reference 1 beat behind steps phase backward (bounded ≤1 beat).
        m.observe(BeatRef::new(9.0, 2.0), t1);

        let due = m.schedule_due(t1, H);
        assert!(due.is_empty(), "no un-strand click from a legitimate ≤1-beat step: {due:?}");
        assert_eq!(m.next_beat, Some(11), "next_beat must not be reseeded by a legitimate step");
    }

    /// The Bevy wiring: a `ServerEvent::BeatSync` message anchors the phasor.
    #[test]
    fn beat_sync_message_anchors_the_phasor() {
        use kaijutsu_types::ContextId;

        let mut app = App::new();
        app.init_resource::<Metronome>()
            .add_message::<ServerEventMessage>()
            .add_systems(Update, ingest_beat_signals);

        // No phasor before any reference.
        assert!(app.world().resource::<Metronome>().beat.is_none());

        app.world_mut().write_message(ServerEventMessage(ServerEvent::BeatSync {
            context_id: ContextId::new(),
            beat_ref: BeatRef::new(4.0, 2.0),
        }));
        app.update();

        let m = app.world().resource::<Metronome>();
        assert!(m.beat.is_some(), "a BeatSync message must anchor the phasor");
    }

    /// The Bevy-level sibling of the pure flood regression in
    /// `timebase.rs` (`a_flood_of_backdated_refs_folds_to_the_true_position...`):
    /// 4+ `BeatSync` messages all arrive in ONE frame (a delivery flood — refs
    /// queued behind a turn's streamed output, released together), each
    /// stamped with its TRUE emission time spread over the past several
    /// seconds. `ingest_beat_signals` must back-date each to its own emission
    /// instant rather than fold all four at this frame's one receipt `now` —
    /// the phasor should land near the NEWEST ref's true position, and the
    /// resulting `schedule_due` call must not burst multiple clicks trying to
    /// "catch up" through the backlog.
    #[test]
    fn a_flood_of_beat_sync_messages_settles_at_the_newest_ref_without_a_burst() {
        use kaijutsu_types::ContextId;

        let now_epoch_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos() as u64;
        let tempo = 2.0; // 120 BPM: one beat every 0.5s

        let mut app = App::new();
        app.init_resource::<Metronome>()
            .add_message::<ServerEventMessage>()
            .add_systems(Update, ingest_beat_signals);

        // Beats 2, 4, 6, 8 (one every 4s at 2 bps — 8 beats apart), stamped
        // with their true emission times: beat 8 is "now", beat 2 was 3s ago
        // (mirrors the pure timebase.rs regression exactly).
        let refs = [
            (2.0, 3_000_000_000u64),
            (4.0, 2_000_000_000u64),
            (6.0, 1_000_000_000u64),
            (8.0, 0u64),
        ];
        for (beat, age_ns) in refs {
            app.world_mut().write_message(ServerEventMessage(ServerEvent::BeatSync {
                context_id: ContextId::new(),
                beat_ref: BeatRef { beat, tempo_bps: tempo, epoch_ns: now_epoch_ns - age_ns },
            }));
        }
        app.update(); // ONE frame ingests all four

        let now = Instant::now();
        let pos = app.world().resource::<Metronome>().beat.as_ref().expect("anchored").position(now);
        assert!((pos - 8.0).abs() < 1.0, "phasor settles near the newest ref's beat, got {pos}");

        let mut m = app.world_mut().resource_mut::<Metronome>();
        let due = m.schedule_due(now, SCHEDULE_HORIZON);
        assert!(due.len() <= 1, "no burst from the flood: {due:?}");
    }

    /// A `BeatSync` in the `Touch` band (older than `REF_FOLD_MAX` but within
    /// `REF_STALE_MAX`) must NOT move the phasor — the metronome is a single
    /// phasor with no separate liveness clock, so a `Touch` disposition is a
    /// pure no-op here (the debug trace, nothing else). This is the harder
    /// half of "reject stale adjustments harder": a ref that used to fold
    /// (anything under the old 5s `REF_STALE_MAX`) now only folds within a
    /// tighter 1s window.
    #[test]
    fn a_touch_disposition_ref_does_not_move_the_phasor() {
        use kaijutsu_types::ContextId;

        let mut app = App::new();
        app.init_resource::<Metronome>()
            .add_message::<ServerEventMessage>()
            .add_systems(Update, ingest_beat_signals);

        // Anchor the phasor first with a fresh (Fold) reference.
        let now_epoch_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos() as u64;
        app.world_mut().write_message(ServerEventMessage(ServerEvent::BeatSync {
            context_id: ContextId::new(),
            beat_ref: BeatRef { beat: 0.0, tempo_bps: 2.0, epoch_ns: now_epoch_ns },
        }));
        app.update();
        let anchored_pos = {
            let m = app.world().resource::<Metronome>();
            m.beat.as_ref().expect("anchored").position(Instant::now())
        };

        // A ref 2s old — past REF_FOLD_MAX (1s), within REF_STALE_MAX (5s) —
        // reports a wildly different beat. If it folded, the phasor would
        // step toward 999.0; it must not.
        app.world_mut().write_message(ServerEventMessage(ServerEvent::BeatSync {
            context_id: ContextId::new(),
            beat_ref: BeatRef {
                beat: 999.0,
                tempo_bps: 2.0,
                epoch_ns: now_epoch_ns.saturating_sub(2_000_000_000),
            },
        }));
        app.update();

        let after_pos = {
            let m = app.world().resource::<Metronome>();
            m.beat.as_ref().expect("still anchored").position(Instant::now())
        };
        assert!(
            (after_pos - anchored_pos).abs() < 1.0,
            "a Touch-band ref must not step the phasor toward its beat: {anchored_pos} → {after_pos}"
        );
    }

    /// A transport flush (stop/pause) halts the metronome — otherwise the phasor
    /// free-runs past the end of the take and keeps clicking (the "note still
    /// playing after stop" bug). After a flush the phasor is dropped, so
    /// `schedule_due` queues nothing until the next `play` re-anchors it.
    #[test]
    fn a_flush_cue_stops_the_metronome() {
        use kaijutsu_audio::{CuePayload, RenderCue};
        use kaijutsu_types::ContextId;

        let mut m = Metronome::default();
        let t0 = Instant::now();
        m.observe(BeatRef::new(0.0, 2.0), t0);
        m.schedule_due(t0, H); // running: phasor anchored, next_beat seeded
        assert!(m.beat.is_some());

        // The flush arrives on the same stream as a RenderCue.
        let flush = ServerEvent::RenderCue {
            context_id: ContextId::new(),
            cue: RenderCue {
                mime: RENDER_FLUSH_MIME.into(),
                payload: CuePayload::Inline(vec![]),
                lead: Duration::ZERO,
                epoch_ns: 0,
            },
        };
        let mut app = App::new();
        app.insert_resource(m)
            .add_message::<ServerEventMessage>()
            .add_systems(Update, ingest_beat_signals);
        app.world_mut().write_message(ServerEventMessage(flush));
        app.update();

        let m = app.world().resource::<Metronome>();
        assert!(m.beat.is_none(), "flush drops the phasor");
        assert!(m.next_beat.is_none(), "flush clears the schedule cursor");
    }

    /// A connection drop (kernel restart/crash/network flake) halts the
    /// metronome even though no `RENDER_FLUSH` arrives — the kernel is gone, so
    /// it can send no cue, and without this the phasor free-runs onto whatever
    /// synth is wired (the "clicks forever after a kernel restart" live bug).
    /// Any non-`Connected` status is the halt signal.
    #[test]
    fn a_connection_drop_stops_the_metronome() {
        let mut m = Metronome::default();
        let t0 = Instant::now();
        m.observe(BeatRef::new(0.0, 2.0), t0);
        m.schedule_due(t0, H); // running: phasor anchored, next_beat seeded
        assert!(m.beat.is_some());

        let mut app = App::new();
        app.insert_resource(m)
            .add_message::<ConnectionStatusMessage>()
            .add_systems(Update, halt_on_connection_loss);
        // The kernel goes away: the FSM reports Closing (then Cooldown/Connecting).
        app.world_mut().write_message(ConnectionStatusMessage(ConnectionStatus::Closing {
            cause: "kernel restart".into(),
        }));
        app.update();

        let m = app.world().resource::<Metronome>();
        assert!(m.beat.is_none(), "a connection drop drops the phasor");
        assert!(m.next_beat.is_none(), "and clears the schedule cursor");
    }

    /// The healthy steady state must NOT silence the metronome: a `Connected`
    /// status (re-seeded on every actor change) leaves a running phasor running,
    /// so a live jam keeps clicking.
    #[test]
    fn a_connected_status_leaves_the_metronome_running() {
        use kaijutsu_types::KernelId;

        let mut m = Metronome::default();
        let t0 = Instant::now();
        m.observe(BeatRef::new(0.0, 2.0), t0);
        m.schedule_due(t0, H);
        assert!(m.beat.is_some());

        let mut app = App::new();
        app.insert_resource(m)
            .add_message::<ConnectionStatusMessage>()
            .add_systems(Update, halt_on_connection_loss);
        app.world_mut().write_message(ConnectionStatusMessage(ConnectionStatus::Connected {
            kernel_id: KernelId::new(),
            context_id: None,
            since_ms: 0,
        }));
        app.update();

        assert!(
            app.world().resource::<Metronome>().beat.is_some(),
            "a Connected status must not disturb a running phasor"
        );
    }

    /// The shipped `metronome.toml` seed must deserialize to exactly the
    /// compiled-in `MetronomeConfig::default()` — otherwise a fresh client and a
    /// no-config client would click differently. Partial files fill from default.
    #[test]
    fn config_parses_the_shipped_default_and_fills_partials() {
        let shipped: MetronomeConfig =
            toml::from_str(include_str!("../../../assets/defaults/metronome.toml"))
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

    /// The apply system folds a fetched `metronome.toml` into the resource.
    #[test]
    fn apply_metronome_config_updates_the_resource() {
        use crate::connection::actor_plugin::RpcResultMessage;

        let mut app = App::new();
        app.init_resource::<Metronome>()
            .add_message::<RpcResultMessage>()
            .add_systems(Update, apply_metronome_config);
        app.world_mut().write_message(RpcResultMessage::MetronomeConfigReceived(
            "enabled = false\nnote = 72\nchannel = 9\nvelocity = 40\ngate_ms = 30\n".to_string(),
        ));
        app.update();

        assert_eq!(
            app.world().resource::<Metronome>().config,
            MetronomeConfig { enabled: false, note: 72, channel: 9, velocity: 40, gate_ms: 30 },
        );
    }

    /// An unparseable body keeps the current config (loud log), never a silent
    /// revert — per the house fail-loud posture.
    #[test]
    fn apply_keeps_current_config_on_unparseable_toml() {
        use crate::connection::actor_plugin::RpcResultMessage;

        let mut app = App::new();
        app.init_resource::<Metronome>()
            .add_message::<RpcResultMessage>()
            .add_systems(Update, apply_metronome_config);
        app.world_mut().write_message(RpcResultMessage::MetronomeConfigReceived(
            "this is not valid toml =".to_string(),
        ));
        app.update();

        assert_eq!(
            app.world().resource::<Metronome>().config,
            MetronomeConfig::default(),
            "a parse failure must not zero out the config",
        );
    }
}
