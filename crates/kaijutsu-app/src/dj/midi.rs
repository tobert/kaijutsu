//! The DJ thread's owned MIDI sink — ABC→MIDI dispatch, the ALSA render
//! port, and the patch-bay auto-connect one-shot (`docs/midi.md` "The DJ
//! thread", DJ-thread arc Task #4 — the demolition). Ported from the deleted
//! `midi.rs` (the app's Bevy-side MIDI-out plugin, `MidiOutPlugin`) with one
//! structural change: there is no Bevy here at all. [`MidiSink`] owns the
//! ALSA `Seq` handle as **loop-local state inside [`super::thread::run_loop`]**
//! rather than a `NonSend` Bevy resource — the same "the app is the sink"
//! doctrine, minus the frame coupling this whole arc exists to remove.
//!
//! **Why loop-local, not a Bevy resource:** a real ALSA `Seq` handle is
//! `!Send` (that's WHY it used to be a `NonSend` resource, pinned to Bevy's
//! main thread). It holds nothing runtime-shaped though — no nested tokio
//! runtime, no async state — so unlike [`super::prefetch::CasPrefetch`]
//! (which must be built/dropped in `thread_main`'s sync frame around
//! `rt.block_on`, never inside the async body, or its `Drop` panics)
//! [`MidiSink`] has no such constraint. It is constructed once by
//! `thread_main` — already running ON the DJ thread, since that function is
//! only ever reached via `std::thread::Builder::spawn` — and handed into
//! [`super::thread::run_loop`] as an owned parameter (mirroring how `sinks`
//! and `prefetch` already arrive), so it never crosses a thread boundary and
//! stays exactly one instance for the whole thread's life. Tests hand in
//! their own [`MidiDispatch`] implementor the same way.
//!
//! **One app, one port.** [`MidiSink`] serves BOTH ABC-cue scheduling (the
//! events arm, via [`dispatch_midi_cue`]) and metronome clicks (the
//! click-timer arm, via [`super::thread::handle_due_clicks`]) through the
//! SAME ALSA seq port — an `aconnect` to a synth wires up the music and the
//! 拍子木 click in one wire, exactly as the pre-Task-#4 `midi.rs`/`metronome.rs`
//! split already documented, now unified onto one owned sink instead of two
//! cooperating Bevy resources.
//!
//! **The [`MidiDispatch`] seam.** `run_loop` is generic over `M:
//! MidiDispatch` (mirroring its existing `H: ActorSource` genericity) so a
//! test can inject a channel-backed recording double and drive the real
//! `select!` loop end to end with no ALSA device — see `dj::thread`'s test
//! module. The trait folds together everything the loop needs from a MIDI
//! sink: clicking, ABC scheduling, flushing, traffic-flag draining, AND one
//! tick of the auto-connect one-shot — broader than the old
//! `dj::thread::ClickSink` it replaces (that seam only ever covered clicks;
//! Task #4 is the task that gives the events arm a sink to dispatch ABC
//! through too, so the seam grew to match).
//!
//! **Auto-connect moved from a Bevy `Timer` to a `tokio::time::interval`**
//! ([`AUTOCONNECT_POLL`]) — [`super::thread::run_loop`] owns the cadence and
//! the one-shot `done` latch as loop-local state; [`MidiSink::tick_autoconnect`]
//! (via [`MidiDispatch`]) does exactly one attempt per call and reports
//! whether to keep retrying, mirroring the old `auto_connect_render` Bevy
//! system's per-tick body with the timer/latch bookkeeping lifted out to the
//! caller. The settle semantics are UNCHANGED: `Connected` / `AlreadyWired` /
//! `Unavailable` latch for the life of the process; `NoSynth` / `Failed`
//! retry. **Never fight a deliberate wiring, and never reverse a later cut**
//! — a hand `aconnect -d` after auto-connect settled stays cut; the one-shot
//! never re-arms.

