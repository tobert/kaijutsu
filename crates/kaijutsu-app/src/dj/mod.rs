//! The DJ thread — musical dispatch off the frame (`docs/midi.md` "The DJ
//! thread", 2026-07-18). A dedicated `std::thread` (current-thread tokio, one
//! `select!` over {actor events, a Bevy control channel, the click-horizon
//! timer, prefetch outcomes}) owns everything musically time-critical end to
//! end: `RenderCue` parse + deadline math + dispatch, ABC→MIDI render + the
//! ALSA `MidiSink`, the [`LocalBeat`](kaijutsu_audio::LocalBeat) phasor +
//! click scheduling, and CAS prefetch dispatch. The wiring that moved
//! `poll_server_events`'s musical consumers off `Update` was Tasks #3–#4 —
//! complete as of this revision (Task #4, the demolition: `midi.rs` and
//! `metronome.rs` deleted).
//!
//! Slice #1 landed the **decision core**: [`core::DjCore`], a pure state
//! machine with no bevy `Message`/`MessageReader`, no channels, no threads,
//! no ALSA/rodio types — so the click policy and the clock-mode machine are
//! TDD-able with hand-picked `Instant`s, exactly like
//! `timebase.rs`/`audio_sched.rs`'s pure cores. `DjCore` PORTS the click
//! policy (`Metronome::schedule_due`'s never-replay + un-strand ladder)
//! rather than calling into it — at the time, `metronome.rs` still owned the
//! only LIVE copy of that policy (`DjCore`'s copy did nothing yet); Task #4
//! deleted `metronome.rs` for good, so `DjCore`'s copy is now the only one.
//!
//! Slice #2 ([`thread`]) wired `DjCore` into the real thread:
//! [`thread::DjPlugin`] spawned it and forwarded actor/metronome-config
//! traffic from Bevy, but was not yet registered in `main.rs` — clock +
//! clicks only, no cue dispatch.
//!
//! Slice #3 ([`audio`], [`prefetch`], 2026-07-18) was the first LIVE wiring:
//! [`thread::DjPlugin`] registered in `main.rs` (replacing the deleted
//! `audio::AudioOutPlugin`), and the DJ thread dispatches every `audio/*`,
//! [`kaijutsu_audio::CLIP_MIME`], and [`kaijutsu_audio::PREPARE_MIME`]
//! `RenderCue` for real — ported from the deleted `audio.rs`
//! (`play_render_cues`/`drain_prefetch_results`/`CasPrefetch`) with one
//! structural change: [`prefetch::CasPrefetch`]'s outcome channel is a
//! `tokio::mpsc` unbounded pair instead of `crossbeam_channel`, so
//! `thread::run_loop`'s `select!` gets a native async arm for it instead of a
//! per-frame drain.
//!
//! **Task #4 ([`midi`], the demolition) is this revision.** `text/vnd.abc`,
//! clicks, and the ALSA render port move onto this thread too — the whole
//! staged migration `docs/midi.md` describes is complete. `midi.rs`
//! (the app's Bevy-side `MidiOutPlugin`) and `metronome.rs` are DELETED; the
//! DJ thread's owned [`midi::MidiSink`] (generic seam:
//! [`midi::MidiDispatch`]) is the sole ALSA sink and the sole clicker.

pub mod audio;
pub mod core;
pub mod midi;
pub mod prefetch;
pub mod thread;

// `kaijutsu-app` is a binary crate — a `pub use` here has no external
// consumer, so most of these re-exports (kept for callers that prefer
// `dj::X` over naming the submodule) still read as unused: nothing in this
// crate writes `dj::DjCore` etc. rather than importing the submodule
// directly (`thread.rs` does the latter). `thread::DjPlugin` is the
// exception as of Task #3 — `main.rs` names it as `dj::DjPlugin`.
#[allow(unused_imports)]
pub use core::{
    BeatObservation, ClockMode, ClockTransition, DjCore, DueClicks, MetronomeConfig,
    TransitionReason,
};
#[allow(unused_imports)]
pub use thread::{DjCtl, DjHandle, DjPlugin, DjPulse, RenderPortTraffic};
