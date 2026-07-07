//! MIDI capture — the ear's pure-data half (`docs/midi.md` M2, "the ear is
//! the sink's twin").
//!
//! A capture thread (app-side, ALSA) stamps incoming MIDI events and pushes
//! them into a [`CaptureRing`]; the app's musical timer cuts phrase-aligned
//! [`CaptureBatch`]es via per-consumer [`Tracker`]s and ships them to the
//! kernel over `commitCapture`. This module owns the ring, the trackers, the
//! window cut, and the batch record — no FFI, no clocks: the *caller* owns
//! time (the phasor cuts windows in the app; tests pass epochs directly).
//!
//! One producer, N consumers: each tracker is an independent read cursor over
//! the same ring (score batcher per phrase, analysis windows, a live spray),
//! so a slow consumer never stalls a fast one. Overwrite is loud, never
//! silent: a cut reports how many events the ring overwrote before the
//! tracker got to them ([`CaptureBatch::lost`]).

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// The capture batch MIME. JSON, not SMF (`audio/midi` implies a standard
/// MIDI file; this is a stamped event log) — per the clip-record precedent a
/// score-riding record is readable text: `serde` round-trips it, kaish `jq`s
/// it, a model can read it raw.
pub const MIDI_CAPTURE_MIME: &str = "application/vnd.kaijutsu.midi-capture+json";

/// The batch record version this build writes and accepts (per-record,
/// OTIO-style, same policy as [`crate::CLIP_VERSION`]). Breaking field
/// changes bump it; compatible growth goes in [`CaptureBatch::ext`].
pub const MIDI_CAPTURE_VERSION: u32 = 1;

/// One captured MIDI event, stamped at receipt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureEvent {
    /// Wall-clock receipt time, nanoseconds since the UNIX epoch. The kernel
    /// maps this onto the track grid at commit (thin client — the app never
    /// computes ticks), keeping the residual as micro-timing metadata.
    pub epoch_ns: u64,
    /// Source lane key, ALSA-address-shaped for ALSA sources (`"24:0"`,
    /// aconnect-readable) but free-form so non-ALSA ears (RTP-MIDI, the
    /// audio2midi mic) ride the same record. Becomes `played_by` at commit.
    pub source: String,
    /// Raw MIDI message bytes (status + data; sysex allowed, realtime spam
    /// filtered at ingest — see [`keep_at_ingest`]).
    pub bytes: Vec<u8>,
}

/// Ingest filter: should this MIDI message enter the ring at all?
///
/// Drops the high-rate realtime spam that would flood every batch — `F8`
/// timing clock (24 PPQN — the M3 drift model observes pulses *locally* at
/// the capture edge, they never ride batches), `F9` tick, `FE` active
/// sensing. Keeps transport intent (`FA` start / `FB` continue / `FC` stop —
/// musically meaningful) and everything with a note in it. Empty messages
/// are dropped (nothing to keep).
pub fn keep_at_ingest(bytes: &[u8]) -> bool {
    match bytes.first() {
        None => false,
        Some(0xF8) | Some(0xF9) | Some(0xFE) => false,
        Some(_) => true,
    }
}

/// A window of captured events cut from the ring — the record that rides a
/// `commitCapture` cell (mime [`MIDI_CAPTURE_MIME`]).
///
/// `Debug` is hand-written to print event/byte *counts*, never the event
/// list — same lesson as `RenderCue`: a stray `debug!(?batch)` on a dense
/// phrase must not dump hundreds of events into a log line.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureBatch {
    /// Record version. Must equal [`MIDI_CAPTURE_VERSION`].
    pub v: u32,
    /// Window start, epoch ns (half-open `[start, end)`); chains from the
    /// previous cut on the same tracker.
    pub window_start_ns: u64,
    /// Window end, epoch ns (exclusive).
    pub window_end_ns: u64,
    /// The events, in capture (sequence) order.
    pub events: Vec<CaptureEvent>,
    /// Events the ring overwrote before this tracker consumed them. `0` in
    /// healthy operation; nonzero means the ring is undersized or the cutter
    /// stalled — surface it, never swallow it.
    #[serde(default)]
    pub lost: u64,
    /// Extension bag: unknown keys survive round-trips (the OTIO lesson).
    #[serde(default)]
    pub ext: serde_json::Map<String, serde_json::Value>,
}

