//! Render targets — consumers of a track's committed score (docs/tracks.md,
//! Stage 3 "the render-target seam").
//!
//! A **render** is a *consumer* of the track's score, not a producer: it never
//! schedules cells, never takes a turn, never appears in failure-routing — so it
//! is NOT an `AttachedContext`. It hangs on the `TrackState` as a small registry
//! and is fed from the materialize crossing: when `materialize_track` advances the
//! cursor past a newly-committed `Concrete` ABC cell, it hands that cell's
//! resolved ABC + the cell's *local instant* to every render target.
//!
//! The cell's instant is computed off a **jitter-free** reference — the beat's
//! *scheduled* fire `Instant` (`TrackState.last_fire_scheduled`), NOT the
//! `SystemTime::now()` latched after the heap pop (the jittery actual wakeup) —
//! so per-beat scheduler jitter never accumulates into the output (Stage 3 review,
//! deepseek). Cells commit *ahead* of the playhead (the speculation lead **is**
//! the jitter buffer, midi.md), so the instant is in the near future: a render
//! target schedules into its device queue ahead of time, never just-in-time.

#[cfg(target_os = "linux")]
use std::ffi::CString;
#[cfg(target_os = "linux")]
use std::time::Duration;

use tokio::time::Instant;

/// A consumer of a track's committed score. M1's one impl is `AlsaMidiOut`
/// (WI 6), which renders the ABC to MIDI events and schedules them into an ALSA
/// seq queue. The trait is `Send` so a target can move onto the beat-scheduler
/// thread; a future cross-node target (RTP-MIDI, midi.md M4) is just another impl
/// — the trait's *home* (on the track) doesn't constrain its *transport*.
pub trait RenderTarget: Send {
    /// Schedule one committed cell's rendered output at `at`. Takes the
    /// **pre-resolved** ABC `&str` (the materialize crossing already ran the
    /// deriver / read CAS once for all targets), so a render target never
    /// re-resolves a `ContentRef` from CAS (Stage 3 lock: `emit(abc:&str)`, not
    /// `emit(&Cell)`). `at` is a near-future local `Instant` on the speculation
    /// lead.
    fn emit(&mut self, abc: &str, at: Instant);

    /// Transport halt (`stop`/`pause`): the lead means the device queue holds ~a
    /// phrase of future events. TRUNCATE this target's already-scheduled events
    /// after `at` and silence any sounding notes, so a stop doesn't blindly play
    /// the buffered phrase (Stage 3 review SEV-1). Default no-op: a target with no
    /// device-side queue (e.g. a test recorder) needs nothing.
    fn flush_scheduled_after(&mut self, _at: Instant) {}
}

// ============================================================================
// AlsaMidiOut — the one real render target this stage (midi.md M1, WI 6)
// ============================================================================

/// An ALSA-sequencer MIDI-out render target: renders a committed cell's ABC to
/// MIDI events and schedules them into an ALSA `snd_seq` queue at the cell's local
/// instant (`docs/tracks.md` Stage 3 / `docs/midi.md` M1). Output-first: a track
/// declares "render my score to ALSA MIDI out"; other clients (a synth, a DAW,
/// `aseqdump`) subscribe to this client's read port.
///
/// **Scheduling model — relative real-time.** `emit` is called *ahead* of the
/// playhead (the speculation lead **is** the jitter buffer), so each event is
/// scheduled relative to *now* by `(at − now) + within-phrase offset`. Relative
/// scheduling means we never have to sync the kernel's monotonic clock to the
/// ALSA queue's clock. The within-phrase offset comes from the ABC's own `Q:`
/// tempo + the fixed PPQ (`MidiParams.ticks_per_beat`).
#[cfg(target_os = "linux")]
pub struct AlsaMidiOut {
    seq: alsa::Seq,
    /// Our read/subscribe-read port id — the source other clients connect to.
    port: i32,
    /// The scheduling queue (real-time). Started in [`AlsaMidiOut::new`].
    queue: i32,
    /// Controls PPQ (`ticks_per_beat`), channel, velocity for the ABC→events render.
    params: kaijutsu_abc::MidiParams,
}

