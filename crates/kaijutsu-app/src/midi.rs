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
use crate::patch_graph::{EndpointInfo, WireInfo};

/// Bridges `ServerEvent::RenderCue` ABC cues into scheduled ALSA seq MIDI.
pub struct MidiOutPlugin;

impl Plugin for MidiOutPlugin {
    fn build(&self, app: &mut App) {
        // NonSend: the ALSA `Seq` handle lives on the main thread with the sink
        // system (a render sink is single-consumer; no cross-thread sharing).
        app.insert_non_send_resource(MidiSink::default());
        app.init_resource::<RenderAutoConnect>();
        // Open the seq port eagerly at startup so it shows up in `aconnect -l`
        // and can be wired to a synth *before* the first cue fires (the queue
        // schedules ~now for a play-now cue, so a lazily-created port would miss
        // its own first notes). Graceful on failure — no ALSA just means no MIDI.
        app.add_systems(Startup, open_midi_sink);
        app.add_systems(Update, play_midi_cues);
        // Patch-bay slice 1: one-shot, patient-retry auto-connect of the render
        // port to a detected GM synth — kills the re-`aconnect`-after-restart
        // papercut (`docs/scenes/patchbay.md`). Additive and startup-once.
        app.add_systems(Update, auto_connect_render);
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
    /// frame that scheduled it. Sound (`note`/`channel`/`velocity`) and `gate_ms`
    /// come from the per-client metronome config. Opens the sink on first use.
    /// No-op without ALSA.
    #[cfg(target_os = "linux")]
    pub(crate) fn click_at(
        &mut self,
        note: u8,
        channel: u8,
        velocity: u8,
        gate_ms: u64,
        offset: std::time::Duration,
    ) {
        if !ensure_open(self) {
            return;
        }
        if let Some(out) = self.out.as_mut() {
            out.click_at(note, channel, velocity, gate_ms, offset);
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub(crate) fn click_at(
        &mut self,
        _note: u8,
        _channel: u8,
        _velocity: u8,
        _gate_ms: u64,
        _offset: std::time::Duration,
    ) {
        ensure_open(self);
    }

    /// Patch-bay slice 1: observe the local seq graph through the render port's
    /// own handle, decide (purely) whether/where to auto-connect, and — only if
    /// there's a target — subscribe render → synth. Additive; never disconnects.
    /// Assumes the caller has already opened the sink.
    #[cfg(target_os = "linux")]
    fn try_auto_connect(&self, patterns: &[&str]) -> AutoConnectOutcome {
        let Some(out) = self.out.as_ref() else {
            return AutoConnectOutcome::Unavailable;
        };
        let render = out.addr();
        let (endpoints, wires) = out.observe();
        match decide_autoconnect(&endpoints, &wires, render, patterns) {
            Some((client, port)) => {
                let name = endpoints
                    .iter()
                    .find(|e| (e.client_id, e.port_id) == (client, port))
                    .map(|e| e.client_name.clone())
                    .unwrap_or_else(|| "synth".into());
                match out.connect_render_to(client, port) {
                    Ok(()) => AutoConnectOutcome::Connected { name, client, port },
                    Err(e) => AutoConnectOutcome::Failed(e),
                }
            }
            // No target: separate "already wired → defer" (stop) from "no synth
            // yet" (keep waiting) so the caller latches the one-shot correctly.
            None if render_has_outbound_wire(&wires, render) => AutoConnectOutcome::AlreadyWired,
            None => AutoConnectOutcome::NoSynth,
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn try_auto_connect(&self, _patterns: &[&str]) -> AutoConnectOutcome {
        AutoConnectOutcome::Unavailable
    }
}

// ── Patch-bay slice 1: render-port auto-connect ─────────────────────────────

/// Case-insensitive substring patterns for a GM synth's ALSA client name. The
/// one obvious config seam: symbolic-endpoint naming (a named-endpoint registry
/// vs a substring list) is an open question in `docs/scenes/patchbay.md` —
/// slice 1 deliberately keeps it a bare const, not a config surface.
const SYNTH_PATTERNS: [&str; 2] = ["timidity", "fluidsynth"];

/// Our own ALSA seq clients — never an auto-connect target even if a pattern
/// somehow matched one (the render's own output, the ear, the patch-view
/// reader). Matched by exact client name.
const OWN_CLIENTS: [&str; 3] = ["kaijutsu-app", "kaijutsu-ear", "kaijutsu-patchview"];

/// Retry cadence — the patch-bay's 2 s poll idiom, reused.
const AUTOCONNECT_POLL_SECS: f32 = 2.0;

/// One-shot latch for the render-port auto-connect. `done` latches for the life
/// of the process once the decision settles — connected, found already-wired, or
/// ALSA-less — and is never re-armed. That "startup-once" stance is the whole
/// point: the metronome click rides the render port and has no off-switch yet,
/// so Amy sometimes cuts this wire with `aconnect -d`; a continuously
/// reconciling ensure would make the wire uncuttable. Continuous declared-wire
/// reconciliation is slice 2's job (kernel-owned).
#[derive(Resource)]
struct RenderAutoConnect {
    done: bool,
    timer: Timer,
}

impl Default for RenderAutoConnect {
    fn default() -> Self {
        Self {
            done: false,
            timer: Timer::from_seconds(AUTOCONNECT_POLL_SECS, TimerMode::Repeating),
        }
    }
}

/// The outcome of one auto-connect attempt. `Connected` / `AlreadyWired` /
/// `Unavailable` settle the one-shot for good; `NoSynth` / `Failed` retry.
enum AutoConnectOutcome {
    Connected { name: String, client: i32, port: i32 },
    AlreadyWired,
    NoSynth,
    Failed(String),
    Unavailable,
}

/// Tick the one-shot: on a slow cadence, until it settles, try to auto-connect
/// the render port to a GM synth. Loud once on the connect, quiet while waiting.
#[cfg(target_os = "linux")]
fn auto_connect_render(
    time: Res<Time>,
    mut sink: NonSendMut<MidiSink>,
    mut latch: ResMut<RenderAutoConnect>,
) {
    if latch.done {
        return;
    }
    if !latch.timer.tick(time.delta()).just_finished() {
        return;
    }
    // No ALSA seq → `ensure_open` warns once and we stand down: no synth will
    // ever appear on a machine without a sequencer, so retrying is pure spam.
    if !ensure_open(&mut sink) {
        latch.done = true;
        return;
    }
    match sink.try_auto_connect(&SYNTH_PATTERNS) {
        AutoConnectOutcome::Connected { name, client, port } => {
            info!("patch-bay slice 1: auto-connected render → {name}:{port} (client {client})");
            latch.done = true;
        }
        AutoConnectOutcome::AlreadyWired => {
            // A hand-patch already owns the render port's routing. Stand down —
            // never fight a deliberate wiring, and never reverse a later cut.
            debug!("patch-bay slice 1: render already wired; standing down");
            latch.done = true;
        }
        AutoConnectOutcome::NoSynth => {
            debug!("patch-bay slice 1: no GM synth yet; will retry");
        }
        AutoConnectOutcome::Failed(e) => {
            debug!("patch-bay slice 1: connect failed, will retry: {e}");
        }
        AutoConnectOutcome::Unavailable => {
            latch.done = true;
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn auto_connect_render(mut latch: ResMut<RenderAutoConnect>) {
    // No ALSA off Linux — nothing to connect, ever.
    latch.done = true;
}

/// True if the render port already feeds anyone. The deferential guard: if a
/// human (or anything) has already wired render outbound, slice 1 does nothing.
fn render_has_outbound_wire(wires: &[WireInfo], render: (i32, i32)) -> bool {
    wires.iter().any(|w| w.src == render)
}

/// Pure decision core: the synth port the render port should auto-connect to,
/// or `None`. `None` means either render is already wired outbound (defer) or no
/// non-self synth matches. Additive only — this never considers a disconnect.
///
/// The target is the first writable-subscribable ("sink") port of the first
/// matching synth; with `endpoints` sorted by `(client_id, port_id)` (as
/// `MidiOut::observe` delivers them) that is the lowest-numbered synth's port 0.
/// Own clients are excluded by name regardless of pattern; a pattern matches as
/// a case-insensitive substring of the client name.
fn decide_autoconnect(
    endpoints: &[EndpointInfo],
    wires: &[WireInfo],
    render: (i32, i32),
    patterns: &[&str],
) -> Option<(i32, i32)> {
    if render_has_outbound_wire(wires, render) {
        return None;
    }
    endpoints.iter().find_map(|e| {
        if !e.is_sink || OWN_CLIENTS.contains(&e.client_name.as_str()) {
            return None;
        }
        let name = e.client_name.to_lowercase();
        let matched = patterns.iter().any(|p| name.contains(&p.to_lowercase()));
        matched.then_some((e.client_id, e.port_id))
    })
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

    /// Snapshot the local seq graph through the render port's own handle: every
    /// client/port as an `EndpointInfo` (sorted by `(client, port)`), plus the
    /// render port's *own* outbound subscriptions as `WireInfo`s — all the
    /// decision core needs. Read-only; reuses `patch_graph`'s flattened types
    /// and enumeration idiom (no second ALSA client just to observe).
    fn observe(&self) -> (Vec<EndpointInfo>, Vec<WireInfo>) {
        use alsa::seq::{self, Addr, PortCap, QuerySubsType};

        let mut endpoints = Vec::new();
        for client in seq::ClientIter::new(&self.seq) {
            let client_id = client.get_client();
            let client_name = client.get_name().unwrap_or("?").to_string();
            for port in seq::PortIter::new(&self.seq, client_id) {
                let addr = port.addr();
                let caps = port.get_capability();
                endpoints.push(EndpointInfo {
                    client_id,
                    port_id: addr.port,
                    client_name: client_name.clone(),
                    port_name: port.get_name().unwrap_or("?").to_string(),
                    is_source: caps.contains(PortCap::READ | PortCap::SUBS_READ),
                    is_sink: caps.contains(PortCap::WRITE | PortCap::SUBS_WRITE),
                });
            }
        }
        endpoints.sort_by_key(|e| (e.client_id, e.port_id));

        let (rc, rp) = self.addr();
        let mut wires = Vec::new();
        let render_addr = Addr { client: rc, port: rp };
        for sub in seq::PortSubscribeIter::new(&self.seq, render_addr, QuerySubsType::READ) {
            let src = sub.get_sender();
            let dst = sub.get_dest();
            wires.push(WireInfo {
                src: (src.client, src.port),
                dst: (dst.client, dst.port),
            });
        }
        (endpoints, wires)
    }

    /// Subscribe the render port → `(dst_client, dst_port)` — the one write this
    /// module makes. Additive: it only ever *creates* a subscription, never
    /// removes one. Errors bubble so the caller can retry on the next tick.
    fn connect_render_to(&self, dst_client: i32, dst_port: i32) -> Result<(), String> {
        use alsa::seq::{Addr, PortSubscribe};

        let map = |e: alsa::Error| format!("{e}");
        let subs = PortSubscribe::empty().map_err(map)?;
        subs.set_sender(Addr { client: self.seq.client_id().map_err(map)?, port: self.port });
        subs.set_dest(Addr { client: dst_client, port: dst_port });
        self.seq.subscribe_port(&subs).map_err(map)?;
        Ok(())
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
    /// (NoteOn then NoteOff `gate_ms` later) on `channel`, queued into the ALSA
    /// real-time queue so it fires at the precise predicted beat time. Gated and
    /// (by default) on a *normal* (non-drum) channel so it sounds under any patch
    /// — GM channel-9 percussion is silent under game soundfonts (the FF4 one on
    /// zorak has no drum kit), and a bare NoteOn on a sustaining patch would
    /// drone. Sound + gate come from the per-client metronome config
    /// (`docs/config-crdt-ownership.md`). Reuses the proven render-queue path, so
    /// it's audible exactly where the music is.
    fn click_at(&mut self, note: u8, channel: u8, velocity: u8, gate_ms: u64, offset: Duration) {
        // Mask to the low nibble so a config typo can never corrupt the status
        // byte (0x9n / 0x8n); default channel 15 keeps the click off the music's
        // channel 0.
        let ch = channel & 0x0F;
        let on = vec![0x90 | ch, note, velocity];
        let off = vec![0x80 | ch, note, 0];
        self.schedule(
            &[(offset, on), (offset + Duration::from_millis(gate_ms), off)],
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

    // -- patch-bay slice 1: auto-connect decision core --------------------

    /// The app's render port address for these tests (a plausible app client id).
    const RENDER: (i32, i32) = (129, 0);

    fn sink(client_id: i32, port_id: i32, client_name: &str) -> EndpointInfo {
        EndpointInfo {
            client_id,
            port_id,
            client_name: client_name.into(),
            port_name: "port".into(),
            is_source: false,
            is_sink: true,
        }
    }

    fn source(client_id: i32, port_id: i32, client_name: &str) -> EndpointInfo {
        EndpointInfo {
            is_source: true,
            is_sink: false,
            ..sink(client_id, port_id, client_name)
        }
    }

    fn wire(src: (i32, i32), dst: (i32, i32)) -> WireInfo {
        WireInfo { src, dst }
    }

    #[test]
    fn picks_the_first_synth_sink_port_when_render_is_unwired() {
        let endpoints = vec![sink(128, 0, "TiMidity"), sink(128, 1, "TiMidity")];
        assert_eq!(
            decide_autoconnect(&endpoints, &[], RENDER, &["timidity"]),
            Some((128, 0)),
            "TiMidity's port 0 is the target"
        );
    }

    #[test]
    fn picks_the_lowest_numbered_of_several_matching_synths() {
        // Sorted as `observe` delivers them: the lowest client wins.
        let endpoints = vec![sink(128, 0, "TiMidity"), sink(140, 0, "FluidSynth")];
        assert_eq!(
            decide_autoconnect(&endpoints, &[], RENDER, &SYNTH_PATTERNS),
            Some((128, 0))
        );
    }

    #[test]
    fn skips_when_the_render_port_already_has_an_outbound_wire() {
        let endpoints = vec![sink(128, 0, "TiMidity")];
        let wires = vec![wire(RENDER, (128, 0))];
        assert_eq!(
            decide_autoconnect(&endpoints, &wires, RENDER, &["timidity"]),
            None,
            "a hand-patch on render → defer, do nothing"
        );
    }

    #[test]
    fn an_unrelated_wire_does_not_block_the_autoconnect() {
        // A wire that doesn't originate at the render port is none of our business.
        let endpoints = vec![sink(128, 0, "TiMidity")];
        let wires = vec![wire((14, 0), (128, 0))];
        assert_eq!(
            decide_autoconnect(&endpoints, &wires, RENDER, &["timidity"]),
            Some((128, 0))
        );
    }

    #[test]
    fn never_returns_an_own_client_port_even_when_a_pattern_matches_it() {
        // The ear is a sink; a pattern that matches our own name must not win.
        let endpoints = vec![sink(200, 0, "kaijutsu-ear")];
        assert_eq!(
            decide_autoconnect(&endpoints, &[], RENDER, &["kaijutsu"]),
            None
        );
    }

    #[test]
    fn matches_the_synth_name_case_insensitively() {
        let endpoints = vec![sink(140, 0, "FLUIDSynth")];
        assert_eq!(
            decide_autoconnect(&endpoints, &[], RENDER, &["fluidsynth"]),
            Some((140, 0))
        );
    }

    #[test]
    fn ignores_a_synths_source_only_port_and_wires_to_none() {
        // A read-only source port is not something to feed MIDI into.
        let endpoints = vec![source(128, 0, "TiMidity")];
        assert_eq!(
            decide_autoconnect(&endpoints, &[], RENDER, &["timidity"]),
            None
        );
    }

    #[test]
    fn returns_none_on_an_empty_graph() {
        assert_eq!(decide_autoconnect(&[], &[], RENDER, &SYNTH_PATTERNS), None);
    }

    #[test]
    fn returns_none_when_no_client_matches_a_pattern() {
        let endpoints = vec![sink(64, 0, "Midi Through"), sink(80, 0, "Some DAW")];
        assert_eq!(
            decide_autoconnect(&endpoints, &[], RENDER, &SYNTH_PATTERNS),
            None
        );
    }

    #[test]
    fn render_has_outbound_wire_is_true_only_for_a_render_sender() {
        assert!(render_has_outbound_wire(&[wire(RENDER, (128, 0))], RENDER));
        assert!(!render_has_outbound_wire(&[wire((14, 0), (128, 0))], RENDER));
        assert!(!render_has_outbound_wire(&[], RENDER));
    }

    /// Live ALSA (needs `/dev/snd/seq`; `#[ignore]`d like `alsa_smoke` in
    /// `patch_graph.rs`). Opens the render sink and asserts its own port shows up
    /// in the observed graph — exercises `observe` without wiring anything.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs a live ALSA sequencer (/dev/snd/seq); run on the zorak runner"]
    fn observe_sees_the_render_port_on_a_live_sequencer() {
        let out = MidiOut::open().expect("open ALSA sink");
        let render = out.addr();
        let (endpoints, _wires) = out.observe();
        assert!(
            endpoints
                .iter()
                .any(|e| (e.client_id, e.port_id) == render && e.client_name == "kaijutsu-app"),
            "the render port should appear in its own observed graph: {endpoints:#?}"
        );
    }
}
