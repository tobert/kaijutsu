//! MIDI render sink — the app is the first MIDI sink (`docs/midi.md` "Render is
//! a wire cue"; `docs/pcm.md` slice 5c).
//!
//! A `RenderCue { mime: "text/vnd.abc", payload: Inline(abc), lead }` carries a
//! committed ABC score *symbolically*. The app renders it to MIDI with the same
//! `kaijutsu_abc::midi::events` path the now-demolished server-side
//! `AlsaMidiOut` used to use, and schedules the events into a local ALSA seq queue at
//! `receipt + lead`. Rendering at the sink is fine here because the app already
//! depends on `kaijutsu-abc` (its ABC→staff renderer), so the "keep the sink
//! dumb, no ABC crate" reason for kernel-side rendering doesn't apply — the mime
//! says which: a truly-dumb edge node later takes a pre-rendered `audio/midi`
//! cue instead. `lead` absorbs wire latency exactly as the speculation lead does
//! for scheduling (`docs/midi.md`): the app schedules ahead, never just-in-time.

use std::time::Duration;

use bevy::prelude::*;
use kaijutsu_audio::{CuePayload, ABC_MIME, RENDER_FLUSH_MIME};
use kaijutsu_client::ServerEvent;

use crate::connection::actor_plugin::ServerEventMessage;

/// Bridges `ServerEvent::RenderCue` ABC cues into scheduled ALSA seq MIDI.
pub struct MidiOutPlugin;

impl Plugin for MidiOutPlugin {
    fn build(&self, app: &mut App) {
        // NonSend: the ALSA `Seq` handle lives on the main thread with the sink
        // system (a render sink is single-consumer; no cross-thread sharing).
        app.insert_non_send_resource(MidiSink::default());
        // Open the seq port eagerly at startup so it shows up in `aconnect -l`
        // and can be wired to a synth *before* the first cue fires (the queue
        // schedules ~now for a play-now cue, so a lazily-created port would miss
        // its own first notes). Graceful on failure — no ALSA just means no MIDI.
        app.add_systems(Startup, open_midi_sink);
        app.add_systems(Update, play_midi_cues);
    }
}

/// Startup: try to open the ALSA seq sink so the port is connectable up front.
fn open_midi_sink(mut sink: NonSendMut<MidiSink>) {
    ensure_open(&mut sink);
}

/// Lazily-opened ALSA seq sink. Opened on the first MIDI cue; `failed` latches
/// once an open attempt fails (no `/dev/snd/seq`) so we warn once, not per-cue.
/// `pub(crate)` so the metronome (`crate::metronome`) can click through the SAME
/// seq port the render cues use — one app, one port, so `aconnect`-ing to a synth
/// wires up both the music and the 拍子木 click.
#[derive(Default)]
pub(crate) struct MidiSink {
    #[cfg(target_os = "linux")]
    out: Option<MidiOut>,
    failed: bool,
}