use std::time::Duration;

use kaijutsu_audio::{ABC_MIME, CuePayload, RENDER_FLUSH_MIME, RenderCue};
use tracing::{debug, warn};

use crate::patch_graph::{EndpointInfo, WireInfo};

// ── MidiDispatch — the seam run_loop is generic over ────────────────────────

/// Everything the DJ thread's `select!` loop dispatches through its one owned
/// MIDI sink: clicks (the click-timer arm, via
/// [`super::thread::handle_due_clicks`]), ABC-rendered cues (the events arm,
/// via [`dispatch_midi_cue`]), a transport flush, the traffic-pulse flag, and
/// one tick of the render-port auto-connect one-shot. [`MidiSink`] is the
/// real (ALSA-backed) implementation; a recording double in `dj::thread`'s
/// test module implements it too, so the whole dispatch path is exercised
/// through the real `run_loop` with no ALSA device.
pub(crate) trait MidiDispatch {
    /// Schedule one metronome click `offset` from now — see
    /// [`MidiSink::click_at`].
    fn click_at(&mut self, note: u8, channel: u8, velocity: u8, gate_ms: u64, offset: Duration);
    /// Schedule a rendered ABC phrase's events, each already offset from
    /// phrase start, `lead` ahead of now — see [`MidiSink::schedule_abc`].
    fn schedule_abc(&mut self, events: Vec<(Duration, Vec<u8>)>, lead: Duration);
    /// Drop every scheduled-but-unplayed event and silence sounding notes.
    fn flush(&mut self);
    /// Drain "did anything leave the render port since the last drain" —
    /// the [`super::thread::DjPulse::RenderTraffic`] source.
    fn take_traffic(&mut self) -> bool;
    /// One attempt at the render-port auto-connect one-shot; `true` once the
    /// decision has settled for good (latch), `false` to keep retrying on the
    /// next timer fire. See [`MidiSink::tick_autoconnect`].
    fn tick_autoconnect(&mut self) -> bool;
}

// ── ABC render + the phase-align backdating ladder (pure) ──────────────────

/// A cue older than this on receipt is rejected outright rather than
/// back-dated — the pipe is backed up badly enough that "when this was true"
/// is no longer useful (mirrors `timebase::REF_STALE_MAX`'s staleness drop for
/// `BeatSync` references; Amy's stale-data rejection applies to render cues
/// too, not just the phasor).
const CUE_STALE_MAX: Duration = kaijutsu_audio::REF_STALE_MAX;

/// Re-anchor a cue's schedule against how OLD it actually is on receipt — the
/// phase-align fix for the per-phrase ΔL jump (`docs/midi.md`): a cue
/// delivered late used to sound late wholesale (`receipt + lead` anchors at
/// the LATE receipt), so consecutive cues carried independent transfer
/// latencies and the render drifted out of phase with the kernel-true click.
///
/// `age` is `now_epoch_ns − cue.epoch_ns` at receipt; `None` means the cue was
/// unstamped (an old peer, or a directive with no meaningful emission
/// instant) — old behavior verbatim, no back-dating.
///
/// - `age ≤ lead`: the lead already had enough slack to absorb the delay —
///   `lead' = lead − age` kills the jump outright, events untouched.
/// - `age > lead`: the delay ate through the whole lead; `d = age − lead` is
///   how far into what SHOULD have already started this cue now sits. Events
///   whose own intra-cue offset is `< d` would schedule into the past — ALSA
///   can't do that, and clamping them all to `now` would smear them into one
///   late chord instead of dropping cleanly, so they're DROPPED instead.
///   NoteOffs among the dropped are orphaned harmlessly (a note-off with no
///   matching note-on is a no-op); NoteOns are never stranded without their
///   off, because `abc_to_timed_events` always orders a note's off strictly
///   after its on, so an on that survives always keeps its off too. Survivors
///   shift by `−d` and `lead'` collapses to `ZERO` (there's no lead left to
///   spend — the survivors schedule as soon as possible).
/// - `d > CUE_STALE_MAX`: the whole cue is too stale to trust even partially
///   — reject it outright (`None`) rather than dribble out a handful of
///   barely-salvaged notes.
fn backdate_events(
    events: Vec<(Duration, Vec<u8>)>,
    lead: Duration,
    age: Option<Duration>,
) -> Option<(Vec<(Duration, Vec<u8>)>, Duration)> {
    let Some(age) = age else {
        return Some((events, lead)); // unstamped: old behavior verbatim
    };
    if age <= lead {
        return Some((events, lead - age));
    }
    let deficit = age - lead;
    if deficit > CUE_STALE_MAX {
        return None; // way too stale to trust even partially
    }
    let survivors = events
        .into_iter()
        .filter(|(offset, _)| *offset >= deficit)
        .map(|(offset, data)| (offset - deficit, data))
        .collect();
    Some((survivors, Duration::ZERO))
}