#[cfg(target_os = "linux")]
impl AlsaMidiOut {
    /// Open an ALSA seq client + a virtual read port + a started real-time queue.
    /// `client_name`/`port_name` are what shows up in `aconnect -l`. Errors (no
    /// ALSA, no `/dev/snd/seq`) are returned, not panicked — the caller decides.
    pub fn new(client_name: &str, port_name: &str) -> Result<Self, String> {
        use alsa::seq::{EventType, PortCap, PortType};

        let map = |e: alsa::Error| format!("AlsaMidiOut: {e}");
        let seq = alsa::Seq::open(None, None, false).map_err(map)?;
        let cname = CString::new(client_name).map_err(|e| format!("AlsaMidiOut: {e}"))?;
        seq.set_client_name(&cname).map_err(map)?;
        let pname = CString::new(port_name).map_err(|e| format!("AlsaMidiOut: {e}"))?;
        // A source port: readable + subscribe-readable, so synths/DAWs connect to it.
        let port = seq
            .create_simple_port(
                &pname,
                PortCap::READ | PortCap::SUBS_READ,
                PortType::MIDI_GENERIC | PortType::APPLICATION,
            )
            .map_err(map)?;
        let qname = CString::new("kaijutsu-render").map_err(|e| format!("AlsaMidiOut: {e}"))?;
        let queue = seq.alloc_named_queue(&qname).map_err(map)?;
        // Start the queue so scheduled (real-time) events actually fire.
        seq.control_queue(queue, EventType::Start, 0, None).map_err(map)?;
        seq.drain_output().map_err(map)?;
        Ok(Self { seq, port, queue, params: kaijutsu_abc::MidiParams::default() })
    }

    /// The `(client, port)` this target publishes on — for tests / introspection.
    pub fn addr(&self) -> (i32, i32) {
        (self.seq.client_id().unwrap_or(-1), self.port)
    }
}

#[cfg(target_os = "linux")]
impl RenderTarget for AlsaMidiOut {
    fn emit(&mut self, abc: &str, at: Instant) {
        // Resolve ABC → the timed MIDI event stream (Stage 3 WI 5). A parse with no
        // tune is a producer bug upstream; log and skip rather than panic the beat.
        let parsed = kaijutsu_abc::parse(abc);
        let Some(tune) = parsed.value.first() else {
            log::warn!("AlsaMidiOut: emit got ABC that parsed to no tune; skipping");
            return;
        };
        // Within-phrase tick → wall-time: the ABC's own tempo + the render PPQ.
        let bpm = tune.header.tempo.as_ref().map(|t| t.bpm).unwrap_or(120).max(1);
        let secs_per_tick = 60.0 / bpm as f64 / self.params.ticks_per_beat.max(1) as f64;
        let events = kaijutsu_abc::midi::events(tune, &self.params);

        // The cell's instant is in the near future (the lead); schedule relative to
        // now so we never depend on the ALSA queue's absolute clock. Clamp at 0 —
        // never schedule into the past (the next_fire MONOTONIC contract).
        let base_rel = at.saturating_duration_since(Instant::now());

        let mut encoder = match alsa::seq::MidiEvent::new(16) {
            Ok(e) => e,
            Err(e) => {
                log::error!("AlsaMidiOut: failed to build MIDI encoder: {e}");
                return;
            }
        };
        // Each MidiEvent carries its own status byte — disable running-status elision.
        encoder.enable_running_status(false);

        for ev in &events {
            // Channel-voice messages only (status < 0xF0). Meta (0xFF tempo/EOT) and
            // System messages are not scheduled to the seq queue.
            match ev.data.first() {
                Some(&status) if status < 0xF0 => {}
                _ => continue,
            }
            encoder.init();
            let seq_ev = match encoder.encode(&ev.data) {
                Ok((_, Some(mut e))) => {
                    let when = base_rel + Duration::from_secs_f64(ev.tick as f64 * secs_per_tick);
                    e.set_source(self.port);
                    e.set_subs(); // broadcast to whoever subscribed to our port
                    e.schedule_real(self.queue, true, when);
                    e
                }
                Ok((_, None)) => continue, // incomplete message — shouldn't happen
                Err(e) => {
                    log::error!("AlsaMidiOut: encode failed: {e}");
                    continue;
                }
            };
            let mut seq_ev = seq_ev;
            if let Err(e) = self.seq.event_output(&mut seq_ev) {
                log::error!("AlsaMidiOut: event_output failed: {e}");
            }
        }
        if let Err(e) = self.seq.drain_output() {
            log::error!("AlsaMidiOut: drain_output failed: {e}");
        }
    }

