//! MIDI capture and hotplug events from the local hardware backend.
use tracing::{debug, error, info, warn};
use std::sync::{Arc, atomic::{AtomicBool, AtomicU64, Ordering}};
use std::time::{Duration, Instant};

const INGRESS_EVENTS: usize = 128;
const MAX_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_INVENTORY_PORTS: usize = 256;

#[derive(Clone, Debug)]
pub struct ObservedPort {
    pub facts: crate::midi_match::PortFacts,
    pub readable: bool,
    pub writable: bool,
    pub listening: bool,
    pub error: Option<String>,
}

impl serde::Serialize for ObservedPort {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("ObservedPort", 8)?;
        s.serialize_field("address", &self.facts.address)?;
        s.serialize_field("client_name", &self.facts.client_name)?;
        s.serialize_field("port_name", &self.facts.port_name)?;
        s.serialize_field("usb_id", &self.facts.usb_id)?;
        s.serialize_field("readable", &self.readable)?;
        s.serialize_field("writable", &self.writable)?;
        s.serialize_field("listening", &self.listening)?;
        s.serialize_field("error", &self.error)?;
        s.end()
    }
}

struct EarSender {
    tx: std::sync::mpsc::SyncSender<EarEvent>,
    losses: Arc<AtomicU64>,
}

impl EarSender {
    fn send(&self, event: EarEvent) -> Result<(), ()> {
        match self.tx.try_send(event) {
            Ok(()) => Ok(()),
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                self.losses.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => Err(()),
        }
    }
}

