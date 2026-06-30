//! Render targets — consumers of a track's committed score (docs/tracks.md,
//! Stage 3 "the render-target seam").
//!
//! A **render** is a *consumer* of the track's score, not a producer: it never
//! schedules cells, never takes a turn, never appears in failure-routing — so it
//! is NOT an `AttachedContext`. It hangs on the `TrackState` as a small registry
//! and is fed from the materialize crossing: when `materialize_track` advances the
//! cursor past a newly-committed `Concrete` ABC cell, it hands that cell's
//! resolved ABC + the cell's *local instant* to every render target.
//!
//! The cell's instant is computed off a **jitter-free** reference — the beat's
//! *scheduled* fire `Instant` (`TrackState.last_fire_scheduled`), NOT the
//! `SystemTime::now()` latched after the heap pop (the jittery actual wakeup) —
//! so per-beat scheduler jitter never accumulates into the output (Stage 3 review,
//! deepseek). Cells commit *ahead* of the playhead (the speculation lead **is**
//! the jitter buffer, midi.md), so the instant is in the near future: a render
//! target schedules into its device queue ahead of time, never just-in-time.

use tokio::time::Instant;

/// A consumer of a track's committed score. M1's one impl is `AlsaMidiOut`
/// (WI 6), which renders the ABC to MIDI events and schedules them into an ALSA
/// seq queue. The trait is `Send` so a target can move onto the beat-scheduler
/// thread; a future cross-node target (RTP-MIDI, midi.md M4) is just another impl
/// — the trait's *home* (on the track) doesn't constrain its *transport*.
pub trait RenderTarget: Send {
    /// Schedule one committed cell's rendered output at `at`. Takes the
    /// **pre-resolved** ABC `&str` (the materialize crossing already ran the
    /// deriver / read CAS once for all targets), so a render target never
    /// re-resolves a `ContentRef` from CAS (Stage 3 lock: `emit(abc:&str)`, not
    /// `emit(&Cell)`). `at` is a near-future local `Instant` on the speculation
    /// lead.
    fn emit(&mut self, abc: &str, at: Instant);

    /// Transport halt (`stop`/`pause`): the lead means the device queue holds ~a
    /// phrase of future events. TRUNCATE this target's already-scheduled events
    /// after `at` and silence any sounding notes, so a stop doesn't blindly play
    /// the buffered phrase (Stage 3 review SEV-1). Default no-op: a target with no
    /// device-side queue (e.g. a test recorder) needs nothing.
    fn flush_scheduled_after(&mut self, _at: Instant) {}
}
