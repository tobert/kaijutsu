//! The council: a System 1 model check on shell submissions the gate's
//! static rules leave uncovered. See `docs/council.md`.
//!
//! `sync` keeps the council server holding the council contexts and specs;
//! `projection` turns a kaijutsu context into the server's context body;
//! `gate` makes one decision for a submission and records it.

pub(crate) mod gate;
#[cfg(test)]
mod gate_e2e;
pub(crate) mod observe;
pub(crate) mod programs;
mod projection;
pub(crate) mod report_stop;
pub(crate) mod reviewer_contexts;
pub(crate) mod shadow;
pub(crate) mod sync;