impl std::fmt::Debug for CaptureBatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let bytes: usize = self.events.iter().map(|e| e.bytes.len()).sum();
        f.debug_struct("CaptureBatch")
            .field("v", &self.v)
            .field("window_start_ns", &self.window_start_ns)
            .field("window_end_ns", &self.window_end_ns)
            .field("events", &format_args!("[{} events, {} midi bytes]", self.events.len(), bytes))
            .field("lost", &self.lost)
            .finish()
    }
}

/// A capture batch failed to parse or validate. Fail-loud, same stance as
/// `ClipError`: a bad batch is an error at commit time, never silently
/// dropped telemetry.
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// Not valid JSON, or the wrong shape.
    #[error("capture batch is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// `v` names a version this build does not understand.
    #[error("unknown capture batch version {0} (this build supports v{MIDI_CAPTURE_VERSION})")]
    UnknownVersion(u32),
    /// Window bounds are inverted.
    #[error("capture window is inverted: start {start} > end {end}")]
    InvertedWindow { start: u64, end: u64 },
}

impl CaptureBatch {
    /// Serialize for the wire / the cell payload.
    pub fn to_json_bytes(&self) -> Result<Vec<u8>, CaptureError> {
        Ok(serde_json::to_vec(self)?)
    }

    /// Parse + validate a batch record (the kernel-side entry point).
    pub fn parse(bytes: &[u8]) -> Result<Self, CaptureError> {
        let batch: CaptureBatch = serde_json::from_slice(bytes)?;
        if batch.v != MIDI_CAPTURE_VERSION {
            return Err(CaptureError::UnknownVersion(batch.v));
        }
        if batch.window_start_ns > batch.window_end_ns {
            return Err(CaptureError::InvertedWindow {
                start: batch.window_start_ns,
                end: batch.window_end_ns,
            });
        }
        Ok(batch)
    }

    /// True when the window held no events (callers usually skip shipping
    /// these; the window chain stays continuous either way).
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// An independent read cursor over a [`CaptureRing`]. Create one per
/// consumer via [`CaptureRing::tracker_at`]; pass it back to
/// [`CaptureRing::cut`] to take the next window.
#[derive(Debug, Clone)]
pub struct Tracker {
    /// Absolute sequence number of the next unconsumed event.
    cursor: u64,
    /// Start of the next window (the previous cut's `until`, or the creation
    /// epoch for a fresh tracker).
    window_start_ns: u64,
}

/// Fixed-capacity ring of captured events. Push-side is the capture thread's
/// drain; cut-side is each consumer's tracker. Overwrites oldest when full
/// and *counts* what each tracker missed (loud, per-tracker `lost`).
#[derive(Debug)]
pub struct CaptureRing {
    buf: VecDeque<CaptureEvent>,
    capacity: usize,
    /// Total events ever pushed; sequence number of the next push. The
    /// oldest event still buffered has sequence `pushed - buf.len()`.
    pushed: u64,
}

impl CaptureRing {
    /// `capacity` is in events. At jam density (a few hundred events per
    /// phrase) a few thousand covers minutes; MIDI is tiny, size generously.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "a zero-capacity ring can only lose events");
        Self {
            buf: VecDeque::with_capacity(capacity),
            capacity,
            pushed: 0,
        }
    }

    /// Append one stamped event, overwriting the oldest when full. Events
    /// are expected in receipt order (one capture thread stamps at receipt);
    /// sequence order is authoritative, `epoch_ns` is assumed monotone
    /// modulo clock adjustments.
    pub fn push(&mut self, ev: CaptureEvent) {
        if self.buf.len() == self.capacity {
            self.buf.pop_front();
        }
        self.buf.push_back(ev);
        self.pushed += 1;
    }

    /// A new consumer cursor starting at the current head (it will see only
    /// events pushed after this call), with its first window opening at
    /// `now_ns`.
    pub fn tracker_at(&self, now_ns: u64) -> Tracker {
        Tracker {
            cursor: self.pushed,
            window_start_ns: now_ns,
        }
    }

