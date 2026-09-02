//! BRP glue for the "agent drives the app" surface.
//!
//! Kaish syntax validation itself lives in
//! [`kaijutsu_present::kaish`] — both clients validate shell input the same
//! way. [`brp_methods`] stays here because it is Bevy Remote Protocol glue,
//! not kaish parsing: it registers the custom methods (context switch/query)
//! agents call over the same protocol they use for scene inspection.

pub mod brp_methods;

pub use kaijutsu_present::kaish::validate;
