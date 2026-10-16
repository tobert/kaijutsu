//! The council: a System 1 model check on shell submissions the gate's
//! static rules leave uncovered. See `docs/council.md`.
//!
//! `sync` keeps the council server holding the council contexts and specs;
//! `projection` turns a kaijutsu context into the server's context body;
//! `seat` builds a council v2 queue seat from that body; `gate` makes one
//! decision for a submission and records it; `shell_case` builds the
//! statements its shell case shows.

pub(crate) mod gate;
#[cfg(test)]
mod gate_e2e;
pub(crate) mod observe;
pub(crate) mod programs;
mod projection;
pub(crate) mod report_stop;
pub(crate) mod reviewer_contexts;
mod seat;
pub(crate) mod shadow;
mod shell_case;
pub(crate) mod sync;
