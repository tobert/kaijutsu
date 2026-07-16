//! App-owned rodio scheduler thread (`docs/pcm.md` R5, "the app owns rodio
//! outright" — decided 2026-07-16): Bevy's `AudioPlayer` has no scheduling
//! primitive and no source-range/gain control, so `bevy_audio` leaves the
//! sink entirely (`main.rs` disables `bevy::audio::AudioPlugin`). This module
//! is the ONE path for ALL sample playback from here on — play-now and
//! scheduled/trimmed/gained are the same mechanism, never two.
//!
//! rodio 0.20's `OutputStream` (and the `cpal::Stream` it wraps) is `!Send`,
//! so it is built and lives entirely on ONE dedicated `std::thread` —
//! [`run`] — and never touches Bevy's main thread. Bevy systems in
//! `audio.rs` never see rodio types at all: they compute a deadline (via
//! [`effective_deadline`]) and send a [`SchedulerCmd`] over a crossbeam
//! channel; everything rodio-shaped lives from [`spawn`] down.
//!
//! [`backdated_lead`]/[`effective_deadline`] mirror `midi.rs::backdate_events`'s
//! epoch-backdating discipline (`docs/midi.md` "The one timebase") collapsed
//! to a single go/no-go/when decision: a scheduled *sound* (unlike a phrase
//! of MIDI events) has no event list to partially drop — it either fires
//! now, fires later, or the whole cue is rejected as too stale to trust.
//!
//! Testability (the house TDD rule): only [`OutputStream::try_default`] and
//! the eventual `Sink::append` genuinely need a live audio device — everything
//! else (the backdating ladder, the deadline-ordered pending queue, decoding
//! + trim + gain) is plain data and pure functions, exercised without one.
//! [`build_source`] and [`handle_cmd`] both work fine with `output: None`
//! (the graceful no-device path this module already needs for a headless
//! box), so unit tests drive them directly; only the literal thread-plus-device
//! smoke test at the bottom is `#[ignore]`d.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::io::Cursor;
use std::time::{Duration, Instant};

use bevy::prelude::Resource;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, unbounded};
use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink, Source, decoder::DecoderError};
use tracing::warn;

use kaijutsu_audio::{REF_STALE_MAX, stamp_age};

/// One command the scheduler thread understands. Bevy systems never
/// construct these directly (see [`AudioSchedulerHandle`]'s convenience
/// methods) except in this crate's own tests, where asserting on a received
/// `SchedulerCmd` is how the dispatch logic in `audio.rs` gets checked
/// without a real rodio thread.
#[derive(Debug)]
pub(crate) enum SchedulerCmd {
    /// Decode + append right now — the play-now fast path, byte-for-byte
    /// parity with the pre-rodio `AudioPlayer` spawn (no trim, no gain, no
    /// heap round trip).
    PlayNow { bytes: Vec<u8> },
    /// Decode ahead of time, trim/gain, and fire at `deadline`. A `deadline`
    /// at or before "now" when the scheduler gets to it fires effectively
    /// immediately (see [`drain_ready`]/[`handle_cmd`]) — this is also how a
    /// CAS-resolved cue whose original deadline already passed during the
    /// fetch plays "as soon as possible" rather than re-deriving a fresh
    /// (wrong) deadline from resolve time (`docs/pcm.md` Decision 4).
    PlayAt {
        bytes: Vec<u8>,
        deadline: Instant,
        src_offset: Option<Duration>,
        src_len: Option<Duration>,
        gain_db: f64,
    },
    /// Transport stop/pause (`RENDER_FLUSH_MIME`): drop every pending sound
    /// and silence every sounding one.
    Flush,
}

/// The Bevy-side handle: a cheap `Sender` clone. Systems call
/// [`Self::play_now`]/[`Self::play_at`]/[`Self::flush`] rather than building
/// a [`SchedulerCmd`] themselves, so the wire shape stays an implementation
/// detail this module can change freely.
#[derive(Resource)]
pub(crate) struct AudioSchedulerHandle {
    tx: Sender<SchedulerCmd>,
}

