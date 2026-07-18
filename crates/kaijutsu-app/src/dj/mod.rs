//! The DJ thread — musical dispatch off the frame (`docs/midi.md` "The DJ
//! thread", 2026-07-18). A dedicated `std::thread` (current-thread tokio, one
//! `select!` over {actor events, a Bevy control channel, the click-horizon
//! timer, prefetch outcomes}) will own everything musically time-critical end
//! to end: `RenderCue` parse + deadline math + dispatch, ABC→MIDI render +
//! the ALSA `MidiSink`, the [`LocalBeat`](kaijutsu_audio::LocalBeat) phasor +
//! click scheduling, and CAS prefetch dispatch. That thread — and the wiring
//! that moves `poll_server_events`' musical consumers off `Update` — is
//! Tasks #2–#4.
//!
//! This slice (#1) lands only the **decision core**: [`core::DjCore`], a pure
//! state machine with no bevy `Message`/`MessageReader`, no channels, no
//! threads, no ALSA/rodio types — so the click policy and the clock-mode
//! machine are TDD-able with hand-picked `Instant`s, exactly like
//! `timebase.rs`/`audio_sched.rs`'s pure cores. `metronome.rs` is UNCHANGED:
//! `DjCore` PORTS its click policy (`Metronome::schedule_due`'s never-replay
//! + un-strand ladder) rather than calling into it, so the metronome keeps
//! working standalone until Task #4 deletes its now-redundant copy.

pub mod core;

// Plan-phase scaffolding (DJ-thread arc Task #1 of 4): re-exported for
// Task #2's thread wiring, which is the named consumer — nothing imports
// these yet.
#[allow(unused_imports)]
pub use core::{
    BeatObservation, ClockMode, ClockTransition, DjCore, DueClicks, MetronomeConfig,
    TransitionReason,
};
