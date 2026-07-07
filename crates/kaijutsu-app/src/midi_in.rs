//! MIDI capture — the app is the first MIDI ear (`docs/midi.md` M2, "the ear
//! is the sink's twin").
//!
//! A dedicated capture thread owns its **own** ALSA seq client (separate from
//! `midi.rs`'s render client: no `!Send` sharing across the frame loop, and
//! echo-exclusion by construction — the ear never subscribes to a
//! `kaijutsu-app` render port). It subscribes ambient-style to every external
//! source port (hotplug included, via System Announce), stamps each event
//! with epoch-ns at receipt, filters realtime spam at ingest
//! (`keep_at_ingest`), and feeds a [`CaptureRing`].
//!
//! On the Bevy side a cutter advances a [`Tracker`] on a wall-clock cadence
//! and ships each non-empty window to the kernel as a `commitCapture` batch
//! — committed against the **session's joined seat** (the server-authoritative
//! `join_context` id; UI browsing through other contexts does NOT re-aim the
//! ear), so `kj transport attach` in that seat is what starts perception.
//! An explicit per-client capture context (`midi_in.toml`) replaces the seat
//! binding when ambient-vs-seat needs separating (docs/issues.md).
//! The cut is gated on having a target: while disconnected or
//! context-less the ring simply accumulates (overwrite is counted, and the
//! first real batch reports it as `lost` — never a silent drop). A kernel
//! refusal (context not attached to a track) is a loud warn each cadence —
//! that noise is the feature: your MIDI is going nowhere, attach.
//!
//! Slice-1 stances (docs/midi.md M2): wall-clock windows (phrase-aligned
//! cuts via the metronome phasor are a follow-on — docs/issues.md), and a
//! refused batch is warned-and-dropped, not requeued (the score's paper
//! trail starts at attach).

use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy::tasks::IoTaskPool;
use kaijutsu_audio::{CaptureRing, Tracker, MIDI_CAPTURE_MIME};

use crate::connection::actor_plugin::{RpcActor, RpcConnectionState};

/// Cut cadence: how often the tracker's window is cut and shipped. Batched
/// telemetry, not realtime — seconds are the design point ("fills up crypto
/// blocks"), and the kernel quantizes to the grid regardless of alignment.
const CUT_INTERVAL: Duration = Duration::from_secs(4);

/// Ring capacity in events. MIDI is tiny; at a dense jam (~50 events/s) this
/// holds several minutes — enough to survive a reconnect without loss.
const RING_CAPACITY: usize = 16_384;

/// Captures ALSA MIDI into a ring and ships windowed batches to the kernel.
pub struct MidiInPlugin;

impl Plugin for MidiInPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CaptureState>();
        // NonSend: the receiver end of the capture thread's channel stays on
        // the main thread with its drain system (same stance as MidiSink).
        app.insert_non_send_resource(MidiEar::default());
        app.add_systems(Startup, open_midi_ear);
        app.add_systems(Update, (drain_ear, cut_and_ship).chain());
    }
}

/// The capture thread's Bevy-side end: a channel of stamped events. `failed`
/// latches an open failure so we warn once, not per-frame.
#[derive(Default)]
pub(crate) struct MidiEar {
    rx: Option<std::sync::mpsc::Receiver<kaijutsu_audio::CaptureEvent>>,
    failed: bool,
}

/// The ring + the score batcher's tracker (the first of the N consumers the
/// ring is built for; analysis windows and a live spray come later).
///
/// Ring and tracker are **born together**: a tracker created later would
/// start at the ring's head and silently skip everything captured before it
/// (`tracker_at` semantics) — the exact loss this design forbids. Born at
/// cursor 0, the tracker sees the whole disconnected backlog, and the
/// ship-gate in [`cut_and_ship`] alone decides when cutting starts.
#[derive(Resource)]
struct CaptureState {
    ring: CaptureRing,
    tracker: Tracker,
    last_cut: Instant,
    /// Latch so "context not ready" logs once per outage, not per cadence.
    waiting_logged: bool,
}

