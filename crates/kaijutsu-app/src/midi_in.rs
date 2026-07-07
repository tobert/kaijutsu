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
        app.init_resource::<ClockSense>();
        // NonSend: the receiver end of the capture thread's channel stays on
        // the main thread with its drain system (same stance as MidiSink).
        app.insert_non_send_resource(MidiEar::default());
        app.add_systems(Startup, open_midi_ear);
        app.add_systems(Update, (drain_ear, cut_and_ship, ship_clock_estimates).chain());
    }
}

/// What the capture thread sends across to Bevy: a stamped score-capture
/// event, or a clock-sense estimate from the pre-ring tap (`docs/midi.md`
/// M3 — pulses never enter the ring; the estimator taps them in the capture
/// thread and only low-rate estimates cross).
pub(crate) enum EarEvent {
    Capture(kaijutsu_audio::CaptureEvent),
    Clock {
        /// Source port ("client:port") the clock master was observed on.
        source: String,
        estimate: kaijutsu_audio::ClockEstimate,
        /// The estimator's monotonic stall counter (position may have
        /// slipped when this moves).
        discontinuities: u64,
    },
}

/// The capture thread's Bevy-side end: a channel of stamped events. `failed`
/// latches an open failure so we warn once, not per-frame.
#[derive(Default)]
pub(crate) struct MidiEar {
    rx: Option<std::sync::mpsc::Receiver<EarEvent>>,
    failed: bool,
}

/// Latest clock-sense per observed master (the M3 slice-2 observability
/// surface: the wire to the kernel is slice 3; the time well can read this
/// later). One entry per source port that has ever emitted an estimate.
#[derive(Resource, Default)]
pub struct ClockSense {
    pub sources: std::collections::HashMap<String, ClockSenseEntry>,
}