    /// Cut the tracker's next window: every unconsumed event with
    /// `epoch_ns < until_ns`, in sequence order. Advances the tracker's
    /// cursor and window chain; reports (and skips past) anything the ring
    /// overwrote before the tracker got to it.
    ///
    /// The scan stops at the first event at-or-after `until_ns`: sequence
    /// order is authoritative, so an out-of-order stamp (clock step) defers
    /// to the next cut rather than reordering the log.
    pub fn cut(&self, tracker: &mut Tracker, until_ns: u64) -> CaptureBatch {
        let oldest = self.pushed - self.buf.len() as u64;
        let lost = oldest.saturating_sub(tracker.cursor);
        tracker.cursor = tracker.cursor.max(oldest);

        let mut events = Vec::new();
        while let Some(ev) = self.buf.get((tracker.cursor - oldest) as usize) {
            if ev.epoch_ns >= until_ns {
                break;
            }
            events.push(ev.clone());
            tracker.cursor += 1;
        }

        let batch = CaptureBatch {
            v: MIDI_CAPTURE_VERSION,
            window_start_ns: tracker.window_start_ns,
            window_end_ns: until_ns,
            events,
            lost,
            ext: serde_json::Map::new(),
        };
        tracker.window_start_ns = until_ns;
        batch
    }