impl Default for CaptureState {
    fn default() -> Self {
        let ring = CaptureRing::new(RING_CAPACITY);
        let tracker = ring.tracker_at(epoch_ns_now());
        Self {
            ring,
            tracker,
            last_cut: Instant::now(),
            waiting_logged: false,
        }
    }
}

/// Startup: spawn the capture thread (Linux/ALSA only). Graceful on failure —
/// no ALSA just means no ear, same as the render sink.
fn open_midi_ear(mut ear: NonSendMut<MidiEar>) {
    if ear.rx.is_some() || ear.failed {
        return;
    }
    match spawn_capture_thread() {
        Ok(rx) => {
            ear.rx = Some(rx);
        }
        Err(e) => {
            warn!("MIDI ear unavailable (no ALSA seq?): {e}");
            ear.failed = true;
        }
    }
}

/// Drain stamped events from the capture thread into the ring. The ring is
/// the loss boundary: a stalled cutter shows up as `lost` on the next batch,
/// never as silent backpressure on the capture thread.
fn drain_ear(ear: NonSend<MidiEar>, mut state: ResMut<CaptureState>) {
    let Some(rx) = ear.rx.as_ref() else {
        return;
    };
    while let Ok(ev) = rx.try_recv() {
        state.ring.push(ev);
    }
}

/// Every `CUT_INTERVAL`, cut the tracker's window and ship it. Gated on a
/// live connection + current context BEFORE the cut, so nothing is consumed
/// from the ring while it has nowhere to go.
fn cut_and_ship(
    mut state: ResMut<CaptureState>,
    conn: Res<RpcConnectionState>,
    actor: Option<Res<RpcActor>>,
) {
    let now = Instant::now();
    if now.duration_since(state.last_cut) < CUT_INTERVAL {
        return;
    }
    state.last_cut = now;

    let target = match (&actor, conn.connected, conn.context_id) {
        (Some(_), true, Some(ctx)) => ctx,
        _ => {
            if !state.waiting_logged {
                info!("MIDI ear: capturing to ring; not shipping until connected with a context");
                state.waiting_logged = true;
            }
            return;
        }
    };
    state.waiting_logged = false;

    let epoch_now_ns = epoch_ns_now();
    let state = &mut *state;
    let batch = state.ring.cut(&mut state.tracker, epoch_now_ns);
    if batch.is_empty() {
        return;
    }
    if batch.lost > 0 {
        warn!(
            "MIDI ear: ring overwrote {} event(s) before this cut — cutter stalled or ring undersized",
            batch.lost
        );
    }
    let payload = match batch.to_json_bytes() {
        Ok(p) => p,
        Err(e) => {
            error!("MIDI ear: batch serialize failed (events dropped): {e}");
            return;
        }
    };

    let handle = actor.expect("target match guarantees actor").handle.clone();
    let events = batch.events.len();
    IoTaskPool::get()
        .spawn(async move {
            match handle.commit_capture(target, MIDI_CAPTURE_MIME, payload).await {
                Ok(block_id) => {
                    info!("MIDI ear: committed {events} event(s) as block {block_id}");
                }
                Err(e) => {
                    // Loud on purpose: playing MIDI with nowhere to land it is
                    // the one state the player must notice (attach the context).
                    warn!("MIDI ear: capture batch refused: {e}");
                }
            }
        })
        .detach();
}