impl AudioSchedulerHandle {
    /// Play-now parity: decode + append immediately, no trim, no gain.
    pub(crate) fn play_now(&self, bytes: Vec<u8>) {
        // A closed receiver only means the scheduler thread is gone (app
        // shutting down) — never a reason to panic a render-cue frame.
        let _ = self.tx.send(SchedulerCmd::PlayNow { bytes });
    }

    /// Schedule playback at `deadline`, with optional source-range trim and
    /// gain baked in (`docs/pcm.md` clip record fields).
    pub(crate) fn play_at(
        &self,
        bytes: Vec<u8>,
        deadline: Instant,
        src_offset: Option<Duration>,
        src_len: Option<Duration>,
        gain_db: f64,
    ) {
        let _ = self.tx.send(SchedulerCmd::PlayAt {
            bytes,
            deadline,
            src_offset,
            src_len,
            gain_db,
        });
    }

    /// Transport stop/pause: drop every pending sound, silence every
    /// sounding one.
    pub(crate) fn flush(&self) {
        let _ = self.tx.send(SchedulerCmd::Flush);
    }
}

/// Spawn the scheduler thread and return the Bevy-side handle. Building the
/// `OutputStream` happens ON the new thread (rodio 0.20's `OutputStream` is
/// `!Send` — it cannot be built here and handed over). A missing/broken audio
/// device degrades gracefully: [`run`] warns once and keeps draining commands
/// so a headless box neither wedges nor crashes the app, matching
/// `midi.rs`'s no-ALSA posture.
pub(crate) fn spawn() -> AudioSchedulerHandle {
    let (tx, rx) = unbounded();
    std::thread::Builder::new()
        .name("kaijutsu-audio-sched".into())
        .spawn(move || run(rx))
        .expect("spawn kaijutsu-audio-sched thread");
    AudioSchedulerHandle { tx }
}

// ── Backdating (mirrors midi.rs::backdate_events, collapsed to one sound) ──

/// Re-anchor `lead` against how old the cue actually is on receipt — the
/// same ladder as `midi.rs::backdate_events`, collapsed to a single
/// go/no-go/when decision (a scheduled sound has no event list to partially
/// drop, unlike a phrase of MIDI events):
///
/// - `epoch_ns == 0` (unstamped): `lead` at face value — `Some(lead)`.
/// - otherwise: `deficit = age.saturating_sub(lead)`; `deficit >
///   REF_STALE_MAX` rejects the WHOLE cue (`None`) rather than firing
///   arbitrarily late; otherwise `lead' = lead.saturating_sub(age)` — the
///   lead absorbs whatever age it can and clamps at zero (play now) once
///   fully spent.
pub(crate) fn backdated_lead(lead: Duration, epoch_ns: u64, now_epoch_ns: u64) -> Option<Duration> {
    let Some(age) = stamp_age(epoch_ns, now_epoch_ns) else {
        return Some(lead); // unstamped: old behavior verbatim
    };
    let deficit = age.saturating_sub(lead);
    if deficit > REF_STALE_MAX {
        return None; // way too stale to trust even partially
    }
    Some(lead.saturating_sub(age))
}

/// [`backdated_lead`] anchored into the caller's `Instant` domain — what a
/// Bevy system sends as `SchedulerCmd::PlayAt::deadline`. `now` is passed in
/// (never read internally), so this stays pure and unit-testable.
pub(crate) fn effective_deadline(
    now: Instant,
    lead: Duration,
    epoch_ns: u64,
    now_epoch_ns: u64,
) -> Option<Instant> {
    backdated_lead(lead, epoch_ns, now_epoch_ns).map(|lead| now + lead)
}

/// dB → linear amplitude multiplier (`10^(dB/20)`; `0.0` dB == unity gain).
pub(crate) fn db_to_linear(gain_db: f64) -> f64 {
    10f64.powf(gain_db / 20.0)
}

// ── R4: skip-loud on a late CAS-resolved fire (docs/pcm.md, "closes R5's
// interim behavior") ────────────────────────────────────────────────────────