pub struct ClockSenseEntry {
    pub estimate: kaijutsu_audio::ClockEstimate,
    pub discontinuities: u64,
    pub received: Instant,
    /// BPM at the last info-level log line (throttle: re-log on ≥1 BPM move).
    last_logged_bpm: f64,
    /// False when this estimate hasn't crossed to the kernel yet; the ship
    /// system flips it (latest-wins — a skipped intermediate is jitter).
    shipped: bool,
    /// Ship-failure warn latch (cleared on success) so a refusal warns once
    /// per episode, not at 2 Hz.
    ship_warned: std::sync::Arc<std::sync::atomic::AtomicBool>,
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

/// Drain the capture thread's channel: score captures into the ring (the
/// loss boundary — a stalled cutter shows up as `lost` on the next batch,
/// never as backpressure on the capture thread), clock estimates into
/// [`ClockSense`] with throttled logging (lock, ≥1 BPM moves, stalls).
fn drain_ear(
    ear: NonSend<MidiEar>,
    mut state: ResMut<CaptureState>,
    mut clock: ResMut<ClockSense>,
) {
    let Some(rx) = ear.rx.as_ref() else {
        return;
    };
    while let Ok(ev) = rx.try_recv() {
        match ev {
            EarEvent::Capture(ev) => state.ring.push(ev),
            EarEvent::Clock { source, estimate, discontinuities } => {
                let bpm = estimate.reference.tempo_bps * 60.0;
                match clock.sources.get_mut(&source) {
                    Some(entry) => {
                        if discontinuities > entry.discontinuities {
                            warn!(
                                "MIDI clock {source}: stall observed (position may have \
                                 slipped; {discontinuities} total)"
                            );
                        }
                        if (bpm - entry.last_logged_bpm).abs() >= 1.0 {
                            info!("MIDI clock {source}: {bpm:.1} BPM (beat {:.1})",
                                estimate.reference.beat);
                            entry.last_logged_bpm = bpm;
                        }
                        entry.estimate = estimate;
                        entry.discontinuities = discontinuities;
                        entry.received = Instant::now();
                        entry.shipped = false;
                    }
                    None => {
                        info!("MIDI clock lock: {source} at {bpm:.1} BPM");
                        clock.sources.insert(
                            source,
                            ClockSenseEntry {
                                estimate,
                                discontinuities,
                                received: Instant::now(),
                                last_logged_bpm: bpm,
                                shipped: false,
                                ship_warned: Default::default(),
                            },
                        );
                    }
                }
            }
        }
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

/// Ship un-shipped clock estimates to the kernel (`docs/midi.md` M3 — the
/// reverse of the `BeatSync` push). Latest-wins per source: an estimate
/// superseded before shipping is jitter, not loss. Same target rule as the
/// capture cutter (the session's joined seat); while target-less the sense
/// stays local (ClockSense still feeds logs/UI) — clock references are
/// ambient, so unlike capture there is nothing to hold back for later.
/// A refusal warns once per episode (the latch), not at 2 Hz.
fn ship_clock_estimates(
    mut clock: ResMut<ClockSense>,
    conn: Res<RpcConnectionState>,
    actor: Option<Res<RpcActor>>,
) {
    let target = match (&actor, conn.connected, conn.context_id) {
        (Some(_), true, Some(ctx)) => ctx,
        _ => return,
    };
    let handle = &actor.as_ref().expect("target match guarantees actor").handle;
    for (source, entry) in clock.sources.iter_mut() {
        if entry.shipped {
            continue;
        }
        entry.shipped = true;
        let handle = handle.clone();
        let source = source.clone();
        let estimate = entry.estimate;
        let warned = entry.ship_warned.clone();
        IoTaskPool::get()
            .spawn(async move {
                let r = handle
                    .report_clock_estimate(
                        target,
                        estimate.reference.beat,
                        estimate.reference.tempo_bps,
                        estimate.epoch_ns,
                        source.clone(),
                    )
                    .await;
                match r {
                    Ok(()) => warned.store(false, std::sync::atomic::Ordering::Relaxed),
                    Err(e) => {
                        if !warned.swap(true, std::sync::atomic::Ordering::Relaxed) {
                            warn!("MIDI clock {source}: estimate refused by kernel: {e}");
                        }
                    }
                }
            })
            .detach();
    }
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
fn spawn_capture_thread() -> Result<std::sync::mpsc::Receiver<EarEvent>, String> {
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
fn spawn_capture_thread() -> Result<std::sync::mpsc::Receiver<EarEvent>, String> {
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
/// send it. `PortStart` announce events feed hotplug subscription; clock
/// events feed the **pre-ring tap** — a per-source `ClockEstimator`
/// (`docs/midi.md` M3). `F8` pulses are tap-exclusive (24 PPQN would flood
/// the ring and no score consumer wants them); Start/Stop/Continue/
/// SongPosition feed the tap AND fall through to the ring, because
/// transport intent is score-meaningful capture too.
#[cfg(target_os = "linux")]
fn capture_loop(
    seq: alsa::Seq,
    dest: alsa::seq::Addr,
    tx: std::sync::mpsc::Sender<EarEvent>,
) {
    use alsa::seq::EventType;
    use kaijutsu_audio::{ClockEstimator, ClockEvent};
    use std::collections::HashMap;

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
    // One estimator per observed clock master (source port).
    let mut clocks: HashMap<String, ClockEstimator> = HashMap::new();

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

        // The clock tap, BEFORE the ring's door filter — its stamps are the
        // estimator's measurements, taken here at receipt, per source.
        let now_ns = epoch_ns_now();
        let source_addr = ev.get_source();
        let source = format!("{}:{}", source_addr.client, source_addr.port);
        let clock_event = match ev.get_type() {
            EventType::Clock => Some(ClockEvent::Pulse { epoch_ns: now_ns }),
            EventType::Start => Some(ClockEvent::Start { epoch_ns: now_ns }),
            EventType::Continue => Some(ClockEvent::Continue { epoch_ns: now_ns }),
            EventType::Stop => Some(ClockEvent::Stop { epoch_ns: now_ns }),
            EventType::Songpos => ev.get_data::<alsa::seq::EvCtrl>().map(|c| {
                ClockEvent::SongPosition { epoch_ns: now_ns, sixteenths: c.value.max(0) as u16 }
            }),
            _ => None,
        };
        if let Some(ce) = clock_event {
            let est = clocks.entry(source.clone()).or_insert_with(ClockEstimator::new);
            if let Some(estimate) = est.observe(ce) {
                let msg = EarEvent::Clock {
                    source: source.clone(),
                    estimate,
                    discontinuities: est.discontinuities,
                };
                if tx.send(msg).is_err() {
                    return; // Bevy side is gone — shut the ear down.
                }
            }
            if ev.get_type() == EventType::Clock {
                continue; // pulses are tap-exclusive; the rest fall through
            }
        }

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
            epoch_ns: now_ns,
            source,
            bytes,
        };
        if tx.send(EarEvent::Capture(event)).is_err() {
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

    /// The drain routes by `EarEvent` kind: captures into the ring, clock
    /// estimates into `ClockSense` (keyed by source, discontinuities and
    /// latest estimate carried through). The tap's whole Bevy-side surface.
    #[test]
    fn drain_routes_captures_to_ring_and_clock_to_sense() {
        use kaijutsu_audio::{BeatRef, ClockEstimate};

        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(EarEvent::Capture(kaijutsu_audio::CaptureEvent {
            epoch_ns: 1,
            source: "24:0".into(),
            bytes: vec![0x90, 60, 100],
        }))
        .unwrap();
        tx.send(EarEvent::Clock {
            source: "36:0".into(),
            estimate: ClockEstimate {
                reference: BeatRef::new(4.0, 2.0),
                epoch_ns: 2,
                residual_ns: 500,
            },
            discontinuities: 1,
        })
        .unwrap();

        let mut app = App::new();
        app.init_resource::<CaptureState>()
            .init_resource::<ClockSense>()
            .add_systems(Update, drain_ear);
        app.world_mut()
            .insert_non_send_resource(MidiEar { rx: Some(rx), failed: false });
        app.update();

        assert_eq!(app.world().resource::<CaptureState>().ring.len(), 1);
        let sense = app.world().resource::<ClockSense>();
        let entry = sense.sources.get("36:0").expect("clock master registered");
        assert_eq!(entry.discontinuities, 1);
        assert!((entry.estimate.reference.tempo_bps - 2.0).abs() < 1e-9);
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
