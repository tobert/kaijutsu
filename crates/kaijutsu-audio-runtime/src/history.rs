//! Per-source MIDI history. Local monotonic time defines coverage; wallclock
//! stamps remain in events for interpretation, never ordering.
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};
use kaijutsu_audio::CaptureEvent;
use serde::Serialize;
use uuid::Uuid;
use crate::midi_in::ObservedPort;

#[derive(Clone, Copy, Debug)]
pub struct HistoryLimits {
    pub retention: Duration,
    pub source_bytes: usize,
    pub node_bytes: usize,
    pub max_sources: usize,
    pub message_bytes: usize,
    pub keep_bytes: usize,
}

impl Default for HistoryLimits {
    fn default() -> Self {
        Self { retention: Duration::from_secs(60), source_bytes: 1024 * 1024,
            node_bytes: 16 * 1024 * 1024, max_sources: 64,
            message_bytes: 64 * 1024, keep_bytes: 8 * 1024 * 1024 }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct SourceStatus {
    pub address: String,
    pub generation: Uuid,
    pub listening: bool,
    pub retained_events: usize,
    pub retained_bytes: usize,
    pub covered_seconds: f64,
    pub lost: u64,
}

#[derive(Debug, Serialize)]
pub struct HistoryWindow {
    pub source: String,
    pub generation: Uuid,
    pub seconds: f64,
    pub start_position: u64,
    pub end_position: u64,
    pub start_elapsed_ns: u64,
    pub end_elapsed_ns: u64,
    pub wallclock_anchor: Option<HistoryAnchor>,
    pub event_elapsed_ns: Vec<u64>,
    pub events: Vec<CaptureEvent>,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct HistoryAnchor {
    pub elapsed_ns: u64,
    pub epoch_ns: u64,
}

struct Stamped {
    at: Instant,
    position: u64,
    event: CaptureEvent,
    bytes: usize,
}

struct Source {
    facts: crate::midi_match::PortFacts,
    generation: Uuid,
    started: Instant,
    anchor: Option<HistoryAnchor>,
    listening: bool,
    coverage_start: Instant,
    last_observed: Instant,
    events: VecDeque<Stamped>,
    bytes: usize,
    head: u64,
    lost: u64,
}

pub struct HistoryStore {
    limits: HistoryLimits,
    sources: BTreeMap<String, Source>,
}

impl HistoryStore {
    pub fn new(limits: HistoryLimits) -> Result<Self, String> {
        if limits.retention.is_zero() || limits.source_bytes == 0 || limits.node_bytes == 0
            || limits.max_sources == 0 || limits.message_bytes == 0 || limits.keep_bytes == 0 {
            return Err("MIDI history limits must all be greater than zero".into());
        }
        Ok(Self { limits, sources: BTreeMap::new() })
    }

    pub fn reconcile(&mut self, ports: &[ObservedPort], now: Instant) -> Result<(), String> {
        for (address, source) in &mut self.sources {
            if !ports.iter().any(|p| p.facts.address == *address && p.listening) {
                source.listening = false;
            }
        }
        self.prune(now);
        for port in ports.iter().filter(|p| p.listening) {
            let address = &port.facts.address;
            if self.sources.get(address).is_some_and(|s| s.listening && s.facts == port.facts) { continue; }
            if !self.sources.contains_key(address) && self.sources.len() >= self.limits.max_sources {
                return Err(format!("MIDI history source limit {} reached; {} is not retained", self.limits.max_sources, address));
            }
            self.sources.insert(address.clone(), Source {
                facts: port.facts.clone(), started: now, anchor: None,
                generation: Uuid::new_v4(), listening: true, coverage_start: now,
                last_observed: now, events: VecDeque::new(), bytes: 0, head: 0, lost: 0,
            });
        }
        Ok(())
    }

    pub fn port_down(&mut self, address: &str) {
        if let Some(source) = self.sources.get_mut(address) { source.listening = false; }
    }

    /// Unknown-source ingress loss invalidates coverage on every source.
    pub fn mark_loss(&mut self, now: Instant) {
        for source in self.sources.values_mut().filter(|s| s.listening) {
            source.lost = source.lost.saturating_add(1);
            source.coverage_start = source.coverage_start.max(now + Duration::from_nanos(1));
        }
    }

    pub fn ingest(&mut self, event: CaptureEvent, at: Instant) -> Result<(), String> {
        self.prune(at);
        let node_bytes: usize = self.sources.values().map(|s| s.bytes).sum();
        let source = self.sources.get_mut(&event.source)
            .ok_or_else(|| format!("MIDI history has no observed source {}", event.source))?;
        if !source.listening { return Err("MIDI history source is not listening".into()); }
        if at < source.last_observed {
            source.lost = source.lost.saturating_add(1);
            source.coverage_start = source.coverage_start.max(source.last_observed);
            return Err("MIDI observation time moved backward; coverage invalidated".into());
        }
        source.last_observed = at;
        source.anchor = Some(HistoryAnchor {
            elapsed_ns: at.saturating_duration_since(source.started).as_nanos() as u64,
            epoch_ns: event.epoch_ns,
        });
        let bytes = std::mem::size_of::<Stamped>() + event.bytes.capacity() + event.source.capacity();
        if event.bytes.len() > self.limits.message_bytes || bytes > self.limits.source_bytes
            || bytes > self.limits.node_bytes.saturating_sub(node_bytes.saturating_sub(source.bytes)) {
            source.lost = source.lost.saturating_add(1);
            source.coverage_start = at + Duration::from_nanos(1);
            return Err("MIDI history message or node byte limit exceeded; coverage invalidated".into());
        }
        let allowed = self.limits.source_bytes.min(self.limits.node_bytes - (node_bytes - source.bytes));
        while source.bytes > allowed - bytes {
            let old = source.events.pop_front().expect("history byte accounting requires an event");
            source.bytes -= old.bytes;
            source.coverage_start = source.coverage_start.max(old.at + Duration::from_nanos(1));
        }
        let position = source.head;
        source.head = source.head.checked_add(1).ok_or("MIDI history position exhausted")?;
        source.bytes += bytes;
        source.events.push_back(Stamped { at, position, event, bytes });
        Ok(())
    }

    fn prune(&mut self, now: Instant) {
        let Some(cutoff) = now.checked_sub(self.limits.retention) else { return; };
        for source in self.sources.values_mut() {
            while source.events.front().is_some_and(|event| event.at < cutoff) {
                source.bytes -= source.events.pop_front().unwrap().bytes;
            }
            source.coverage_start = source.coverage_start.max(cutoff);
        }
        self.sources.retain(|_, s| s.listening || s.last_observed >= cutoff || !s.events.is_empty());
    }

    pub fn inventory(&mut self, now: Instant) -> Vec<SourceStatus> {
        self.prune(now);
        self.sources.iter().map(|(address, s)| SourceStatus {
            address: address.clone(), generation: s.generation, listening: s.listening,
            retained_events: s.events.len(), retained_bytes: s.bytes,
            covered_seconds: now.saturating_duration_since(s.coverage_start).as_secs_f64(), lost: s.lost,
        }).collect()
    }

    pub fn keep(&mut self, address: &str, generation: Uuid, seconds: f64, now: Instant) -> Result<HistoryWindow, String> {
        if !seconds.is_finite() || seconds <= 0.0 || seconds > self.limits.retention.as_secs_f64() {
            return Err(format!("MIDI window seconds must be greater than zero and at most {}", self.limits.retention.as_secs_f64()));
        }
        self.prune(now);
        let source = self.sources.get(address).ok_or("unknown MIDI history source")?;
        if source.generation != generation { return Err("MIDI history generation changed; inspect inventory again".into()); }
        if !source.listening { return Err("MIDI history source stopped; relative live windows are unavailable".into()); }
        let start = now.checked_sub(Duration::from_secs_f64(seconds)).ok_or("MIDI window starts before local time")?;
        if start < source.coverage_start { return Err("MIDI window has incomplete coverage: expired, not yet watched, or input loss".into()); }
        let selected: Vec<_> = source.events.iter().filter(|e| e.at >= start && e.at < now).collect();
        let bytes: usize = selected.iter().map(|e| e.bytes).sum();
        if bytes > self.limits.keep_bytes { return Err("MIDI snapshot byte limit exceeded".into()); }
        let start_position = source.events.iter().find(|e| e.at >= start).map_or(source.head, |e| e.position);
        let end_position = source.events.iter().find(|e| e.at >= now).map_or(source.head, |e| e.position);
        let event_elapsed_ns = selected.iter().map(|e| e.at.saturating_duration_since(source.started).as_nanos() as u64).collect();
        Ok(HistoryWindow { source: address.into(), generation, seconds, start_position,
            end_position, start_elapsed_ns: start.saturating_duration_since(source.started).as_nanos() as u64,
            end_elapsed_ns: now.saturating_duration_since(source.started).as_nanos() as u64,
            wallclock_anchor: source.anchor, event_elapsed_ns,
            events: selected.into_iter().map(|e| e.event.clone()).collect() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn port() -> ObservedPort {
        ObservedPort { facts: crate::midi_match::PortFacts { client_name: "JD-Xi".into(),
            port_name: "MIDI".into(), address: "24:0".into(), usb_id: None },
            readable: true, writable: true, listening: true, error: None }
    }
    #[test]
    fn idle_coverage_repeated_reads_loss_and_replug() {
        let now = Instant::now();
        let mut store = HistoryStore::new(HistoryLimits::default()).unwrap();
        store.reconcile(&[port()], now).unwrap();
        let generation = store.inventory(now)[0].generation;
        assert!(store.keep("24:0", generation, 1.0, now).is_err());
        assert!(store.keep("24:0", generation, 1.0, now + Duration::from_secs(1)).unwrap().events.is_empty());
        store.ingest(CaptureEvent { source: "24:0".into(), epoch_ns: 1, bytes: vec![0xFA] }, now + Duration::from_secs(1)).unwrap();
        for _ in 0..2 { assert_eq!(store.keep("24:0", generation, 2.0, now + Duration::from_secs(2)).unwrap().events.len(), 1); }
        store.mark_loss(now + Duration::from_secs(2));
        assert!(store.keep("24:0", generation, 2.0, now + Duration::from_secs(3)).is_err());
        store.reconcile(&[], now + Duration::from_secs(3)).unwrap();
        assert!(store.keep("24:0", generation, 1.0, now + Duration::from_secs(3)).is_err());
        store.reconcile(&[port()], now + Duration::from_secs(4)).unwrap();
        assert_ne!(store.inventory(now + Duration::from_secs(4))[0].generation, generation);
        assert!(store.keep("24:0", generation, 1.0, now + Duration::from_secs(5)).is_err());
    }

    #[test]
    fn address_reuse_with_changed_names_starts_new_generation() {
        let now = Instant::now();
        let mut store = HistoryStore::new(HistoryLimits::default()).unwrap();
        store.reconcile(&[port()], now).unwrap();
        let old = store.inventory(now)[0].generation;
        let mut replacement = port();
        replacement.facts.client_name = "KSP".into();
        store.reconcile(&[replacement], now + Duration::from_secs(1)).unwrap();
        assert_ne!(old, store.inventory(now + Duration::from_secs(1))[0].generation);
    }

    #[test]
    fn loss_at_window_start_is_not_complete_coverage() {
        let now = Instant::now();
        let mut store = HistoryStore::new(HistoryLimits::default()).unwrap();
        store.reconcile(&[port()], now).unwrap();
        let generation = store.inventory(now)[0].generation;
        store.mark_loss(now + Duration::from_secs(1));
        assert!(store.keep("24:0", generation, 1.0, now + Duration::from_secs(2)).is_err());
    }

    #[test]
    fn source_and_message_limits_and_retention_are_explicit() {
        let now = Instant::now();
        let mut store = HistoryStore::new(HistoryLimits { max_sources: 1, message_bytes: 1,
            retention: Duration::from_secs(2), ..HistoryLimits::default() }).unwrap();
        store.reconcile(&[port()], now).unwrap();
        let generation = store.inventory(now)[0].generation;
        let mut other = port();
        other.facts.address = "25:0".into();
        assert!(store.reconcile(&[port(), other], now).is_err());
        assert!(store.ingest(CaptureEvent { source: "24:0".into(), epoch_ns: 0, bytes: vec![0xF0, 0xF7] }, now + Duration::from_secs(1)).is_err());
        assert!(store.keep("24:0", generation, 1.0, now + Duration::from_secs(2)).is_err());
        store.ingest(CaptureEvent { source: "24:0".into(), epoch_ns: 0, bytes: vec![0xFA] }, now + Duration::from_secs(2)).unwrap();
        assert_eq!(store.inventory(now + Duration::from_secs(5))[0].retained_events, 0);
        assert!(store.keep("24:0", generation, 3.0, now + Duration::from_secs(5)).is_err());
    }

    #[test]
    fn node_and_snapshot_budgets_fail_without_corrupting_other_sources() {
        let now = Instant::now();
        let event_bytes = std::mem::size_of::<Stamped>() + "24:0".len() + 1;
        let mut store = HistoryStore::new(HistoryLimits { source_bytes: event_bytes,
            node_bytes: event_bytes, keep_bytes: 1, ..HistoryLimits::default() }).unwrap();
        let mut other = port();
        other.facts.address = "25:0".into();
        store.reconcile(&[port(), other], now).unwrap();
        let generation = store.inventory(now)[0].generation;
        store.ingest(CaptureEvent { source: "24:0".into(), epoch_ns: 2, bytes: vec![0xFA] }, now + Duration::from_secs(1)).unwrap();
        assert!(store.ingest(CaptureEvent { source: "25:0".into(), epoch_ns: 1, bytes: vec![0xFC] }, now + Duration::from_secs(1)).is_err());
        assert!(store.keep("24:0", generation, 2.0, now + Duration::from_secs(2)).unwrap_err().contains("snapshot byte limit"));
        let sources = store.inventory(now + Duration::from_secs(2));
        assert_eq!(sources[0].retained_events, 1);
        assert_eq!(sources[0].lost, 0);
        assert_eq!(sources[1].lost, 1);
        assert!(sources.iter().map(|s| s.retained_bytes).sum::<usize>() <= event_bytes);
    }

    #[test]
    fn byte_eviction_and_wallclock_rollback_preserve_monotonic_order() {
        let now = Instant::now();
        let event_bytes = std::mem::size_of::<Stamped>() + "24:0".len() + 1;
        let mut store = HistoryStore::new(HistoryLimits { source_bytes: event_bytes,
            ..HistoryLimits::default() }).unwrap();
        store.reconcile(&[port()], now).unwrap();
        let generation = store.inventory(now)[0].generation;
        store.ingest(CaptureEvent { source: "24:0".into(), epoch_ns: 2, bytes: vec![0xFA] }, now + Duration::from_secs(1)).unwrap();
        store.ingest(CaptureEvent { source: "24:0".into(), epoch_ns: 1, bytes: vec![0xFC] }, now + Duration::from_secs(2)).unwrap();
        assert!(store.keep("24:0", generation, 3.0, now + Duration::from_secs(3)).is_err());
        let window = store.keep("24:0", generation, 1.0, now + Duration::from_secs(3)).unwrap();
        assert_eq!(window.events[0].epoch_ns, 1);
        assert_eq!(window.event_elapsed_ns, vec![2_000_000_000]);
        assert_eq!(window.start_position, 1);
        assert_eq!(window.end_position, 2);
    }

    #[test]
    fn sampled_head_excludes_later_ingestion_even_for_empty_window() {
        let now = Instant::now();
        let mut store = HistoryStore::new(HistoryLimits::default()).unwrap();
        store.reconcile(&[port()], now).unwrap();
        let generation = store.inventory(now)[0].generation;
        store.ingest(CaptureEvent { source: "24:0".into(), epoch_ns: 1, bytes: vec![0xFA] }, now + Duration::from_secs(2)).unwrap();
        let window = store.keep("24:0", generation, 1.0, now + Duration::from_secs(1)).unwrap();
        assert!(window.events.is_empty());
        assert_eq!(window.start_position, 0);
        assert_eq!(window.end_position, 0);
    }
}