pub(crate) struct EarWorker {
    rx: Option<std::sync::mpsc::Receiver<EarEvent>>,
    pub losses: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl EarWorker {
    pub fn take_rx(&mut self) -> std::sync::mpsc::Receiver<EarEvent> {
        self.rx.take().expect("MIDI ingress receiver already taken")
    }
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
    Capture { event: kaijutsu_audio::CaptureEvent, observed_at: Instant },
    Inventory { ports: Vec<ObservedPort>, observed_at: Instant },
    InventoryError { error: String },
    Watermark { observed_at: Instant, epoch_ns: u64 },
    Clock {
        /// Source port ("client:port") the clock master was observed on.
        source: String,
        estimate: kaijutsu_audio::ClockEstimate,
        /// The estimator's monotonic stall counter (position may have
        /// slipped when this moves).
        discontinuities: u64,
    },
    /// A new endpoint incarnation announced by hotplug. Full snapshots
    /// reconcile missed notices; the matcher only receives portable facts.
    PortUp(crate::midi_match::PortFacts),
    /// A port vanished. The observer invalidates its generation immediately.
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
/// capture port, ambient subscriptions, readiness-driven reads and bounded
/// nonblocking ingress. Exits when the receiver closes or shutdown is set.
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
    let (channel_tx, rx) = std::sync::mpsc::sync_channel(INGRESS_EVENTS);
    let losses = Arc::new(AtomicU64::new(0));
    let tx = EarSender { tx: channel_tx, losses: losses.clone() };

    // Ambient initial sweep: every external readable port gets subscribed;
    // every external port at all (readable or not — a synth's input port is
    // still evidence the device is here) gets reported for presence matching.
    let initial = scan_ports(&seq, dest)?;
    let subscribed = initial.iter().filter(|p| p.listening).count();
    let _ = tx.send(EarEvent::Inventory { ports: initial, observed_at: Instant::now() });
    info!(
        "MIDI ear open on ALSA seq {}:{} ({subscribed} source(s) subscribed)",
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
    Ok(EarWorker { rx: Some(rx), losses, stop, join: Some(join) })
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
fn subscribe_source(seq: &alsa::Seq, dest: alsa::seq::Addr, addr: alsa::seq::Addr) -> Result<bool, String> {
    use alsa::seq::{PortCap, PortSubscribe, PortSubscribeIter, QuerySubsType};

    if addr.client == 0 || addr.client == dest.client {
        return Ok(false);
    }
    let Ok(client_info) = seq.get_any_client_info(addr.client) else {
        return Ok(false);
    };
    match client_info.get_name() {
        Ok(name) if is_own_client(name) => {
            return Ok(false);
        }
        Ok(_) => {}
        Err(e) => return Err(e.to_string()),
    }
    let Ok(pinfo) = seq.get_any_port_info(addr) else {
        return Ok(false);
    };
    let caps = pinfo.get_capability();
    if !caps.contains(PortCap::READ | PortCap::SUBS_READ) {
        return Ok(false);
    }
    if PortSubscribeIter::new(seq, addr, QuerySubsType::READ).any(|s| s.get_dest() == dest) {
        return Ok(true);
    }
    let Ok(subs) = PortSubscribe::empty() else {
        return Err("cannot allocate MIDI subscription".into());
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
            Ok(true)
        }
        Err(e) => {
            // Already-subscribed (announce raced the sweep) and permission
            // refusals both land here; neither is fatal.
            debug!("MIDI ear: subscribe {}:{} failed: {e}", addr.client, addr.port);
            if PortSubscribeIter::new(seq, addr, QuerySubsType::READ).any(|s| s.get_dest() == dest) {
                Ok(true)
            } else { Err(e.to_string()) }
        }
    }
}

#[cfg(target_os = "linux")]
fn scan_ports(seq: &alsa::Seq, dest: alsa::seq::Addr) -> Result<Vec<ObservedPort>, String> {
    use alsa::seq::PortCap;
    let mut ports = Vec::new();
    for client in alsa::seq::ClientIter::new(seq) {
        for p in alsa::seq::PortIter::new(seq, client.get_client()) {
            let Some(facts) = port_facts(seq, dest.client, p.addr()) else { continue; };
            if ports.len() == MAX_INVENTORY_PORTS {
                return Err(format!("MIDI inventory exceeds {MAX_INVENTORY_PORTS} ports; full snapshot unavailable"));
            }
            let caps = p.get_capability();
            let result = subscribe_source(seq, dest, p.addr());
            ports.push(ObservedPort { facts,
                readable: caps.contains(PortCap::READ),
                writable: caps.contains(PortCap::WRITE),
                listening: result.as_ref().copied().unwrap_or(false), error: result.err() });
        }
    }
    Ok(ports)
}

/// Decode raw MIDI with receipt stamps. Clock messages also feed the local
/// estimator. Musical recording filters clock/active sensing downstream;
/// retrospective history retains them with every other admitted message.
#[cfg(target_os = "linux")]
fn capture_loop(
    seq: alsa::Seq,
    dest: alsa::seq::Addr,
    tx: EarSender,
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

    let decoder = match alsa::seq::MidiEvent::new(MAX_MESSAGE_BYTES as u32) {
        Ok(d) => d,
        Err(e) => {
            error!("MIDI ear: decoder init failed: {e}");
            return;
        }
    };
    // Every event decodes to a complete message with its own status byte.
    decoder.enable_running_status(false);
    let mut buf = vec![0u8; MAX_MESSAGE_BYTES];
    // One estimator per observed clock master (source port).
    let mut clocks: HashMap<String, ClockEstimator> = HashMap::new();

    let mut input = seq.input();
    let mut next_scan = Instant::now() + Duration::from_secs(2);
    while !stop.load(Ordering::Relaxed) {
        if Instant::now() >= next_scan {
            let report = match scan_ports(&seq, dest) {
                Ok(ports) => EarEvent::Inventory { ports, observed_at: Instant::now() },
                Err(error) => EarEvent::InventoryError { error },
            };
            if tx.send(report).is_err() { return; }
            next_scan = Instant::now() + Duration::from_secs(2);
        }
        let ev = match input.event_input() {
            Ok(ev) => ev,
            Err(e) if e.errno() == libc::EAGAIN => {
                if tx.send(EarEvent::Watermark { observed_at: Instant::now(), epoch_ns: epoch_ns_now() }).is_err() {
                    return;
                }
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
                if e.errno() == libc::ENOSPC { tx.losses.fetch_add(1, Ordering::Relaxed); continue; }
                if e.errno() == libc::EINTR { continue; }
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
                    let _ = subscribe_source(&seq, dest, addr);
                    next_scan = Instant::now();
                }
                continue;
            }
            EventType::PortExit => {
                if let Some(addr) = ev.get_data::<alsa::seq::Addr>() {
                    let address = format!("{}:{}", addr.client, addr.port);
                    clocks.remove(&address);
                    next_scan = Instant::now();
                    if tx.send(EarEvent::PortDown { address }).is_err() {
                        return;
                    }
                }
                continue;
            }
            EventType::ClientExit => {
                next_scan = Instant::now();
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
        let observed_at = Instant::now();
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
        }

        if ev.get_ext().is_some_and(|bytes| bytes.len() > MAX_MESSAGE_BYTES) {
            tx.losses.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let n = match decoder.decode(&mut buf, &mut ev.into_owned()) {
            Ok(n) => n,
            // Failed decoding is counted as unknown-source loss.
            Err(_) => { tx.losses.fetch_add(1, Ordering::Relaxed); continue; },
        };
        if n == 0 {
            continue;
        }
        let bytes = buf[..n].to_vec();
        let event = kaijutsu_audio::CaptureEvent {
            epoch_ns: now_ns,
            source,
            bytes,
        };
        if tx.send(EarEvent::Capture { event, observed_at }).is_err() {
            return; // Bevy side is gone — shut the ear down.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_ingress_counts_loss_without_blocking_and_disconnects() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let losses = Arc::new(AtomicU64::new(0));
        let sender = EarSender { tx, losses: losses.clone() };
        sender.send(EarEvent::PortDown { address: "24:0".into() }).unwrap();
        sender.send(EarEvent::PortDown { address: "25:0".into() }).unwrap();
        assert_eq!(losses.load(Ordering::Relaxed), 1);
        assert!(matches!(rx.try_recv(), Ok(EarEvent::PortDown { address }) if address == "24:0"));
        drop(rx);
        assert!(sender.send(EarEvent::PortDown { address: "24:0".into() }).is_err());
    }
}