/// Render ABC → a flat list of `(offset-from-phrase-start, raw channel-voice
/// MIDI bytes)`, ready to schedule relative to a start instant. Reuses the
/// exact `abc→events` path the demolished server-side `AlsaMidiOut` used; the
/// only sink step is tick→wall via the tune's own `Q:` tempo + the render
/// PPQ. Meta/system events (status ≥ 0xF0) are dropped — they never went to
/// the seq queue. Empty if the ABC parses to no tune (a producer bug
/// upstream — logged loudly at the call site, not here).
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

/// Consume one `RenderCue` off the DJ thread's events arm: flush the sink on
/// a transport stop/pause, or render+schedule an ABC phrase. Everything else
/// (audio/*, CLIP_MIME, PREPARE_MIME) is `dj::audio`'s own mime, ignored here
/// — the two dispatch functions run side by side off the SAME events-arm
/// iteration (`dj::thread::run_loop`), each reading `RENDER_FLUSH_MIME`
/// independently, mirroring the pre-Task-#4 `audio.rs`/`midi.rs` split now
/// folded onto one thread.
///
/// `now_epoch_ns` is the SAME read `dj::thread::handle_server_event` and
/// `dj::audio::dispatch_render_cue` already used for this event (read once
/// per receipt, `dj::thread`'s discipline) — so a cue's staleness math never
/// drifts from the clock reaction to the same event.
pub(crate) fn dispatch_midi_cue(cue: &RenderCue, now_epoch_ns: u64, sink: &mut dyn MidiDispatch) {
    if cue.mime == RENDER_FLUSH_MIME {
        sink.flush();
        return;
    }
    if cue.mime != ABC_MIME {
        return;
    }
    let CuePayload::Inline(bytes) = &cue.payload else {
        // CAS-backed ABC (a large score by ref) is a recorded follow-up
        // (docs/issues.md), not resolved yet — loud, not silently dropped.
        warn!("MIDI cue with a CAS payload not resolved yet (mime={})", cue.mime);
        return;
    };
    let Ok(abc) = std::str::from_utf8(bytes) else {
        warn!("MIDI cue ABC payload was not UTF-8; skipping");
        return;
    };
    let events = abc_to_timed_events(abc);
    if events.is_empty() {
        warn!("MIDI cue ABC rendered to no events; skipping");
        return;
    }
    let age = (cue.epoch_ns != 0)
        .then(|| Duration::from_nanos(now_epoch_ns.saturating_sub(cue.epoch_ns)));
    let Some((events, lead)) = backdate_events(events, cue.lead, age) else {
        kaijutsu_telemetry::record_stale_cue_dropped();
        warn!(
            "MIDI cue rejected — stale beyond {CUE_STALE_MAX:?} by the time it was \
             received; dropping the whole phrase rather than smear it"
        );
        return;
    };
    if events.is_empty() {
        warn!("MIDI cue backdated to no survivors (fully in the past); skipping");
        return;
    }
    sink.schedule_abc(events, lead);
}