fn epoch_ns_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Spawn the ALSA capture thread: its own seq client ("kaijutsu-ear"), a
/// capture port, ambient subscriptions, blocking event loop → stamped events
/// on the channel. Exits when the Bevy side drops the receiver.
#[cfg(target_os = "linux")]
fn spawn_capture_thread(
) -> Result<std::sync::mpsc::Receiver<kaijutsu_audio::CaptureEvent>, String> {
    use alsa::seq::{Addr, PortCap, PortSubscribe, PortType};
    use std::ffi::CString;

    let map = |e: alsa::Error| format!("{e}");
    // Blocking client: the thread parks in event_input until MIDI arrives.
    let seq = alsa::Seq::open(None, None, false).map_err(map)?;
    seq.set_client_name(&CString::new("kaijutsu-ear").map_err(|e| e.to_string())?)
        .map_err(map)?;
    let port = seq
        .create_simple_port(
            &CString::new("capture").map_err(|e| e.to_string())?,
            PortCap::WRITE | PortCap::SUBS_WRITE,
            PortType::MIDI_GENERIC | PortType::APPLICATION,
        )
        .map_err(map)?;
    let own = seq.client_id().map_err(map)?;
    let dest = Addr { client: own, port };

    // Hotplug: System Announce (0:1) tells us when a new port appears.
    let announce = PortSubscribe::empty().map_err(map)?;
    announce.set_sender(Addr { client: 0, port: 1 });
    announce.set_dest(dest);
    if let Err(e) = seq.subscribe_port(&announce) {
        warn!("MIDI ear: no System Announce subscription (hotplug disabled): {e}");
    }

    // Ambient initial sweep: every external readable port.
    let mut subscribed = 0usize;
    for client in alsa::seq::ClientIter::new(&seq) {
        for p in alsa::seq::PortIter::new(&seq, client.get_client()) {
            let addr = p.addr();
            if subscribe_source(&seq, dest, addr) {
                subscribed += 1;
            }
        }
    }
    info!(
        "kaijutsu-app MIDI ear open on ALSA seq {}:{} ({subscribed} source(s) subscribed)",
        own, port
    );

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("kaijutsu-midi-ear".into())
        .spawn(move || capture_loop(seq, dest, tx))
        .map_err(|e| e.to_string())?;
    Ok(rx)
}

#[cfg(not(target_os = "linux"))]
fn spawn_capture_thread(
) -> Result<std::sync::mpsc::Receiver<kaijutsu_audio::CaptureEvent>, String> {
    Err("MIDI capture is Linux/ALSA-only".into())
}

/// Should the ear listen to this port? Readable-subscribable external sources
/// only: never our own clients (`kaijutsu-app` render = the band's own output
/// — echo; `kaijutsu-ear` = self), never System (0), never "Midi Through"
/// (anything routed through it that we should hear, we already hear at its
/// real source; through-wiring our own output would echo).
#[cfg(target_os = "linux")]
fn subscribe_source(seq: &alsa::Seq, dest: alsa::seq::Addr, addr: alsa::seq::Addr) -> bool {
    use alsa::seq::{PortCap, PortSubscribe};

    if addr.client == 0 || addr.client == dest.client {
        return false;
    }
    let Ok(client_info) = seq.get_any_client_info(addr.client) else {
        return false;
    };
    match client_info.get_name() {
        Ok(name) if name == "kaijutsu-app" || name == "kaijutsu-ear" || name == "Midi Through" => {
            return false;
        }
        Ok(_) => {}
        Err(_) => return false,
    }
    let Ok(pinfo) = seq.get_any_port_info(addr) else {
        return false;
    };
    let caps = pinfo.get_capability();
    if !caps.contains(PortCap::READ | PortCap::SUBS_READ) {
        return false;
    }
    let Ok(subs) = PortSubscribe::empty() else {
        return false;
    };
    subs.set_sender(addr);
    subs.set_dest(dest);
    match seq.subscribe_port(&subs) {
        Ok(()) => {
            info!(
                "MIDI ear: listening to {}:{} ({})",
                addr.client,
                addr.port,
                client_info.get_name().unwrap_or("?")
            );
            true
        }
        Err(e) => {
            // Already-subscribed (announce raced the sweep) and permission
            // refusals both land here; neither is fatal.
            debug!("MIDI ear: subscribe {}:{} failed: {e}", addr.client, addr.port);
            false
        }
    }
}

