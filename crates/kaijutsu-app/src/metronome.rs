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

use std::time::Instant;

use bevy::prelude::*;
use kaijutsu_audio::{beat_onsets_in, LocalBeat};
use kaijutsu_client::ServerEvent;

use crate::connection::actor_plugin::ServerEventMessage;

/// GM percussion note 76, "Hi Wood Block" — the 拍子木 click.
const CLICK_NOTE: u8 = 76;

/// Consumes `ServerEvent::BeatSync` into a local phasor and clicks the beat.
pub struct MetronomePlugin;

impl Plugin for MetronomePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Metronome>();
        // Ingest references first, then click off the (freshly corrected) phasor.
        app.add_systems(Update, (ingest_beat_sync, click_on_beat).chain());
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
    /// Phasor position at the previous tick — the low edge of the click window.
    last_pos: f64,
    /// Whether the audible click sounds. Auto-emit of references is always-on in
    /// the kernel; the click is the sink's opt-in (default on for this slice; a
    /// keybind toggle is future work).
    pub enabled: bool,
}

impl Default for Metronome {
    fn default() -> Self {
        Self { beat: None, last_pos: 0.0, enabled: true }
    }
}

impl Metronome {
    /// Fold a reference into the phasor (creating it on the first one). On
    /// creation, seed `last_pos` to the phasor's own position so the first tick
    /// doesn't fire a burst of onsets from zero.
    fn observe(&mut self, reference: kaijutsu_audio::BeatRef, at: Instant) {
        match &mut self.beat {
            Some(beat) => beat.observe(reference, at),
            None => {
                let beat = LocalBeat::new(reference, at);
                self.last_pos = beat.position(at);
                self.beat = Some(beat);
            }
        }
    }

    /// Advance the phasor to `now` and return the integer beat onsets crossed
    /// since the last tick (empty until a reference has anchored it). Pure — no
    /// audio — so the click cadence is unit-testable without ALSA.
    fn tick(&mut self, now: Instant) -> Vec<i64> {
        let Some(beat) = &self.beat else {
            return Vec::new();
        };
        let cur = beat.position(now);
        let onsets = beat_onsets_in(self.last_pos, cur);
        self.last_pos = cur;
        onsets
    }
}

/// Fold every `BeatSync` reference on the event stream into the phasor. `receipt`
/// is `Instant::now()` at frame time — the same re-anchor-at-receipt the render
/// sinks use for `lead` (frame-quantized, and that's the accepted tradeoff).
fn ingest_beat_sync(
    mut messages: MessageReader<ServerEventMessage>,
    mut metronome: ResMut<Metronome>,
) {
    let now = Instant::now();
    for ServerEventMessage(event) in messages.read() {
        if let ServerEvent::BeatSync { beat_ref, .. } = event {
            metronome.observe(*beat_ref, now);
        }
    }
}

/// Click the 拍子木 once for every beat the phasor crossed this frame.
fn click_on_beat(
    mut metronome: ResMut<Metronome>,
    mut sink: NonSendMut<crate::midi::MidiSink>,
) {
    if !metronome.enabled {
        return;
    }
    let onsets = metronome.tick(Instant::now());
    for _ in onsets {
        sink.click(CLICK_NOTE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_audio::BeatRef;
    use std::time::Duration;

    #[test]
    fn a_fresh_metronome_never_clicks() {
        let mut m = Metronome::default();
        assert!(m.tick(Instant::now()).is_empty(), "no reference yet → no click");
    }

    #[test]
    fn the_first_reference_anchors_without_a_click_burst() {
        // A reference arriving at beat 100 must NOT fire clicks for beats 0..100
        // — seeding last_pos to the phasor position prevents the burst.
        let mut m = Metronome::default();
        let t0 = Instant::now();
        m.observe(BeatRef::new(100.0, 2.0), t0);
        assert!(m.tick(t0).is_empty(), "anchoring is silent");
    }

    #[test]
    fn the_phasor_clicks_each_beat_crossing() {
        let mut m = Metronome::default();
        let t0 = Instant::now();
        m.observe(BeatRef::new(0.0, 2.0), t0); // 120 BPM: a beat every 0.5 s
        assert!(m.tick(t0).is_empty(), "anchored at beat 0, nothing crossed yet");
        // 0.6 s later the phasor is at 1.2 → beat 1 crossed.
        assert_eq!(m.tick(t0 + Duration::from_secs_f64(0.6)), vec![1]);
        // Another 0.5 s → 2.2 → beat 2.
        assert_eq!(m.tick(t0 + Duration::from_secs_f64(1.1)), vec![2]);
        // A slow frame that spans two beats fires both.
        assert_eq!(m.tick(t0 + Duration::from_secs_f64(2.1)), vec![3, 4]);
    }

    #[test]
    fn a_later_reference_keeps_the_beat_continuous() {
        // A correcting reference must not skip or double-fire a beat: the click
        // cadence stays monotone across an `observe`.
        let mut m = Metronome::default();
        let t0 = Instant::now();
        m.observe(BeatRef::new(0.0, 2.0), t0);
        let _ = m.tick(t0);
        let _ = m.tick(t0 + Duration::from_secs_f64(0.6)); // clicked beat 1
        // A reference at ~beat 2 arrives slightly ahead of our extrapolation.
        m.observe(BeatRef::new(2.05, 2.0), t0 + Duration::from_secs_f64(1.0));
        // We still click beat 2 exactly once as the phasor passes it.
        let onsets = m.tick(t0 + Duration::from_secs_f64(1.2));
        assert_eq!(onsets, vec![2], "beat 2 fires once, not skipped nor doubled");
    }

    /// The Bevy wiring: a `ServerEvent::BeatSync` message anchors the phasor.
    #[test]
    fn beat_sync_message_anchors_the_phasor() {
        use kaijutsu_types::ContextId;

        let mut app = App::new();
        app.init_resource::<Metronome>()
            .add_message::<ServerEventMessage>()
            .add_systems(Update, ingest_beat_sync);

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
}