// ── click byte-masking (pure) ────────────────────────────────────────────

/// Build the (NoteOn, NoteOff) byte triples for one metronome click, masking
/// every byte to its valid MIDI range first: channel to the low nibble (keeps
/// the status byte `0x9n`/`0x8n` intact) and note/velocity to 7 bits each — a
/// data byte ≥ 0x80 (e.g. velocity=200/0xC8 from a config typo) IS a MIDI
/// status byte, so unmasked it would inject a rogue Program Change mid-stream
/// rather than just mis-sounding the click. Pure and platform-independent (no
/// ALSA) so the masking is unit-testable without a live sequencer.
fn click_bytes(note: u8, channel: u8, velocity: u8) -> (Vec<u8>, Vec<u8>) {
    let ch = channel & 0x0F;
    let note = note & 0x7F;
    let velocity = velocity & 0x7F;
    (vec![0x90 | ch, note, velocity], vec![0x80 | ch, note, 0])
}

// ── patch-bay slice 1: render-port auto-connect (pure decision core) ───────

/// Case-insensitive substring patterns for a GM synth's ALSA client name. The
/// one obvious config seam: symbolic-endpoint naming (a named-endpoint registry
/// vs a substring list) is an open question in `docs/scenes/patchbay.md` —
/// slice 1 deliberately keeps it a bare const, not a config surface.
const SYNTH_PATTERNS: [&str; 2] = ["timidity", "fluidsynth"];

/// Our own ALSA seq clients — never an auto-connect target even if a pattern
/// somehow matched one (the render's own output, the ear, the patch-view
/// reader). Matched by exact client name.
const OWN_CLIENTS: [&str; 3] = ["kaijutsu-app", "kaijutsu-ear", "kaijutsu-patchview"];

/// The DJ thread's auto-connect retry cadence (`super::thread::run_loop`'s
/// `tokio::time::interval` arm) — the patch-bay's 2 s poll idiom, reused.
/// `tokio::time::interval`'s own documented guarantee ("the first tick
/// completes immediately") replaces the old Bevy `Timer`'s hand-primed
/// `elapsed = duration` trick for a prompt cold-start attempt.
pub(crate) const AUTOCONNECT_POLL: Duration = Duration::from_secs(2);