    fn flush_scheduled_after(&mut self, _at: Instant) {
        use alsa::seq::{EvCtrl, Event, EventType, Remove, RemoveEvents};

        // Drop every pending/scheduled output event on our queue — the buffered
        // phrase the lead put there. (A precise "after `at`" truncation would add
        // TIME_AFTER + a queue-relative time; for a stop/pause we want the whole
        // tail gone, so removing all pending OUTPUT on the queue is the right,
        // conservative behaviour.)
        if let Ok(rm) = RemoveEvents::new() {
            rm.set_queue(self.queue);
            rm.set_condition(Remove::OUTPUT);
            if let Err(e) = self.seq.remove_events(rm) {
                log::error!("AlsaMidiOut: remove_events failed: {e}");
            }
        }
        // Silence anything already sounding: ALL_SOUNDS_OFF (CC 120) + ALL_NOTES_OFF
        // (CC 123) on every channel, sent DIRECT (bypassing the queue) so they take
        // effect immediately rather than queueing behind the truncation.
        for channel in 0..16u8 {
            for param in [120u32, 123u32] {
                let ctrl = EvCtrl { channel, param, value: 0 };
                let mut e = Event::new(EventType::Controller, &ctrl);
                e.set_source(self.port);
                e.set_subs();
                e.set_direct();
                let _ = self.seq.event_output(&mut e);
            }
        }
        let _ = self.seq.drain_output();
    }
}

#[cfg(all(test, target_os = "linux"))]
mod alsa_tests {
    use super::*;
    use std::time::Duration as StdDuration;

    /// Live ALSA loopback (needs `/dev/snd/seq`; `#[ignore]` so CI without ALSA
    /// stays green — run with `--ignored` on a box with the sequencer, e.g. zorak).
    /// Opens an `AlsaMidiOut`, connects a reader port to it, emits a 4-note phrase
    /// at `now`, and asserts the NoteOns arrive in pitch order; then a
    /// `flush_scheduled_after` truncates and emits the all-notes-off controllers.
    #[tokio::test]
    #[ignore = "needs a live ALSA sequencer (/dev/snd/seq); run on the zorak runner"]
    async fn alsa_loopback_plays_notes_and_flush_silences() {
        use alsa::seq::{Addr, EventType, PortCap, PortSubscribe, PortType};
        use std::ffi::CString;

        let mut out = AlsaMidiOut::new("kj-test-out", "render").expect("open ALSA out");
        let (out_client, out_port) = out.addr();

        // A reader client with a writable port, subscribed to the out port.
        let reader = alsa::Seq::open(None, None, true).expect("open reader");
        reader.set_client_name(&CString::new("kj-test-reader").unwrap()).unwrap();
        let in_port = reader
            .create_simple_port(
                &CString::new("in").unwrap(),
                PortCap::WRITE | PortCap::SUBS_WRITE,
                PortType::MIDI_GENERIC | PortType::APPLICATION,
            )
            .unwrap();
        let subs = PortSubscribe::empty().unwrap();
        subs.set_sender(Addr { client: out_client, port: out_port });
        subs.set_dest(Addr { client: reader.client_id().unwrap(), port: in_port });
        reader.subscribe_port(&subs).expect("subscribe reader to out");

        // Emit a 4-note ascending phrase, scheduled at ~now (small lead) so it plays
        // out quickly. CDEF at L:1/16 / a brisk tempo keeps the test well under 1 s.
        let abc = "X:1\nT:t\nM:4/4\nL:1/16\nQ:1/4=240\nK:C\nCDEF|\n";
        let at = Instant::now() + StdDuration::from_millis(20);
        out.emit(abc, at);

        // Collect NoteOn pitches as the queue plays them out (poll up to ~1 s).
        let mut note_ons: Vec<u8> = Vec::new();
        let mut input = reader.input();
        let deadline = std::time::Instant::now() + StdDuration::from_secs(1);
        while std::time::Instant::now() < deadline && note_ons.len() < 4 {
            if input.event_input_pending(true).unwrap_or(0) > 0 {
                if let Ok(ev) = input.event_input() {
                    if ev.get_type() == EventType::Noteon {
                        if let Some(n) = ev.get_data::<alsa::seq::EvNote>() {
                            // NoteOn with velocity 0 is a NoteOff; keep real ones.
                            if n.velocity > 0 {
                                note_ons.push(n.note);
                            }
                        }
                    }
                }
            } else {
                std::thread::sleep(StdDuration::from_millis(5));
            }
        }
        assert_eq!(note_ons.len(), 4, "all four NoteOns played through the loopback");
        assert!(
            note_ons.windows(2).all(|w| w[0] < w[1]),
            "NoteOns arrive in ascending pitch order (CDEF): {note_ons:?}"
        );

        // Flush must not error on a live device (truncate + all-notes-off).
        out.flush_scheduled_after(Instant::now());
    }
}