/// How late a *musically-placed* cue's media may land past its already-
/// backdated deadline before the fire is dropped loud rather than played
/// (`docs/pcm.md` R4, "Late-fetch policy is skip-loud"). Small on purpose —
/// this only absorbs the scheduler thread's own wakeup slop right at the
/// deadline (`run`'s `recv_timeout`-as-sleep granularity), never a real fetch
/// delay: the fetch itself is bounded separately and generously by
/// `audio.rs::FETCH_TIMEOUT` (R4 step 5), a wholly different budget. 100ms is
/// comfortably above scheduler jitter and comfortably below "a listener
/// notices this one-shot played meaningfully late."
pub(crate) const GRACE: Duration = Duration::from_millis(100);

/// What to do once a scheduled sound's bytes are ready to fire, given how far
/// `now` sits past `deadline`. Pure — no clock read, no I/O — so it's
/// unit-testable with hand-picked `Instant`s; the one call site is
/// `audio.rs::drain_prefetch_results`, right before it would otherwise call
/// `scheduler.play_at`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeadlineDecision {
    /// Play it — on time, or unstamped (no musical placement to violate).
    Fire,
    /// Too late for a *stamped* cue — drop instead of firing arbitrarily
    /// late. Carries how late, for the log line.
    DropLate { late_by: Duration },
}

/// `stamped` is whether the cue that produced this deadline carried a
/// non-zero `RenderCue::epoch_ns` — the musical crossing always stamps;
/// `kj play`'s play-now/`--cas` cues do not (asap semantics, and there is no
/// musical placement to be "late" against). An unstamped cue always fires,
/// however late — that preserves `kj play --cas`'s existing behavior
/// (`docs/pcm.md` R5's interim note, now closed for the stamped case only).
/// A stamped cue fires within `GRACE` of its deadline and drops loud beyond
/// it.
pub(crate) fn decide_deadline(deadline: Instant, stamped: bool, now: Instant) -> DeadlineDecision {
    if !stamped {
        return DeadlineDecision::Fire;
    }
    let late_by = now.saturating_duration_since(deadline);
    if late_by > GRACE {
        DeadlineDecision::DropLate { late_by }
    } else {
        DeadlineDecision::Fire
    }
}

// ── The pending queue: deadline-ordered, payload-agnostic ──────────────────

/// One pending scheduled sound, min-ordered by `deadline` (earliest first)
/// atop std's max-heap `BinaryHeap`, with a monotonic `seq` tie-break so
/// same-instant entries fire in arrival order. Generic over the payload so
/// the ordering/drain logic is unit-testable with a dummy payload — no rodio
/// `Source`, no audio device, anywhere near those tests.
struct Scheduled<T> {
    deadline: Instant,
    seq: u64,
    payload: T,
}

impl<T> PartialEq for Scheduled<T> {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline && self.seq == other.seq
    }
}
impl<T> Eq for Scheduled<T> {}
impl<T> PartialOrd for Scheduled<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl<T> Ord for Scheduled<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reversed: BinaryHeap is a max-heap, so flipping the comparison
        // (deadline, then the seq tie-break) makes `.peek()`/`.pop()` return
        // the EARLIEST-arriving entry instead of the latest.
        other
            .deadline
            .cmp(&self.deadline)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

/// Pop every entry whose deadline has arrived (`<= now`), earliest first,
/// leaving the rest in `heap`. Pure — no clock read, no I/O — so it's
/// unit-testable with a dummy payload and hand-picked `Instant`s.
fn drain_ready<T>(heap: &mut BinaryHeap<Scheduled<T>>, now: Instant) -> Vec<T> {
    let mut ready = Vec::new();
    while matches!(heap.peek(), Some(top) if top.deadline <= now) {
        ready.push(heap.pop().expect("just peeked Some").payload);
    }
    ready
}

// ── Decode + trim + gain (needs no audio device — just bytes) ─────────────

type BoxedSource = Box<dyn Source<Item = i16> + Send>;