impl MidiSink {
    /// Schedule one metronome click `offset` from now into the sink queue — so
    /// ALSA fires it at the phasor's predicted beat time, not at the irregular
    /// frame that scheduled it. Opens the sink on first use. No-op without ALSA.
    #[cfg(target_os = "linux")]
    pub(crate) fn click_at(&mut self, note: u8, offset: std::time::Duration) {
        if !ensure_open(self) {
            return;
        }
        if let Some(out) = self.out.as_mut() {
            out.click_at(note, offset);
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub(crate) fn click_at(&mut self, _note: u8, _offset: std::time::Duration) {
        ensure_open(self);
    }
}

/// Render ABC → a flat list of `(offset-from-phrase-start, raw channel-voice
/// MIDI bytes)`, ready to schedule relative to a start instant. Reuses the exact
/// `abc→events` path `AlsaMidiOut` used; the only sink step is tick→wall via the
/// tune's own `Q:` tempo + the render PPQ. Meta/system events (status ≥ 0xF0)
/// are dropped — they never went to the seq queue. Empty if the ABC parses to no
/// tune (a producer bug upstream — logged loudly at the call site, not here).
fn abc_to_timed_events(abc: &str) -> Vec<(Duration, Vec<u8>)> {
    let parsed = kaijutsu_abc::parse(abc);
    let Some(tune) = parsed.value.first() else {
        return Vec::new();
    };
    let params = kaijutsu_abc::MidiParams::default();
    let bpm = tune.header.tempo.as_ref().map(|t| t.bpm).unwrap_or(120).max(1);
    let secs_per_tick = 60.0 / bpm as f64 / params.ticks_per_beat.max(1) as f64;
    kaijutsu_abc::midi::events(tune, &params)
        .into_iter()
        .filter(|ev| matches!(ev.data.first(), Some(&status) if status < 0xF0))
        .map(|ev| {
            (
                Duration::from_secs_f64(ev.tick as f64 * secs_per_tick),
                ev.data,
            )
        })
        .collect()
}

/// Consume `RenderCue` ABC cues and schedule their MIDI into the ALSA seq queue.
/// Reads the same message stream as the audio sink (`audio.rs`) — each system
/// keeps its own cursor — and filters by mime, so the two never contend.
fn play_midi_cues(
    mut messages: MessageReader<ServerEventMessage>,
    mut sink: NonSendMut<MidiSink>,
) {
    for ServerEventMessage(event) in messages.read() {
        let ServerEvent::RenderCue { cue, .. } = event else {
            continue;
        };
        // A transport flush (stop/pause): drop the buffered phrase + silence.
        if cue.mime == RENDER_FLUSH_MIME {
            flush(&mut sink);
            continue;
        }
        if cue.mime != ABC_MIME {
            continue;
        }
        let CuePayload::Inline(bytes) = &cue.payload else {
            // CAS-backed ABC (a large score by ref) is slice-5c prefetch too.
            warn!("MIDI cue with a CAS payload not resolved yet (mime={})", cue.mime);
            continue;
        };
        let Ok(abc) = std::str::from_utf8(bytes) else {
            warn!("MIDI cue ABC payload was not UTF-8; skipping");
            continue;
        };
        let events = abc_to_timed_events(abc);
        if events.is_empty() {
            warn!("MIDI cue ABC rendered to no events; skipping");
            continue;
        }
        schedule(&mut sink, events, cue.lead);
    }
}

/// Open the sink if it isn't already; `false` if it's unavailable (open failed
/// once — latched, so we warn once, not per-cue).
#[cfg(target_os = "linux")]
fn ensure_open(sink: &mut MidiSink) -> bool {
    if sink.out.is_some() {
        return true;
    }
    if sink.failed {
        return false;
    }
    match MidiOut::open() {
        Ok(out) => {
            info!("kaijutsu-app MIDI sink open on ALSA seq {:?}", out.addr());
            sink.out = Some(out);
            true
        }
        Err(e) => {
            warn!("MIDI sink unavailable (no ALSA seq?): {e}");
            sink.failed = true;
            false
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn ensure_open(sink: &mut MidiSink) -> bool {
    if !sink.failed {
        warn!("MIDI render sink is Linux/ALSA-only; ignoring MIDI cues on this platform");
        sink.failed = true;
    }
    false
}

#[cfg(target_os = "linux")]
fn schedule(sink: &mut MidiSink, events: Vec<(Duration, Vec<u8>)>, lead: Duration) {
    if !ensure_open(sink) {
        return;
    }
    if let Some(out) = sink.out.as_mut() {
        out.schedule(&events, lead);
    }
}

#[cfg(not(target_os = "linux"))]
fn schedule(sink: &mut MidiSink, _events: Vec<(Duration, Vec<u8>)>, _lead: Duration) {
    ensure_open(sink);
}

/// Transport flush (stop/pause): drop the buffered phrase + silence sounding
/// notes. No-op if the sink was never opened (nothing scheduled).
#[cfg(target_os = "linux")]
fn flush(sink: &mut MidiSink) {
    if let Some(out) = sink.out.as_mut() {
        out.flush();
    }
}

#[cfg(not(target_os = "linux"))]
fn flush(_sink: &mut MidiSink) {}

/// An ALSA-sequencer MIDI-out sink: a subscribe-readable source port + a started
/// real-time queue. Other clients (`aconnect` → TiMidity, a DAW, `aseqdump`)
/// connect to `addr()`. Ported from the server's `AlsaMidiOut` but consuming
/// pre-timed events rather than ABC (the render already ran at the sink).
#[cfg(target_os = "linux")]
struct MidiOut {
    seq: alsa::Seq,
    port: i32,
    queue: i32,
}

#[cfg(target_os = "linux")]
impl MidiOut {
    fn open() -> Result<Self, String> {
        use alsa::seq::{EventType, PortCap, PortType};
        use std::ffi::CString;

        let map = |e: alsa::Error| format!("{e}");
        let seq = alsa::Seq::open(None, None, false).map_err(map)?;
        seq.set_client_name(&CString::new("kaijutsu-app").map_err(|e| e.to_string())?)
            .map_err(map)?;
        let port = seq
            .create_simple_port(
                &CString::new("render").map_err(|e| e.to_string())?,
                PortCap::READ | PortCap::SUBS_READ,
                PortType::MIDI_GENERIC | PortType::APPLICATION,
            )
            .map_err(map)?;
        let queue = seq
            .alloc_named_queue(&CString::new("kaijutsu-app-render").map_err(|e| e.to_string())?)
            .map_err(map)?;
        seq.control_queue(queue, EventType::Start, 0, None).map_err(map)?;
        seq.drain_output().map_err(map)?;
        Ok(Self { seq, port, queue })
    }

    fn addr(&self) -> (i32, i32) {
        (self.seq.client_id().unwrap_or(-1), self.port)
    }

    /// Schedule each event at `lead + offset` relative to now (real-time queue),
    /// so we never sync the app clock to the ALSA queue's clock.
    fn schedule(&mut self, events: &[(Duration, Vec<u8>)], lead: Duration) {
        let mut encoder = match alsa::seq::MidiEvent::new(16) {
            Ok(e) => e,
            Err(e) => {
                error!("MIDI encoder init failed: {e}");
                return;
            }
        };
        // Each event carries its own status byte — disable running-status elision.
        encoder.enable_running_status(false);

        for (offset, data) in events {
            encoder.init();
            match encoder.encode(data) {
                Ok((_, Some(mut ev))) => {
                    let when = lead + *offset;
                    ev.set_source(self.port);
                    ev.set_subs();
                    ev.schedule_real(self.queue, true, when);
                    if let Err(e) = self.seq.event_output(&mut ev) {
                        error!("MIDI event_output failed: {e}");
                    }
                }
                Ok((_, None)) => continue, // incomplete message — shouldn't happen
                Err(e) => {
                    error!("MIDI encode failed: {e}");
                    continue;
                }
            }
        }
        if let Err(e) = self.seq.drain_output() {
            error!("MIDI drain_output failed: {e}");
        }
    }

    /// Drop every scheduled-but-unplayed event on our queue and silence sounding
    /// notes (ALL_SOUNDS_OFF + ALL_NOTES_OFF on every channel, sent DIRECT so
    /// they bypass the queue). Ported from the server's `flush_scheduled_after`.
    fn flush(&mut self) {
        use alsa::seq::{EvCtrl, Event, EventType, Remove, RemoveEvents};

        if let Ok(rm) = RemoveEvents::new() {
            rm.set_queue(self.queue);
            rm.set_condition(Remove::OUTPUT);
            if let Err(e) = self.seq.remove_events(rm) {
                error!("MIDI remove_events failed: {e}");
            }
        }
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

    /// Schedule a metronome click at `offset` from now: a short **gated** note
    /// (~60 ms, NoteOn then NoteOff) on a dedicated channel, queued into the ALSA
    /// real-time queue so it fires at the precise predicted beat time. Gated and
    /// on a *normal* (non-drum) channel so it sounds under any patch — GM
    /// channel-9 percussion is silent under game soundfonts (the FF4 one on
    /// zorak has no drum kit), and a bare NoteOn on a sustaining patch would
    /// drone. Reuses the proven render-queue path, so it's audible exactly where
    /// the music is.
    fn click_at(&mut self, note: u8, offset: Duration) {
        // Channel 15: off the music's channel 0, so the click keeps its own
        // patch and never collides with a musician's render on the same port.
        const CH: u8 = 15;
        let on = vec![0x90 | CH, note, 110];
        let off = vec![0x80 | CH, note, 0];
        self.schedule(
            &[(offset, on), (offset + Duration::from_millis(60), off)],
            Duration::ZERO,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A brisk four-note ascending phrase. `L:1/4`, one note per beat.
    const CDEF: &str = "X:1\nT:t\nM:4/4\nL:1/4\nQ:1/4=120\nK:C\nCDEF|\n";

    /// The pure render (ABC → timed events) is unit-testable with no ALSA: the
    /// CDEF phrase yields four NoteOns in ascending pitch at increasing offsets,
    /// the first at the phrase start.
    #[test]
    fn abc_renders_to_ascending_noteons_at_increasing_offsets() {
        let events = abc_to_timed_events(CDEF);
        assert!(!events.is_empty(), "CDEF should render to MIDI events");

        // NoteOn = status 0x90..0x9F with velocity (data[2]) > 0.
        let note_ons: Vec<(Duration, u8)> = events
            .iter()
            .filter(|(_, d)| d.len() == 3 && d[0] & 0xF0 == 0x90 && d[2] > 0)
            .map(|(off, d)| (*off, d[1]))
            .collect();

        assert_eq!(note_ons.len(), 4, "four notes: {note_ons:?}");
        assert_eq!(note_ons[0].0, Duration::ZERO, "first note starts at phrase start");
        assert!(
            note_ons.windows(2).all(|w| w[0].1 < w[1].1),
            "pitches ascend C<D<E<F: {note_ons:?}"
        );
        assert!(
            note_ons.windows(2).all(|w| w[0].0 < w[1].0),
            "note starts advance in time: {note_ons:?}"
        );
    }

    #[test]
    fn noteless_abc_renders_to_no_channel_voice_events() {
        // Empty input → no tune → no events.
        assert!(abc_to_timed_events("").is_empty());
        // A header with no note body → no channel-voice events (any meta like a
        // tempo/program is status ≥ 0xF0 and filtered out). NB: the ABC parser
        // is deliberately lenient — bare letters a–g ARE notes — so a "garbage"
        // string that happens to contain them renders real notes; that's why
        // this uses a structured, genuinely note-free tune.
        assert!(abc_to_timed_events("X:1\nT:empty\nM:4/4\nL:1/4\nK:C\n").is_empty());
    }

    /// Live ALSA loopback (needs `/dev/snd/seq`; `#[ignore]` so CI stays green —
    /// run with `--ignored` on a box with the sequencer, e.g. zorak). Opens the
    /// sink, subscribes a reader, schedules the CDEF phrase at a small lead, and
    /// asserts the four NoteOns arrive in ascending pitch.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs a live ALSA sequencer (/dev/snd/seq); run on the zorak runner"]
    fn alsa_loopback_plays_the_phrase() {
        use alsa::seq::{Addr, EventType, PortCap, PortSubscribe, PortType};
        use std::ffi::CString;

        let mut out = MidiOut::open().expect("open ALSA sink");
        let (out_client, out_port) = out.addr();

        let reader = alsa::Seq::open(None, None, true).expect("open reader");
        reader
            .set_client_name(&CString::new("kj-app-test-reader").unwrap())
            .unwrap();
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
        reader.subscribe_port(&subs).expect("subscribe reader");

        out.schedule(&abc_to_timed_events(CDEF), Duration::from_millis(20));

        let mut note_ons: Vec<u8> = Vec::new();
        let mut input = reader.input();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline && note_ons.len() < 4 {
            if input.event_input_pending(true).unwrap_or(0) > 0 {
                if let Ok(ev) = input.event_input() {
                    if ev.get_type() == EventType::Noteon {
                        if let Some(n) = ev.get_data::<alsa::seq::EvNote>() {
                            if n.velocity > 0 {
                                note_ons.push(n.note);
                            }
                        }
                    }
                }
            } else {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        assert_eq!(note_ons.len(), 4, "four NoteOns through the loopback: {note_ons:?}");
        assert!(note_ons.windows(2).all(|w| w[0] < w[1]), "ascending: {note_ons:?}");
    }
}
