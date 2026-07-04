//! # kaijutsu-viz — D3-inspired scales for bespoke data-driven views
//!
//! A small, dependency-free substrate of D3-style scales. First and only
//! committed consumer: the **time-well context browser** (see
//! `docs/timewell.md` — esp. its substrate-notes appendix — and
//! `docs/time-well-concepts.md`).
//!
//! ## No-clamp stance
//!
//! **Clamping is opt-in, not the default.** All scales extrapolate freely
//! outside the domain by default — the same contract D3 holds. Clamp with
//! `.clamp(true)` on the builder. This matches the project preference for
//! relative values over fixed-pixel clamps; a scale that silently pins values
//! hides logic errors behind quiet behavior.
//!
//! ## Invertibility
//!
//! Every continuous scale ships `invert`; every threshold scale ships
//! `invert_extent`. `invert(scale(x)) ≈ x` is a property-tested invariant.

pub mod join;
pub mod layout;
pub mod scales;
