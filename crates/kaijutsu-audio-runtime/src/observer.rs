//! Continuous input observation independent of network reconciliation.
use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}};
use std::time::{Duration, Instant};
use crate::history::{HistoryLimits, HistoryStore};
use crate::midi_in::{EarEvent, EarWorker, ObservedPort};

pub(crate) struct Observation {
    pub history: HistoryStore,
    pub ports: Vec<ObservedPort>,
    pub ready: bool,
    pub error: Option<String>,
    pub ingress_lost: u64,
    pub legacy_lost: u64,
    pub head: Option<(Instant, u64)>,
    /// Monotonic per-source MIDI event count, keyed by ALSA address
    /// (`"client:port"`) — the inventory report's `events` field for an
    /// input endpoint (`docs/audio-daemon.md` "One inventory owner").
    /// Counts every observed Capture event, independent of whether the
    /// bounded retention ring admitted it — this is "did the ear see
    /// traffic", not "is it still retained".
    pub event_counts: std::collections::BTreeMap<String, u64>,
    ingress_floor: Option<Instant>,
}

impl Observation {
    pub fn new() -> Self {
        Self { history: HistoryStore::new(HistoryLimits::default()).expect("valid MIDI history defaults"),
            ports: Vec::new(), ready: false, error: None, ingress_lost: 0, legacy_lost: 0, head: None,
            event_counts: std::collections::BTreeMap::new(), ingress_floor: None }
    }

    pub fn inventory(&mut self) -> serde_json::Value {
        serde_json::json!({
            "backend": "alsa", "state": if self.error.is_some() { "error" } else if self.ready { "ready" } else { "pending" },
            "ports": self.ports, "sources": self.history.inventory(self.head.map(|h| h.0).unwrap_or_else(Instant::now)),
            "head_age_ms": self.head.map(|h| h.0.elapsed().as_millis() as u64),
            "error": self.error, "ingress_lost": self.ingress_lost, "recording_ingress_lost": self.legacy_lost,
            "retention_seconds": 60, "source_bytes": 1024 * 1024, "node_bytes": 16 * 1024 * 1024,
        })
    }

    fn ingest(&mut self, event: &EarEvent) {
        let observed_at = match event {
            EarEvent::Capture { observed_at, .. } | EarEvent::Inventory { observed_at, .. }
                | EarEvent::Watermark { observed_at, .. } => Some(*observed_at),
            _ => None,
        };
        if observed_at.zip(self.ingress_floor).is_some_and(|(at, floor)| at < floor) { return; }
        match event {
            EarEvent::Watermark { observed_at, epoch_ns } => self.head = Some((*observed_at, *epoch_ns)),
            EarEvent::Capture { event, observed_at } => {
                *self.event_counts.entry(event.source.clone()).or_insert(0) += 1;
                if let Err(error) = self.history.ingest(event.clone(), *observed_at) { self.error = Some(error); }
            }
            EarEvent::Inventory { ports, observed_at } => {
                self.error = self.history.reconcile(ports, *observed_at).err();
                self.ports = ports.clone();
                self.ready = true;
            }
            EarEvent::InventoryError { error } => {
                self.head = None;
                self.ready = false;
                self.error = Some(error.clone());
                self.history.mark_loss(Instant::now());
                let _ = self.history.reconcile(&[], Instant::now());
            }
            EarEvent::PortDown { address } => self.history.port_down(address),
            EarEvent::PortUp(facts) => self.history.port_down(&facts.address),
            EarEvent::ClientDown { client_id } => {
                for port in &self.ports {
                    if port.facts.address.starts_with(&format!("{client_id}:")) { self.history.port_down(&port.facts.address); }
                }
            }
            _ => {}
        }
    }

    fn ingress_loss(&mut self, lost: u64, now: Instant) {
        self.ingress_floor = Some(now);
        self.head = None;
        self.ready = false;
        self.history.mark_loss(now);
        let _ = self.history.reconcile(&[], now);
        self.ingress_lost = lost;
    }
}

pub(crate) struct Observer {
    pub shared: Arc<Mutex<Observation>>,
    pub rx: std::sync::mpsc::Receiver<EarEvent>,
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Observer {
    pub fn start(mut ear: EarWorker, record: bool) -> Result<Self, String> {
        let rx = ear.take_rx();
        let (legacy_tx, legacy_rx) = std::sync::mpsc::sync_channel(128);
        let shared = Arc::new(Mutex::new(Observation::new()));
        let observed = shared.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let join = std::thread::Builder::new().name("kaijutsu-midi-history".into()).spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                let event = rx.recv_timeout(Duration::from_millis(20));
                let mut state = observed.lock().expect("MIDI observation lock poisoned");
                let lost = ear.losses.load(Ordering::Relaxed);
                if lost != state.ingress_lost {
                    state.ingress_loss(lost, Instant::now());
                    tracing::warn!(lost, "MIDI ingress overran; retained coverage invalidated");
                }
                match event {
                    Ok(event) => {
                        state.ingest(&event);
                        if matches!(event, EarEvent::Watermark { .. }) { continue; }
                        if !record && matches!(event, EarEvent::Capture { .. }) { continue; }
                        if let Err(error) = legacy_tx.try_send(event) {
                            match error {
                                std::sync::mpsc::TrySendError::Full(_) => {
                                    state.legacy_lost += 1;
                                }
                                std::sync::mpsc::TrySendError::Disconnected(_) => break,
                            }
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        state.head = None;
                        state.error = Some("MIDI capture worker stopped".into());
                        state.history.mark_loss(Instant::now());
                        let _ = state.history.reconcile(&[], Instant::now());
                        break;
                    }
                }
            }
            drop(ear);
        }).map_err(|e| e.to_string())?;
        Ok(Self { shared, rx: legacy_rx, stop, join: Some(join) })
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() { let _ = join.join(); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ports() -> Vec<ObservedPort> {
        vec![ObservedPort { facts: crate::midi_match::PortFacts { address:"24:0".into(), client_name:"JD-Xi".into(), port_name:"MIDI".into(), usb_id:None },
            readable:true, writable:true, listening:true, error:None }]
    }
    #[test]
    fn queued_inventory_cannot_restore_coverage_before_ingress_loss() {
        let now = Instant::now();
        let mut observation = Observation::new();
        observation.ingest(&EarEvent::Inventory { ports:ports(), observed_at:now });
        observation.ingress_loss(1, now + Duration::from_secs(3));
        observation.ingest(&EarEvent::Inventory { ports:ports(), observed_at:now });
        let source = observation.history.inventory(now + Duration::from_secs(4))[0].clone();
        assert!(observation.history.keep("24:0", source.generation, 4.0, now + Duration::from_secs(4)).is_err());
    }
}
