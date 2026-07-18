//! The DJ thread — musical dispatch off the frame (`docs/midi.md` "The DJ
//! thread", 2026-07-18). A dedicated `std::thread` (current-thread tokio, one
//! `select!` over {actor events, a Bevy control channel, the click-horizon
//! timer, prefetch outcomes}) owns everything musically time-critical end to
//! end: `RenderCue` parse + deadline math + dispatch, ABC→MIDI render + the
//! ALSA `MidiSink`, the [`LocalBeat`](kaijutsu_audio::LocalBeat) phasor +
//! click scheduling, and CAS prefetch dispatch. The wiring that moves
//! `poll_server_events`'s musical consumers off `Update` is Tasks #3–#4.
//!
//! Slice #1 landed the **decision core**: [`core::DjCore`], a pure state
//! machine with no bevy `Message`/`MessageReader`, no channels, no threads,
//! no ALSA/rodio types — so the click policy and the clock-mode machine are
//! TDD-able with hand-picked `Instant`s, exactly like
//! `timebase.rs`/`audio_sched.rs`'s pure cores. `metronome.rs` is UNCHANGED:
//! `DjCore` PORTS its click policy (`Metronome::schedule_due`'s never-replay
//! + un-strand ladder) rather than calling into it, so the metronome keeps
//! working standalone until Task #4 deletes its now-redundant copy.
//!
//! Slice #2 ([`thread`]) wires `DjCore` into the real thread: [`thread::DjPlugin`]
//! spawns it and forwards actor/metronome-config traffic from Bevy, but is
//! **not registered in `main.rs` yet** — Task #3 does that once a real cue
//! sink exists to make the thread's output audible. This task is clock +
//! clicks only, same scope `DjCore` itself declares; every other `RenderCue`
//! mime and the CAS-prefetch/ALSA sinks are Tasks #3/#4.

pub mod core;
pub mod thread;

// `kaijutsu-app` is a binary crate — a `pub use` here has no external
// consumer, so these re-exports (kept for callers that prefer `dj::X` over
// naming the submodule) read as unused until something in this crate
// actually writes `dj::DjCore` etc. rather than importing the submodule
// directly (`thread.rs` does the latter). `thread::DjPlugin` in particular
// is unused until Task #3 wires `.add_plugins(dj::DjPlugin)` into `main.rs`.
#[allow(unused_imports)]
pub use core::{
    BeatObservation, ClockMode, ClockTransition, DjCore, DueClicks, MetronomeConfig,
    TransitionReason,
};
#[allow(unused_imports)]
pub use thread::{DjCtl, DjHandle, DjPlugin, DjPulse};
