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

use std::time::{Duration, Instant};

use bevy::prelude::*;
use kaijutsu_audio::{LocalBeat, RENDER_FLUSH_MIME};
use kaijutsu_client::ServerEvent;

use crate::connection::actor_plugin::ServerEventMessage;

/// The click pitch — a high, short note (C6) on the sink's dedicated metronome
/// channel. A pitched click, not a drum: GM channel-9 percussion is silent under
/// game soundfonts (the FF4 one on zorak), so `MidiSink::click_at` gates a
/// melodic note instead. C6 reads as a crisp 拍子木-like tick.
const CLICK_NOTE: u8 = 84;

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
        // Ingest references first, then click off the (freshly corrected) phasor.
        app.add_systems(Update, (ingest_beat_signals, click_on_beat).chain());
    }
}

/// The app's continuous local beat: a phasor slaved to the kernel's low-rate
/// references, plus the last position we clicked from (so a beat crossing fires
/// exactly once). A single followed beat — the standalone slice tracks one
/// rolling track; a later cut keys per track/score context.
#[derive(Resource)]
pub struct Metronome {
    /// The phasor, once a first reference has anchored it.
    beat: Option<LocalBeat>,
    /// The next integer beat not yet scheduled into the sink queue (monotonic;
    /// each beat is scheduled exactly once, ahead of time).
    next_beat: Option<i64>,
    /// Whether the audible click sounds. Auto-emit of references is always-on in
    /// the kernel; the click is the sink's opt-in (default on for this slice; a
    /// keybind toggle is future work).
    pub enabled: bool,
}

impl Default for Metronome {
    fn default() -> Self {
        Self { beat: None, next_beat: None, enabled: true }
    }
}

impl Metronome {
    /// Fold a reference into the phasor (creating it on the first one). `next_beat`
    /// is left untouched — it seeds lazily from the phasor in [`schedule_due`] and
    /// stays monotonic across corrections (which keep position continuous).
    fn observe(&mut self, reference: kaijutsu_audio::BeatRef, at: Instant) {
        match &mut self.beat {
            Some(beat) => beat.observe(reference, at),
            None => self.beat = Some(LocalBeat::new(reference, at)),
        }
    }

    /// Halt the metronome: drop the phasor so it stops free-running (and stops
    /// scheduling clicks). Called on a transport flush (stop/pause) — the phasor
    /// can't distinguish "clock stopped" from "gap between low-rate references"
    /// on its own, so the flush is the explicit stop signal. A new reference
    /// (the next `play`) re-anchors it.
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
    fn schedule_due(&mut self, now: Instant, horizon: Duration) -> Vec<Duration> {
        let Some(beat) = &self.beat else {
            return Vec::new();
        };
        let cur = beat.position(now);
        let tempo = beat.tempo_bps();
        if tempo <= 0.0 {
            return Vec::new();
        }
        // First call: start at the next whole beat after the anchor (don't
        // retro-fire the beat we anchored on).
        let mut next = self.next_beat.unwrap_or_else(|| cur.floor() as i64 + 1);
        let horizon_secs = horizon.as_secs_f64();
        let mut offsets = Vec::new();
        loop {
            let secs = (next as f64 - cur) / tempo;
            if secs > horizon_secs {
                break;
            }
            // Clamp a just-missed beat to "now" rather than scheduling the past.
            offsets.push(Duration::from_secs_f64(secs.max(0.0)));
            next += 1;
        }
        self.next_beat = Some(next);
        offsets
    }
}

/// Drive the phasor from the transport signals on the event stream: fold every
/// `BeatSync` reference in (`receipt` is `Instant::now()` at frame time — the
/// same re-anchor-at-receipt the render sinks use for `lead`), and **reset on a
/// `RENDER_FLUSH` cue** (stop/pause) so the metronome halts instead of
/// free-running past the end of the take.
fn ingest_beat_signals(
    mut messages: MessageReader<ServerEventMessage>,
    mut metronome: ResMut<Metronome>,
) {
    let now = Instant::now();
    for ServerEventMessage(event) in messages.read() {
        match event {
            ServerEvent::BeatSync { beat_ref, .. } => metronome.observe(*beat_ref, now),
            ServerEvent::RenderCue { cue, .. } if cue.mime == RENDER_FLUSH_MIME => {
                metronome.reset()
            }
            _ => {}
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
    if !metronome.enabled {
        return;
    }
    for offset in metronome.schedule_due(Instant::now(), SCHEDULE_HORIZON) {
        sink.click_at(CLICK_NOTE, offset);
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
    fn a_stalled_frame_catches_up_without_replaying_the_past() {
        // A long gap (a stalled frame) schedules every beat that came due in it,
        // each once, clamping an already-past beat to now (offset 0) rather than
        // scheduling into the past.
        let mut m = Metronome::default();
        let t0 = Instant::now();
        m.observe(BeatRef::new(0.0, 2.0), t0);
        m.schedule_due(t0, H); // start scheduling (seeds next_beat = 1)
        // Now a long stall to 1.6 s: beats 1 (0.5s), 2 (1.0s), 3 (1.5s) all came
        // due during the gap and must each be scheduled once, clamped to now.
        let due = m.schedule_due(t0 + Duration::from_millis(1600), H);
        assert_eq!(due.len(), 3, "beats 1,2,3 each scheduled once: {due:?}");
        assert!(due.iter().all(|d| d.as_secs_f64() < 0.001), "past beats clamp to now");
        // Nothing replays on the next call.
        assert!(m.schedule_due(t0 + Duration::from_millis(1650), H).is_empty());
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
            cue: RenderCue { mime: RENDER_FLUSH_MIME.into(), payload: CuePayload::Inline(vec![]), lead: Duration::ZERO },
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
}
