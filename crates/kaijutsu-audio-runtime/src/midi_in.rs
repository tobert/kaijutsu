//! MIDI capture and hotplug events from the local hardware backend.
use tracing::{debug, error, info, warn};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

pub(crate) struct EarWorker {
    pub rx: std::sync::mpsc::Receiver<EarEvent>,
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Drop for EarWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

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
    /// A port exists: seen in the initial sweep or announced by hotplug
    /// (`docs/midi-next.md` "Presence is sink-fed" — Announce is the trigger,
    /// nothing polls). Backend-neutral facts only; the matcher never sees an
    /// ALSA type.
    PortUp(crate::midi_match::PortFacts),
    /// A port vanished. Address only — its names left with it, so the Bevy
    /// side resolves them from the topology it already holds. **Unplug is a
    /// first-class event**: it becomes a `present=false` report, never a
    /// silence.
    PortDown { address: String },
    /// A whole client vanished (belt-and-braces for a backend that reports
    /// the client exit without a `PortExit` per port): every port under it is
    /// gone.
    ClientDown { client_id: i32 },
}

fn is_own_client(name: &str) -> bool {
    matches!(name, "kaijutsu-audio" | "kaijutsu-app" | "kaijutsu-ear" | "kaijutsu-exchange" | "kaijutsu-patchview" | "Midi Through")
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
pub(crate) fn spawn_capture_thread(priority: u8) -> Result<EarWorker, String> {
    use alsa::seq::{Addr, PortCap, PortSubscribe, PortType};
    use std::ffi::CString;

    let map = |e: alsa::Error| format!("{e}");
    // Poll waits for input and bounds shutdown latency on a quiet device.
    let seq = alsa::Seq::open(None, None, true).map_err(map)?;
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
    seq.subscribe_port(&announce).map_err(|e| format!("cannot subscribe to MIDI hotplug events: {e}"))?;

    // The channel exists before the sweep so the sweep's port facts ride it:
    // presence needs the ports that were already there at startup, not only
    // the ones that arrive later by announce.
    let (tx, rx) = std::sync::mpsc::channel();

    // Ambient initial sweep: every external readable port gets subscribed;
    // every external port at all (readable or not — a synth's input port is
    // still evidence the device is here) gets reported for presence matching.
    let mut subscribed = 0usize;
    for client in alsa::seq::ClientIter::new(&seq) {
        for p in alsa::seq::PortIter::new(&seq, client.get_client()) {
            let addr = p.addr();
            if let Some(facts) = port_facts(&seq, dest.client, addr) {
                let _ = tx.send(EarEvent::PortUp(facts));
            }
            if subscribe_source(&seq, dest, addr) {
                subscribed += 1;
            }
        }
    }
    info!(
        "kaijutsu-app MIDI ear open on ALSA seq {}:{} ({subscribed} source(s) subscribed)",
        own, port
    );
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let join = std::thread::Builder::new()
        .name("kaijutsu-midi-ear".into())
        .spawn(move || {
            if let Err(e) = crate::scheduling::set_priority(priority) {
                warn!("{e}; continuing at the current scheduling priority");
            }
            capture_loop(seq, dest, tx, worker_stop)
        })
        .map_err(|e| e.to_string())?;
    Ok(EarWorker { rx, stop, join: Some(join) })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn spawn_capture_thread(_priority: u8) -> Result<EarWorker, String> {
    Err("MIDI capture is Linux/ALSA-only".into())
}

/// Backend-neutral facts about one ALSA port, or `None` when it isn't a
/// device at all: the System client (0), our own clients (render/ear/
/// patchview), and "Midi Through" are plumbing, not gear. This is the ONLY
/// place ALSA types touch the presence path — everything downstream
/// (`crate::midi_match`, the wire, the kernel) speaks display names and an
/// opaque address, which is what lets a CoreMIDI backend reuse all of it.
///
/// `usb_id` is left `None`: the sysfs walk from an ALSA card to its USB
/// `vendor:product` is deferred (`docs/midi-next.md` slice 1 step 3 allows
/// it), and the matcher already prefers a USB ID when one appears — filling
/// this field is the whole of that follow-up.
#[cfg(target_os = "linux")]
fn port_facts(
    seq: &alsa::Seq,
    own_client: i32,
    addr: alsa::seq::Addr,
) -> Option<crate::midi_match::PortFacts> {
    if addr.client == 0 || addr.client == own_client {
        return None;
    }
    let client_info = seq.get_any_client_info(addr.client).ok()?;
    let client_name = client_info.get_name().ok()?.to_string();
    if is_own_client(&client_name) {
        return None;
    }
    let pinfo = seq.get_any_port_info(addr).ok()?;
    let port_name = pinfo.get_name().unwrap_or("?").to_string();
    Some(crate::midi_match::PortFacts {
        client_name,
        port_name,
        address: format!("{}:{}", addr.client, addr.port),
        usb_id: None,
    })
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
        Ok(name) if is_own_client(name) => {
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
    stop: Arc<AtomicBool>,
) {
    use alsa::seq::EventType;
    use kaijutsu_audio::{ClockEstimator, ClockEvent};
    use std::collections::HashMap;
    use alsa::poll::Descriptors;

    let mut descriptors = match (&seq, Some(alsa::Direction::Capture)).get() {
        Ok(descriptors) => descriptors,
        Err(e) => { error!("cannot poll MIDI capture: {e}"); return; }
    };

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
    while !stop.load(Ordering::Relaxed) {
        let ev = match input.event_input() {
            Ok(ev) => ev,
            Err(e) if e.errno() == libc::EAGAIN => {
                if let Err(e) = alsa::poll::poll(&mut descriptors, 20)
                    && e.errno() != libc::EINTR {
                    error!("MIDI input poll failed: {e}");
                    return;
                }
                continue;
            }
            Err(e) => {
                // ENOSPC = kernel-side queue overrun: events were lost. Loud,
                // then keep listening — the ring's lost-counting covers the
                // Bevy side; this covers the ALSA side.
                warn!("MIDI ear: event_input error (events may be lost): {e}");
                if e.errno() == libc::ENOSPC || e.errno() == libc::EINTR { continue; }
                return;
            }
        };
        // Hotplug, both directions. Arrival subscribes the ear AND feeds
        // presence; departure feeds presence only (there is nothing left to
        // unsubscribe). A vanished port must be *reported* gone — silence
        // would leave the kernel holding a presence fact that has become a
        // lie (`docs/midi-next.md`).
        match ev.get_type() {
            EventType::PortStart => {
                if let Some(addr) = ev.get_data::<alsa::seq::Addr>() {
                    if let Some(facts) = port_facts(&seq, dest.client, addr)
                        && tx.send(EarEvent::PortUp(facts)).is_err()
                    {
                        return;
                    }
                    subscribe_source(&seq, dest, addr);
                }
                continue;
            }
            EventType::PortExit => {
                if let Some(addr) = ev.get_data::<alsa::seq::Addr>() {
                    let address = format!("{}:{}", addr.client, addr.port);
                    if tx.send(EarEvent::PortDown { address }).is_err() {
                        return;
                    }
                }
                continue;
            }
            EventType::ClientExit => {
                if let Some(addr) = ev.get_data::<alsa::seq::Addr>()
                    && tx.send(EarEvent::ClientDown { client_id: addr.client }).is_err()
                {
                    return;
                }
                continue;
            }
            _ => {}
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
            let est = clocks.entry(source.clone()).or_default();
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