/// The outcome of one auto-connect attempt. `Connected` / `AlreadyWired` /
/// `Unavailable` settle the one-shot for good; `NoSynth` / `Failed` retry.
enum AutoConnectOutcome {
    Connected { name: String, client: i32, port: i32 },
    AlreadyWired,
    NoSynth,
    Failed(String),
    Unavailable,
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

// ── MidiSink — the real, loop-local, ALSA-backed sink ───────────────────────

/// Lazily-opened ALSA seq sink, owned loop-local by [`super::thread::run_loop`]
/// (never a Bevy resource — see the module doc). Opened on the first MIDI cue
/// or click; `failed` latches once an open attempt fails (no `/dev/snd/seq`)
/// so we warn once, not per-cue.
#[derive(Default)]
pub(crate) struct MidiSink {
    #[cfg(target_os = "linux")]
    out: Option<MidiOut>,
    failed: bool,
    /// Set whenever a send is actually issued out the render port (an ABC cue
    /// or a metronome click); drained by [`Self::take_traffic`] once per
    /// select!-iteration burst that touched the sink — see
    /// [`super::thread::DjPulse::RenderTraffic`]'s doc for the coalescing
    /// rationale.
    traffic: bool,
}

impl MidiSink {
    /// Open the sink if it isn't already; `false` if it's unavailable (open
    /// failed once — latched, so we warn once, not per-cue/click).
    #[cfg(target_os = "linux")]
    fn ensure_open(&mut self) -> bool {
        if self.out.is_some() {
            return true;
        }
        if self.failed {
            return false;
        }
        match MidiOut::open() {
            Ok(out) => {
                tracing::info!("kaijutsu-dj MIDI sink open on ALSA seq {:?}", out.addr());
                self.out = Some(out);
                true
            }
            Err(e) => {
                warn!("MIDI sink unavailable (no ALSA seq?): {e}");
                self.failed = true;
                false
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn ensure_open(&mut self) -> bool {
        if !self.failed {
            warn!("MIDI render sink is Linux/ALSA-only; ignoring MIDI cues on this platform");
            self.failed = true;
        }
        false
    }

    /// Patch-bay slice 1: observe the local seq graph through the render
    /// port's own handle, decide (purely) whether/where to auto-connect, and
    /// — only if there's a target — subscribe render → synth. Additive; never
    /// disconnects. Assumes the caller has already opened the sink.
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
            // No target: separate "already wired → defer" (stop) from "no
            // synth yet" (keep waiting) so the caller latches the one-shot
            // correctly.
            None if render_has_outbound_wire(&wires, render) => AutoConnectOutcome::AlreadyWired,
            None => AutoConnectOutcome::NoSynth,
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn try_auto_connect(&self, _patterns: &[&str]) -> AutoConnectOutcome {
        AutoConnectOutcome::Unavailable
    }
}

impl MidiDispatch for MidiSink {
    #[cfg(target_os = "linux")]
    fn click_at(&mut self, note: u8, channel: u8, velocity: u8, gate_ms: u64, offset: Duration) {
        if !self.ensure_open() {
            return;
        }
        if let Some(out) = self.out.as_mut() {
            out.click_at(note, channel, velocity, gate_ms, offset);
            self.traffic = true; // a click left the render port — pulse the chord
        }
    }
    #[cfg(not(target_os = "linux"))]
    fn click_at(&mut self, _note: u8, _channel: u8, _velocity: u8, _gate_ms: u64, _offset: Duration) {
        self.ensure_open();
    }

    #[cfg(target_os = "linux")]
    fn schedule_abc(&mut self, events: Vec<(Duration, Vec<u8>)>, lead: Duration) {
        if !self.ensure_open() {
            return;
        }
        if let Some(out) = self.out.as_mut() {
            out.schedule(&events, lead);
            self.traffic = true; // a cue left the render port — pulse the chord
        }
    }
    #[cfg(not(target_os = "linux"))]
    fn schedule_abc(&mut self, _events: Vec<(Duration, Vec<u8>)>, _lead: Duration) {
        self.ensure_open();
    }

    #[cfg(target_os = "linux")]
    fn flush(&mut self) {
        if let Some(out) = self.out.as_mut() {
            out.flush();
        }
    }
    #[cfg(not(target_os = "linux"))]
    fn flush(&mut self) {}

    fn take_traffic(&mut self) -> bool {
        std::mem::take(&mut self.traffic)
    }

    #[cfg(target_os = "linux")]
    fn tick_autoconnect(&mut self) -> bool {
        // No ALSA seq → `ensure_open` warns once and we stand down: no synth
        // will ever appear on a machine without a sequencer, so retrying is
        // pure spam.
        if !self.ensure_open() {
            return true;
        }
        match self.try_auto_connect(&SYNTH_PATTERNS) {
            AutoConnectOutcome::Connected { name, client, port } => {
                tracing::info!("patch-bay slice 1: auto-connected render → {name}:{port} (client {client})");
                true
            }
            AutoConnectOutcome::AlreadyWired => {
                // A hand-patch already owns the render port's routing. Stand
                // down — never fight a deliberate wiring, and never reverse a
                // later cut.
                debug!("patch-bay slice 1: render already wired; standing down");
                true
            }
            AutoConnectOutcome::NoSynth => {
                debug!("patch-bay slice 1: no GM synth yet; will retry");
                false
            }
            AutoConnectOutcome::Failed(e) => {
                debug!("patch-bay slice 1: connect failed, will retry: {e}");
                false
            }
            AutoConnectOutcome::Unavailable => true,
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn tick_autoconnect(&mut self) -> bool {
        // No ALSA off Linux — nothing to connect, ever.
        true
    }
}

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
                tracing::error!("MIDI encoder init failed: {e}");
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
                        tracing::error!("MIDI event_output failed: {e}");
                    }
                }
                Ok((_, None)) => continue, // incomplete message — shouldn't happen
                Err(e) => {
                    tracing::error!("MIDI encode failed: {e}");
                    continue;
                }
            }
        }
        if let Err(e) = self.seq.drain_output() {
            tracing::error!("MIDI drain_output failed: {e}");
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
                tracing::error!("MIDI remove_events failed: {e}");
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
        // Byte-masking lives in `click_bytes` (config-typo hazard, unit-tested
        // without ALSA); default channel 15 keeps the click off the music's
        // channel 0.
        let (on, off) = click_bytes(note, channel, velocity);
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

    // ── Slice 2 (phase-align): backdate_events ──────────────────────────────

    /// A synthetic 3-event cue: NoteOn/NoteOff at offset 0 (the note that's
    /// most at risk of landing in the past), and a lone later NoteOn at offset
    /// 400ms — enough spread to exercise "some survive, some don't".
    fn synthetic_events() -> Vec<(Duration, Vec<u8>)> {
        vec![
            (Duration::ZERO, vec![0x90, 60, 100]),                    // NoteOn, t=0
            (Duration::from_millis(200), vec![0x80, 60, 0]),          // NoteOff, t=200ms
            (Duration::from_millis(400), vec![0x90, 64, 100]),        // NoteOn, t=400ms
        ]
    }

    #[test]
    fn fresh_cue_shrinks_lead_by_its_age_events_untouched() {
        let events = synthetic_events();
        let lead = Duration::from_millis(300);
        let age = Some(Duration::from_millis(120)); // age <= lead
        let (out_events, out_lead) = backdate_events(events.clone(), lead, age).expect("not stale");
        assert_eq!(out_lead, Duration::from_millis(180), "lead shrinks by exactly the age");
        assert_eq!(out_events, events, "events are untouched when the lead absorbs the age");
    }

    #[test]
    fn age_equal_to_lead_is_the_fresh_boundary_not_the_late_one() {
        // age == lead is the `age <= lead` branch (fresh), not `age > lead`.
        let events = synthetic_events();
        let lead = Duration::from_millis(200);
        let age = Some(Duration::from_millis(200));
        let (out_events, out_lead) = backdate_events(events.clone(), lead, age).expect("not stale");
        assert_eq!(out_lead, Duration::ZERO, "lead fully consumed, but not the late branch");
        assert_eq!(out_events, events, "still untouched — the boundary is inclusive on the fresh side");
    }

    #[test]
    fn late_cue_drops_past_notes_and_shifts_survivors() {
        // lead=100ms, age=350ms → deficit d=250ms. Offset 0 (<250ms) and
        // offset 200ms (<250ms) are dropped; offset 400ms (>=250ms) survives,
        // shifted to 400ms-250ms=150ms.
        let events = synthetic_events();
        let lead = Duration::from_millis(100);
        let age = Some(Duration::from_millis(350));
        let (out_events, out_lead) = backdate_events(events, lead, age).expect("within stale window");
        assert_eq!(out_lead, Duration::ZERO, "no lead left to spend once behind");
        assert_eq!(out_events.len(), 1, "only the offset-400ms event survives: {out_events:?}");
        assert_eq!(out_events[0].0, Duration::from_millis(150), "survivor shifted by -d");
        assert_eq!(out_events[0].1, vec![0x90, 64, 100], "survivor is the second NoteOn");
    }

    #[test]
    fn a_dropped_noteon_orphans_its_later_noteoff_harmlessly() {
        // The NoteOn at offset 0 is dropped (< d), but its NoteOff at 200ms
        // survives when d <= 200ms — an orphaned NoteOff (no matching On) rides
        // through. That's fine: a NoteOff for a note that was never turned on
        // is a no-op at the synth, never a stuck note.
        let events = synthetic_events();
        let lead = Duration::from_millis(50);
        let age = Duration::from_millis(200 + 50); // d = 200ms exactly
        let (out_events, _lead) =
            backdate_events(events, lead, Some(age)).expect("within stale window");
        // Offset-0 NoteOn dropped (0 < 200); offset-200ms NoteOff survives at
        // the boundary (200 >= 200) shifted to 0; offset-400ms NoteOn survives
        // shifted to 200ms.
        assert_eq!(out_events.len(), 2, "the orphaned off + the later on survive: {out_events:?}");
        assert_eq!(out_events[0].1, vec![0x80, 60, 0], "the orphaned NoteOff rides through");
        assert_eq!(out_events[0].0, Duration::ZERO, "shifted to now");
        assert!(
            !out_events.iter().any(|(_, d)| d == &vec![0x90, 60, 100]),
            "the dropped NoteOn must not appear: {out_events:?}"
        );
    }

    #[test]
    fn a_cue_stale_beyond_the_max_is_rejected_outright() {
        let events = synthetic_events();
        let lead = Duration::from_millis(50);
        // deficit = age - lead must exceed CUE_STALE_MAX (5s).
        let age = Some(CUE_STALE_MAX + Duration::from_millis(1) + lead);
        assert_eq!(
            backdate_events(events, lead, age),
            None,
            "a cue this stale is rejected whole, not partially salvaged"
        );
    }

    #[test]
    fn a_cue_exactly_at_the_stale_boundary_is_still_accepted() {
        let events = synthetic_events();
        let lead = Duration::from_millis(50);
        let age = Some(CUE_STALE_MAX + lead); // deficit == CUE_STALE_MAX exactly
        assert!(
            backdate_events(events, lead, age).is_some(),
            "exactly CUE_STALE_MAX deficit is still accepted (boundary is inclusive)"
        );
    }

    #[test]
    fn an_unstamped_cue_passes_through_verbatim() {
        let events = synthetic_events();
        let lead = Duration::from_millis(300);
        let (out_events, out_lead) =
            backdate_events(events.clone(), lead, None).expect("unstamped never rejects");
        assert_eq!(out_lead, lead, "no age to backdate against — lead untouched");
        assert_eq!(out_events, events, "events untouched too");
    }

    // ── dispatch_midi_cue: the events-arm entry point ───────────────────────

    /// A tiny recording double for `dispatch_midi_cue`'s own tests — narrower
    /// than `dj::thread`'s `RecordingMidiSink` (that one drives the whole
    /// `run_loop`; this one just asserts what THIS function decided to do).
    #[derive(Default)]
    struct RecordingSink {
        scheduled: Option<(Vec<(Duration, Vec<u8>)>, Duration)>,
        flushed: bool,
    }

    impl MidiDispatch for RecordingSink {
        fn click_at(&mut self, _: u8, _: u8, _: u8, _: u64, _: Duration) {
            unreachable!("dispatch_midi_cue never clicks")
        }
        fn schedule_abc(&mut self, events: Vec<(Duration, Vec<u8>)>, lead: Duration) {
            self.scheduled = Some((events, lead));
        }
        fn flush(&mut self) {
            self.flushed = true;
        }
        fn take_traffic(&mut self) -> bool {
            false
        }
        fn tick_autoconnect(&mut self) -> bool {
            true
        }
    }

    #[test]
    fn a_flush_cue_flushes_the_sink_and_schedules_nothing() {
        let mut sink = RecordingSink::default();
        let cue = RenderCue::now_inline(RENDER_FLUSH_MIME, Vec::new());
        dispatch_midi_cue(&cue, 0, &mut sink);
        assert!(sink.flushed);
        assert!(sink.scheduled.is_none());
    }

    #[test]
    fn a_non_abc_mime_is_ignored() {
        let mut sink = RecordingSink::default();
        let cue = RenderCue::now_inline("audio/wav", vec![1, 2, 3]);
        dispatch_midi_cue(&cue, 0, &mut sink);
        assert!(!sink.flushed);
        assert!(sink.scheduled.is_none());
    }

    #[test]
    fn an_unstamped_zero_lead_abc_cue_schedules_events_untouched() {
        let mut sink = RecordingSink::default();
        let cue = RenderCue::now_inline(ABC_MIME, CDEF.as_bytes().to_vec());
        dispatch_midi_cue(&cue, 0, &mut sink);
        let (events, lead) = sink.scheduled.expect("ABC cue must schedule");
        assert_eq!(events.len(), abc_to_timed_events(CDEF).len());
        assert_eq!(lead, Duration::ZERO, "unstamped, zero-lead cue: lead untouched");
    }

    /// The CAS-payload warn is a deliberate, kept behavior — CAS-backed ABC
    /// isn't resolved on the DJ thread yet (a recorded follow-up), so a CAS
    /// cue must warn and schedule nothing, never panic or silently vanish.
    #[test]
    fn a_cas_payload_abc_cue_warns_and_schedules_nothing() {
        let mut sink = RecordingSink::default();
        let cue = RenderCue {
            mime: ABC_MIME.into(),
            payload: CuePayload::Cas(kaijutsu_cas::ContentHash::from_data(b"a-score")),
            lead: Duration::ZERO,
            epoch_ns: 0,
        };
        dispatch_midi_cue(&cue, 0, &mut sink);
        assert!(sink.scheduled.is_none());
        assert!(!sink.flushed);
    }

    #[test]
    fn a_stale_abc_cue_is_rejected_and_schedules_nothing() {
        let mut sink = RecordingSink::default();
        let now_epoch_ns: u64 = 100_000_000_000;
        let stale_epoch_ns = now_epoch_ns
            .saturating_sub((CUE_STALE_MAX + Duration::from_secs(2)).as_nanos() as u64);
        let cue = RenderCue {
            mime: ABC_MIME.into(),
            payload: CuePayload::Inline(CDEF.as_bytes().to_vec()),
            lead: Duration::from_millis(50),
            epoch_ns: stale_epoch_ns,
        };
        dispatch_midi_cue(&cue, now_epoch_ns, &mut sink);
        assert!(sink.scheduled.is_none(), "a too-stale cue must not schedule");
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
                if let Ok(ev) = input.event_input()
                    && ev.get_type() == EventType::Noteon
                    && let Some(n) = ev.get_data::<alsa::seq::EvNote>()
                    && n.velocity > 0
                {
                    note_ons.push(n.note);
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

    // -- click_bytes: metronome click byte masking --------------------------

    #[test]
    fn click_bytes_masks_channel_to_the_low_nibble() {
        let (on, off) = click_bytes(60, 0xFF, 100);
        assert_eq!(on[0], 0x9F, "status byte keeps NoteOn nibble + masked channel");
        assert_eq!(off[0], 0x8F, "status byte keeps NoteOff nibble + masked channel");
    }

    #[test]
    fn click_bytes_masks_note_and_velocity_to_seven_bits() {
        // A config typo like velocity=200 (0xC8) must never ride through as a
        // stray status byte (0xC8 & 0xF0 == 0xC0, Program Change).
        let (on, off) = click_bytes(0xFF, 0, 200);
        assert_eq!(on, vec![0x90, 0x7F, 0x48], "note clamped to 7 bits, 200 & 0x7F == 0x48");
        assert_eq!(off, vec![0x80, 0x7F, 0x00], "note-off pitch clamped the same way");
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