/// The blocking capture loop: decode each event to raw MIDI bytes, stamp it,
/// send it. `PortStart` announce events feed hotplug subscription instead.
#[cfg(target_os = "linux")]
fn capture_loop(
    seq: alsa::Seq,
    dest: alsa::seq::Addr,
    tx: std::sync::mpsc::Sender<kaijutsu_audio::CaptureEvent>,
) {
    use alsa::seq::EventType;

    let decoder = match alsa::seq::MidiEvent::new(4096) {
        Ok(d) => d,
        Err(e) => {
            error!("MIDI ear: decoder init failed: {e}");
            return;
        }
    };
    // Every event decodes to a complete message with its own status byte.
    decoder.enable_running_status(false);
    let mut buf = [0u8; 4096];

    let mut input = seq.input();
    loop {
        let ev = match input.event_input() {
            Ok(ev) => ev,
            Err(e) => {
                // ENOSPC = kernel-side queue overrun: events were lost. Loud,
                // then keep listening — the ring's lost-counting covers the
                // Bevy side; this covers the ALSA side.
                warn!("MIDI ear: event_input error (events may be lost): {e}");
                continue;
            }
        };
        if ev.get_type() == EventType::PortStart {
            if let Some(addr) = ev.get_data::<alsa::seq::Addr>() {
                subscribe_source(&seq, dest, addr);
            }
            continue;
        }
        let source = ev.get_source();
        let n = match decoder.decode(&mut buf, &mut ev.into_owned()) {
            Ok(n) => n,
            // Non-MIDI events (announce chatter, client start/exit) and
            // oversized sysex land here; neither is a capture event.
            Err(_) => continue,
        };
        if n == 0 {
            continue;
        }
        let bytes = buf[..n].to_vec();
        if !kaijutsu_audio::keep_at_ingest(&bytes) {
            continue;
        }
        let event = kaijutsu_audio::CaptureEvent {
            epoch_ns: epoch_ns_now(),
            source: format!("{}:{}", source.client, source.port),
            bytes,
        };
        if tx.send(event).is_err() {
            return; // Bevy side is gone — shut the ear down.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ship-gate: with no actor/connection/context, the cutter must not
    /// consume from the ring — a cut with nowhere to ship would silently
    /// discard the window. Events survive untouched for the first real cut.
    #[test]
    fn no_target_means_no_cut() {
        let mut app = App::new();
        let mut state = CaptureState::default();
        state.last_cut = Instant::now() - CUT_INTERVAL * 2; // cadence due
        for i in 0..3 {
            state.ring.push(kaijutsu_audio::CaptureEvent {
                epoch_ns: 1_000 + i,
                source: "24:0".into(),
                bytes: vec![0x90, 60, 100],
            });
        }
        app.insert_resource(state)
            .init_resource::<crate::connection::actor_plugin::RpcConnectionState>()
            .add_systems(Update, cut_and_ship);
        app.update();

        let state = app.world().resource::<CaptureState>();
        assert!(state.waiting_logged, "the gate branch ran (cadence was due)");
        // Nothing was consumed: a later cut still sees all three events.
        let batch = state.ring.cut(&mut state.tracker.clone(), u64::MAX);
        assert_eq!(batch.events.len(), 3, "the ring kept the whole backlog");
    }

    /// The cutter's loss boundary: the tracker is born WITH the ring (cursor
    /// 0), so events captured before shipping is possible (disconnected, no
    /// context) are not skipped — the first real cut carries the whole
    /// backlog. A tracker created later would start at the ring head and
    /// silently drop that backlog; this test pins the born-together shape.
    #[test]
    fn the_first_cut_carries_the_preconnection_backlog() {
        let mut state = CaptureState::default();
        // Capture while disconnected: events pile into the ring, no cutting.
        let base = epoch_ns_now();
        for i in 0..5 {
            state.ring.push(kaijutsu_audio::CaptureEvent {
                epoch_ns: base + i,
                source: "24:0".into(),
                bytes: vec![0x90, 60, 100],
            });
        }
        // Target appears: the first cut ships everything, nothing lost.
        let batch = state.ring.cut(&mut state.tracker, base + 1_000);
        assert_eq!(batch.events.len(), 5, "the backlog ships in the first batch");
        assert_eq!(batch.lost, 0);
    }
}