    /// Events currently buffered (bounded by capacity).
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// True when nothing is buffered.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Total events ever pushed (monotonic; drop math for observability).
    pub fn total_pushed(&self) -> u64 {
        self.pushed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(epoch_ns: u64, bytes: &[u8]) -> CaptureEvent {
        CaptureEvent {
            epoch_ns,
            source: "24:0".into(),
            bytes: bytes.to_vec(),
        }
    }

    #[test]
    fn ingest_filter_drops_realtime_spam_keeps_transport_and_notes() {
        // Dropped: clock, tick, active sensing, empty.
        assert!(!keep_at_ingest(&[0xF8]));
        assert!(!keep_at_ingest(&[0xF9]));
        assert!(!keep_at_ingest(&[0xFE]));
        assert!(!keep_at_ingest(&[]));
        // Kept: transport intent is musically meaningful.
        assert!(keep_at_ingest(&[0xFA]));
        assert!(keep_at_ingest(&[0xFB]));
        assert!(keep_at_ingest(&[0xFC]));
        // Kept: notes, CC, sysex.
        assert!(keep_at_ingest(&[0x90, 60, 100]));
        assert!(keep_at_ingest(&[0xB0, 1, 64]));
        assert!(keep_at_ingest(&[0xF0, 0x7E, 0xF7]));
    }

    #[test]
    fn cut_takes_events_before_until_and_chains_windows() {
        let mut ring = CaptureRing::new(16);
        let mut t = ring.tracker_at(5);
        ring.push(ev(10, &[0x90, 60, 100]));
        ring.push(ev(20, &[0x80, 60, 0]));
        ring.push(ev(30, &[0x90, 62, 90]));

        let first = ring.cut(&mut t, 25);
        assert_eq!(first.window_start_ns, 5, "first window opens at tracker birth");
        assert_eq!(first.window_end_ns, 25);
        assert_eq!(first.events.len(), 2);
        assert_eq!(first.lost, 0);

        let second = ring.cut(&mut t, 100);
        assert_eq!(second.window_start_ns, 25, "windows chain half-open");
        assert_eq!(second.events.len(), 1);
        assert_eq!(second.events[0].epoch_ns, 30);

        let third = ring.cut(&mut t, 200);
        assert!(third.is_empty(), "nothing new -> empty batch, window still advances");
        assert_eq!(third.window_start_ns, 100);
    }

    #[test]
    fn trackers_are_independent_cursors() {
        let mut ring = CaptureRing::new(16);
        let mut a = ring.tracker_at(0);
        let mut b = ring.tracker_at(0);
        ring.push(ev(10, &[0x90, 60, 100]));
        ring.push(ev(20, &[0x80, 60, 0]));

        assert_eq!(ring.cut(&mut a, 100).events.len(), 2);
        // a consumed everything; b still sees it all.
        assert_eq!(ring.cut(&mut b, 100).events.len(), 2);
        // and both are drained now.
        assert!(ring.cut(&mut a, 200).is_empty());
        assert!(ring.cut(&mut b, 200).is_empty());
    }

    #[test]
    fn overwrite_is_counted_loudly_then_recovers() {
        let mut ring = CaptureRing::new(4);
        let mut t = ring.tracker_at(0);
        for i in 0..6 {
            ring.push(ev(10 * (i + 1), &[0x90, 60 + i as u8, 100]));
        }
        // Capacity 4, pushed 6: the tracker missed the first 2.
        let batch = ring.cut(&mut t, 1_000);
        assert_eq!(batch.lost, 2, "overwritten events are counted, not swallowed");
        assert_eq!(batch.events.len(), 4);
        assert_eq!(batch.events[0].epoch_ns, 30, "oldest surviving event");

        // Caught up: the next cut is clean.
        ring.push(ev(100, &[0x90, 70, 100]));
        let clean = ring.cut(&mut t, 1_000);
        assert_eq!(clean.lost, 0);
        assert_eq!(clean.events.len(), 1);
    }

    #[test]
    fn late_stamp_defers_to_next_cut_never_reorders() {
        let mut ring = CaptureRing::new(16);
        let mut t = ring.tracker_at(0);
        ring.push(ev(10, &[0x90, 60, 100]));
        ring.push(ev(50, &[0x90, 61, 100])); // stamped ahead (clock step)
        ring.push(ev(20, &[0x90, 62, 100])); // behind its predecessor

        let batch = ring.cut(&mut t, 30);
        // Scan stops at the 50-stamp; the 20-stamp behind it waits (sequence
        // order is authoritative).
        assert_eq!(batch.events.len(), 1);
        let rest = ring.cut(&mut t, 100);
        assert_eq!(rest.events.len(), 2);
        assert_eq!(rest.events[0].epoch_ns, 50);
        assert_eq!(rest.events[1].epoch_ns, 20);
    }

    #[test]
    fn batch_json_round_trips_and_version_gates() {
        let mut ring = CaptureRing::new(4);
        let mut t = ring.tracker_at(0);
        ring.push(ev(10, &[0x90, 60, 100]));
        let batch = ring.cut(&mut t, 100);

        let bytes = batch.to_json_bytes().expect("serialize");
        let back = CaptureBatch::parse(&bytes).expect("parse");
        assert_eq!(back, batch);

        // Unknown version is a loud error, not a guess.
        let mut wrong = batch.clone();
        wrong.v = 99;
        let bytes = wrong.to_json_bytes().expect("serialize");
        assert!(matches!(
            CaptureBatch::parse(&bytes),
            Err(CaptureError::UnknownVersion(99))
        ));

        // Inverted window is rejected at parse.
        let mut inverted = batch.clone();
        inverted.window_start_ns = 200;
        inverted.window_end_ns = 100;
        let bytes = inverted.to_json_bytes().expect("serialize");
        assert!(matches!(
            CaptureBatch::parse(&bytes),
            Err(CaptureError::InvertedWindow { .. })
        ));
    }

    #[test]
    fn batch_ext_bag_survives_round_trip() {
        let json = format!(
            r#"{{"v":{MIDI_CAPTURE_VERSION},"window_start_ns":0,"window_end_ns":10,
                "events":[],"future_field":{{"x":1}}}}"#
        );
        // Unknown top-level keys outside ext are dropped by serde (no
        // deny_unknown_fields — forward-readable); keys *inside* ext survive.
        let batch = CaptureBatch::parse(json.as_bytes()).expect("parse");
        assert!(batch.is_empty());

        let mut with_ext = batch.clone();
        with_ext
            .ext
            .insert("groove".into(), serde_json::json!({"swing": 0.12}));
        let bytes = with_ext.to_json_bytes().expect("serialize");
        let back = CaptureBatch::parse(&bytes).expect("parse");
        assert_eq!(back.ext["groove"]["swing"], serde_json::json!(0.12));
    }

    #[test]
    fn debug_elides_the_event_list() {
        let mut ring = CaptureRing::new(8);
        let mut t = ring.tracker_at(0);
        for i in 0..5 {
            ring.push(ev(i + 1, &[0x90, 60, 100]));
        }
        let batch = ring.cut(&mut t, 100);
        let s = format!("{batch:?}");
        assert!(s.contains("[5 events, 15 midi bytes]"), "counts, not contents: {s}");
        assert!(!s.contains("144"), "no raw byte dump: {s}");
    }
}
