//! Hardware I/O and musical scheduling, independent of any user interface.

mod audio_sched;
pub mod dj;
mod inventory_report;
mod midi_exchange;
mod midi_in;
pub mod midi_match;
pub mod midi_presence;
pub mod patch_graph;
mod runtime;
mod scheduling;
mod capture_export;
mod takes;
pub mod history;
mod observer;

pub use runtime::{Engine, Options};
pub use takes::CaptureControl;
pub use audio_sched::output_names;

#[cfg(test)]
mod tests {
    #[test]
    fn runtime_does_not_require_bevy() {
        let options = super::Options::default();
        assert!(options.audio);
        assert_eq!(options.rt_priority, 0);
    }
}