/// Decode `bytes`, apply the clip source-range + gain, and box it — the
/// "decode ahead of the deadline" step (`docs/pcm.md` R5): this runs off the
/// scheduler thread's hot loop as soon as a `PlayAt` command arrives, so the
/// eventual `sink.append` at the deadline is just a queue push, never a
/// decode. Needs no audio device: `Decoder::new` only parses bytes.
fn build_source(
    bytes: Vec<u8>,
    src_offset: Option<Duration>,
    src_len: Option<Duration>,
    gain_db: f64,
) -> Result<BoxedSource, DecoderError> {
    let decoder = Decoder::new(Cursor::new(bytes))?;
    let gain = db_to_linear(gain_db) as f32;
    let source: BoxedSource = match (src_offset, src_len) {
        (None, None) => Box::new(decoder.amplify(gain)),
        (Some(off), None) => Box::new(decoder.skip_duration(off).amplify(gain)),
        (None, Some(len)) => Box::new(decoder.take_duration(len).amplify(gain)),
        (Some(off), Some(len)) => {
            Box::new(decoder.skip_duration(off).take_duration(len).amplify(gain))
        }
    };
    Ok(source)
}

// ── The thread body ─────────────────────────────────────────────────────

/// Build (or fail to build) an output stream, append `source` through a
/// fresh `Sink`, and track it in `live` for [`handle_cmd`]'s `Flush` arm to
/// stop later. `output: None` (no device) degrades silently past this point —
/// warned once at [`run`]'s startup, not per-cue.
fn fire(
    source: BoxedSource,
    output: Option<&(OutputStream, OutputStreamHandle)>,
    live: &mut Vec<Sink>,
) {
    let Some((_, handle)) = output else {
        return;
    };
    match Sink::try_new(handle) {
        Ok(sink) => {
            sink.append(source);
            live.push(sink);
        }
        Err(e) => warn!("kaijutsu-audio-sched: sink creation failed: {e}"),
    }
}

/// Apply one command against the scheduler's state. Pure enough to unit-test
/// with `output: None` — the decode/heap/flush bookkeeping never touches
/// rodio's device-dependent bits unless `output` is `Some`.
fn handle_cmd(
    cmd: SchedulerCmd,
    output: Option<&(OutputStream, OutputStreamHandle)>,
    pending: &mut BinaryHeap<Scheduled<BoxedSource>>,
    live: &mut Vec<Sink>,
    next_seq: &mut u64,
) {
    match cmd {
        SchedulerCmd::PlayNow { bytes } => match build_source(bytes, None, None, 0.0) {
            Ok(source) => fire(source, output, live),
            Err(e) => warn!("kaijutsu-audio-sched: play-now decode failed, dropping the cue: {e}"),
        },
        SchedulerCmd::PlayAt {
            bytes,
            deadline,
            src_offset,
            src_len,
            gain_db,
        } => {
            match build_source(bytes, src_offset, src_len, gain_db) {
                Ok(source) => {
                    if deadline <= Instant::now() {
                        // Already due (a zero backdated lead, or a CAS fetch
                        // that ran past its original deadline) — fire now
                        // rather than round-trip the heap for nothing.
                        fire(source, output, live);
                    } else {
                        *next_seq += 1;
                        pending.push(Scheduled {
                            deadline,
                            seq: *next_seq,
                            payload: source,
                        });
                    }
                }
                Err(e) => {
                    warn!("kaijutsu-audio-sched: scheduled decode failed, dropping the cue: {e}")
                }
            }
        }
        SchedulerCmd::Flush => {
            pending.clear();
            for sink in live.drain(..) {
                sink.stop();
            }
        }
    }
}

/// The scheduler thread's whole life. Builds the output stream once
/// (degrading to a device-less "drain but never sound" mode on failure),
/// then loops: sleep until the next command OR the earliest pending
/// deadline, whichever comes first. `rx.recv_timeout` IS the
/// sleep-until-deadline wait — a fresh command wakes it early with no
/// separate unpark to wire up, and a timeout means it's time to fire
/// whatever's ready.
fn run(rx: Receiver<SchedulerCmd>) {
    let output = match OutputStream::try_default() {
        Ok(pair) => Some(pair),
        Err(e) => {
            warn!(
                "kaijutsu-audio-sched: no audio output device ({e}); render cues will be drained \
                 silently rather than wedging the app"
            );
            None
        }
    };
    let mut pending: BinaryHeap<Scheduled<BoxedSource>> = BinaryHeap::new();
    let mut live: Vec<Sink> = Vec::new();
    let mut next_seq: u64 = 0;

    loop {
        // Opportunistic pruning: cheap, and keeps `live` from growing
        // unbounded across a long session.
        live.retain(|s| !s.empty());

        let cmd = match pending.peek() {
            None => match rx.recv() {
                Ok(cmd) => cmd,
                Err(_) => return, // every Sender dropped — app is shutting down
            },
            Some(top) => {
                let wait = top.deadline.saturating_duration_since(Instant::now());
                match rx.recv_timeout(wait) {
                    Ok(cmd) => cmd,
                    Err(RecvTimeoutError::Timeout) => {
                        for source in drain_ready(&mut pending, Instant::now()) {
                            fire(source, output.as_ref(), &mut live);
                        }
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        };
        handle_cmd(cmd, output.as_ref(), &mut pending, &mut live, &mut next_seq);
    }
}

// ── Test-only channel access (audio.rs's tests assert on sent commands) ───

#[cfg(test)]
pub(crate) fn test_handle() -> (AudioSchedulerHandle, Receiver<SchedulerCmd>) {
    let (tx, rx) = unbounded();
    (AudioSchedulerHandle { tx }, rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── backdated_lead: mirrors midi.rs::backdate_events's own suite ──────

    /// Stamp `epoch_ns` as `age` old, in the u64-ns wallclock domain.
    fn aged(epoch_ns: u64, age: Duration) -> u64 {
        epoch_ns + age.as_nanos() as u64
    }

    const EPOCH: u64 = 1_000_000_000; // arbitrary non-zero "emission instant"

    #[test]
    fn fresh_cue_shrinks_lead_by_its_age() {
        let lead = Duration::from_millis(300);
        let now_epoch_ns = aged(EPOCH, Duration::from_millis(120));
        assert_eq!(
            backdated_lead(lead, EPOCH, now_epoch_ns),
            Some(Duration::from_millis(180))
        );
    }

    #[test]
    fn age_equal_to_lead_is_the_fresh_boundary_not_the_late_one() {
        let lead = Duration::from_millis(200);
        let now_epoch_ns = aged(EPOCH, Duration::from_millis(200));
        assert_eq!(
            backdated_lead(lead, EPOCH, now_epoch_ns),
            Some(Duration::ZERO),
            "lead fully consumed, but not via the late/reject branch"
        );
    }

    #[test]
    fn late_cue_clamps_lead_to_zero_rather_than_going_negative() {
        // age=350ms, lead=100ms -> deficit=250ms, well within REF_STALE_MAX.
        let lead = Duration::from_millis(100);
        let now_epoch_ns = aged(EPOCH, Duration::from_millis(350));
        assert_eq!(
            backdated_lead(lead, EPOCH, now_epoch_ns),
            Some(Duration::ZERO),
            "no lead left to spend once behind — play now, never negative"
        );
    }

    #[test]
    fn a_cue_stale_beyond_the_max_is_rejected_outright() {
        let lead = Duration::from_millis(50);
        let now_epoch_ns = aged(EPOCH, REF_STALE_MAX + Duration::from_millis(1) + lead);
        assert_eq!(
            backdated_lead(lead, EPOCH, now_epoch_ns),
            None,
            "a cue this stale is rejected whole, not fired arbitrarily late"
        );
    }

    #[test]
    fn a_cue_exactly_at_the_stale_boundary_is_still_accepted() {
        let lead = Duration::from_millis(50);
        let now_epoch_ns = aged(EPOCH, REF_STALE_MAX + lead); // deficit == REF_STALE_MAX exactly
        assert!(
            backdated_lead(lead, EPOCH, now_epoch_ns).is_some(),
            "exactly REF_STALE_MAX deficit is still accepted (boundary is inclusive)"
        );
    }

    #[test]
    fn an_unstamped_cue_passes_lead_through_verbatim() {
        let lead = Duration::from_millis(300);
        assert_eq!(
            backdated_lead(lead, 0, 999_999_999),
            Some(lead),
            "epoch_ns == 0 means unstamped — no age to backdate against"
        );
    }

    // ── effective_deadline ────────────────────────────────────────────────

    #[test]
    fn effective_deadline_anchors_the_backdated_lead_onto_now() {
        let now = Instant::now();
        let lead = Duration::from_millis(300);
        let now_epoch_ns = aged(EPOCH, Duration::from_millis(100));
        let deadline = effective_deadline(now, lead, EPOCH, now_epoch_ns).expect("not stale");
        assert_eq!(deadline, now + Duration::from_millis(200));
    }

    #[test]
    fn effective_deadline_is_none_when_the_cue_is_too_stale() {
        let now = Instant::now();
        let lead = Duration::from_millis(50);
        let now_epoch_ns = aged(EPOCH, REF_STALE_MAX + Duration::from_secs(1) + lead);
        assert!(effective_deadline(now, lead, EPOCH, now_epoch_ns).is_none());
    }

    // ── decide_deadline: R4's skip-loud gate ───────────────────────────────

    #[test]
    fn a_stamped_cue_past_grace_drops_late() {
        let deadline = Instant::now();
        let now = deadline + GRACE + Duration::from_millis(1);
        assert_eq!(
            decide_deadline(deadline, true, now),
            DeadlineDecision::DropLate {
                late_by: GRACE + Duration::from_millis(1)
            },
            "a musically-placed cue more than GRACE late must drop, not fire"
        );
    }

    #[test]
    fn a_stamped_cue_on_time_fires() {
        let deadline = Instant::now();
        let now = deadline; // exactly on time
        assert_eq!(
            decide_deadline(deadline, true, now),
            DeadlineDecision::Fire,
            "on time (or early) always fires"
        );
    }

    #[test]
    fn a_stamped_cue_exactly_at_the_grace_boundary_still_fires() {
        let deadline = Instant::now();
        let now = deadline + GRACE; // exactly GRACE late — inclusive boundary
        assert_eq!(
            decide_deadline(deadline, true, now),
            DeadlineDecision::Fire,
            "exactly GRACE late is still accepted (boundary is inclusive)"
        );
    }

    #[test]
    fn an_unstamped_cue_always_fires_however_late() {
        // asap semantics (`kj play --cas`) — no musical placement to violate,
        // so lateness (even wildly past GRACE) never drops it.
        let deadline = Instant::now();
        let now = deadline + GRACE * 100;
        assert_eq!(
            decide_deadline(deadline, false, now),
            DeadlineDecision::Fire,
            "unstamped cues fire regardless of how late — asap semantics preserved"
        );
    }

    // ── db_to_linear ─────────────────────────────────────────────────────

    #[test]
    fn db_to_linear_is_unity_at_zero_db() {
        assert_eq!(db_to_linear(0.0), 1.0);
    }

    #[test]
    fn db_to_linear_matches_familiar_reference_points() {
        assert!(
            (db_to_linear(20.0) - 10.0).abs() < 1e-9,
            "20 dB is a 10x multiplier"
        );
        assert!(
            (db_to_linear(-20.0) - 0.1).abs() < 1e-9,
            "-20 dB is a 0.1x multiplier"
        );
    }

    // ── Scheduled/drain_ready: deadline-ordered, payload-agnostic ─────────

    #[test]
    fn drain_ready_pops_entries_at_or_before_now_in_deadline_order() {
        let base = Instant::now();
        let mut heap = BinaryHeap::new();
        // Pushed out of deadline order on purpose.
        heap.push(Scheduled {
            deadline: base + Duration::from_millis(30),
            seq: 2,
            payload: "c",
        });
        heap.push(Scheduled {
            deadline: base + Duration::from_millis(10),
            seq: 0,
            payload: "a",
        });
        heap.push(Scheduled {
            deadline: base + Duration::from_millis(20),
            seq: 1,
            payload: "b",
        });

        let now = base + Duration::from_millis(20);
        let ready = drain_ready(&mut heap, now);
        assert_eq!(
            ready,
            vec!["a", "b"],
            "earliest-first, only entries at/before now"
        );
        assert_eq!(heap.len(), 1, "the 30ms-out entry is still pending");
    }

    #[test]
    fn drain_ready_on_an_empty_heap_returns_nothing() {
        let mut heap: BinaryHeap<Scheduled<i32>> = BinaryHeap::new();
        assert!(drain_ready(&mut heap, Instant::now()).is_empty());
    }

    #[test]
    fn drain_ready_breaks_a_deadline_tie_by_arrival_order() {
        let same = Instant::now() + Duration::from_millis(5);
        let mut heap = BinaryHeap::new();
        heap.push(Scheduled {
            deadline: same,
            seq: 5,
            payload: "later-arrival",
        });
        heap.push(Scheduled {
            deadline: same,
            seq: 1,
            payload: "earlier-arrival",
        });
        assert_eq!(
            drain_ready(&mut heap, same),
            vec!["earlier-arrival", "later-arrival"]
        );
    }

    // ── build_source: decode + trim + gain, no device involved ────────────

    const TEST_WAV: &str = "/home/atobey/src/pawlsa-mcp/pawlsa-test.wav";

    fn test_wav_bytes() -> Option<Vec<u8>> {
        std::fs::read(TEST_WAV).ok()
    }

    #[test]
    fn build_source_decodes_real_wav_bytes_with_no_trim_or_gain() {
        let Some(bytes) = test_wav_bytes() else {
            eprintln!("skipping: {TEST_WAV} not present on this machine");
            return;
        };
        let source = build_source(bytes, None, None, 0.0).expect("real WAV decodes");
        assert!(source.channels() > 0);
    }

    #[test]
    fn build_source_applies_offset_and_length_trim_without_erroring() {
        let Some(bytes) = test_wav_bytes() else {
            eprintln!("skipping: {TEST_WAV} not present on this machine");
            return;
        };
        let trimmed = build_source(
            bytes,
            Some(Duration::from_millis(10)),
            Some(Duration::from_millis(20)),
            -6.0,
        )
        .expect("decodes with trim + gain");
        assert!(trimmed.channels() > 0);
    }

    #[test]
    fn build_source_rejects_garbage_bytes_loudly_never_a_panic() {
        let garbage = vec![0u8; 16];
        assert!(
            build_source(garbage, None, None, 0.0).is_err(),
            "garbage bytes must fail to decode as a Result, never panic"
        );
    }

    // ── handle_cmd: the state machine, exercised with output: None ────────
    //
    // `output: None` never reaches `Sink::try_new`/`OutputStream` — only
    // `run`'s `OutputStream::try_default()` call is genuinely device-bound,
    // so all of this is real coverage of the decode/heap/flush bookkeeping
    // without a device anywhere in the loop.

    #[test]
    fn play_now_with_no_device_decodes_but_plays_nothing() {
        let Some(bytes) = test_wav_bytes() else {
            eprintln!("skipping: {TEST_WAV} not present on this machine");
            return;
        };
        let mut pending = BinaryHeap::new();
        let mut live = Vec::new();
        let mut seq = 0u64;
        handle_cmd(
            SchedulerCmd::PlayNow { bytes },
            None,
            &mut pending,
            &mut live,
            &mut seq,
        );
        assert!(pending.is_empty());
        assert!(
            live.is_empty(),
            "no device means no Sink to track, but must not panic"
        );
    }

    #[test]
    fn play_at_with_a_future_deadline_enqueues_rather_than_firing() {
        let Some(bytes) = test_wav_bytes() else {
            eprintln!("skipping: {TEST_WAV} not present on this machine");
            return;
        };
        let mut pending = BinaryHeap::new();
        let mut live = Vec::new();
        let mut seq = 0u64;
        let deadline = Instant::now() + Duration::from_secs(60);
        handle_cmd(
            SchedulerCmd::PlayAt {
                bytes,
                deadline,
                src_offset: None,
                src_len: None,
                gain_db: 0.0,
            },
            None,
            &mut pending,
            &mut live,
            &mut seq,
        );
        assert_eq!(
            pending.len(),
            1,
            "a future deadline is enqueued, not fired immediately"
        );
    }

    #[test]
    fn play_at_with_a_past_deadline_fires_immediately_instead_of_enqueuing() {
        let Some(bytes) = test_wav_bytes() else {
            eprintln!("skipping: {TEST_WAV} not present on this machine");
            return;
        };
        let mut pending = BinaryHeap::new();
        let mut live = Vec::new();
        let mut seq = 0u64;
        let deadline = Instant::now(); // already due
        handle_cmd(
            SchedulerCmd::PlayAt {
                bytes,
                deadline,
                src_offset: None,
                src_len: None,
                gain_db: 0.0,
            },
            None,
            &mut pending,
            &mut live,
            &mut seq,
        );
        assert!(
            pending.is_empty(),
            "an already-due deadline fires immediately rather than sitting in the heap"
        );
    }

    #[test]
    fn flush_clears_pending_and_stops_every_live_sink() {
        let Some(bytes) = test_wav_bytes() else {
            eprintln!("skipping: {TEST_WAV} not present on this machine");
            return;
        };
        let mut pending = BinaryHeap::new();
        let mut live = Vec::new();
        let mut seq = 0u64;
        for _ in 0..2 {
            let deadline = Instant::now() + Duration::from_secs(60);
            handle_cmd(
                SchedulerCmd::PlayAt {
                    bytes: bytes.clone(),
                    deadline,
                    src_offset: None,
                    src_len: None,
                    gain_db: 0.0,
                },
                None,
                &mut pending,
                &mut live,
                &mut seq,
            );
        }
        assert_eq!(pending.len(), 2);
        handle_cmd(SchedulerCmd::Flush, None, &mut pending, &mut live, &mut seq);
        assert!(pending.is_empty(), "flush drops every pending sound");
        assert!(
            live.is_empty(),
            "no device ever populated `live` here, but flush must not panic"
        );
    }

    #[test]
    fn decode_failure_in_play_now_warns_rather_than_panicking() {
        let mut pending = BinaryHeap::new();
        let mut live = Vec::new();
        let mut seq = 0u64;
        handle_cmd(
            SchedulerCmd::PlayNow {
                bytes: vec![0u8; 4],
            },
            None,
            &mut pending,
            &mut live,
            &mut seq,
        );
        assert!(
            pending.is_empty(),
            "a decode failure never lands in the pending queue"
        );
    }

    // ── The genuinely device-bound edge ────────────────────────────────────

    /// Live device smoke test (needs a real audio output; `#[ignore]`d like
    /// `midi.rs`'s ALSA loopback tests, so CI stays green). Spawns the real
    /// scheduler thread, plays the pawlsa fixture now and on a short lead,
    /// then flushes — the only assertion CI can't make is "it sounded," so
    /// this just proves the whole path runs without panicking end to end.
    #[test]
    #[ignore = "needs a real audio output device; run manually (e.g. on the zorak runner)"]
    fn live_scheduler_thread_plays_and_flushes_without_panicking() {
        let Some(bytes) = test_wav_bytes() else {
            eprintln!("skipping: {TEST_WAV} not present on this machine");
            return;
        };
        let handle = spawn();
        handle.play_now(bytes.clone());
        handle.play_at(
            bytes,
            Instant::now() + Duration::from_millis(200),
            None,
            None,
            -6.0,
        );
        std::thread::sleep(Duration::from_millis(400));
        handle.flush();
        std::thread::sleep(Duration::from_millis(50));
    }
}
