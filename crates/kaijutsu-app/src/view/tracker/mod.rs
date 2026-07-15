//! Tracker station — the pattern-grid face at East (`docs/tracks.md`; the
//! approved plan is `snazzy-jumping-hejlsberg.md`). Slice 0: track state
//! only (no score-cell content), read-only (no transport RPCs).
//!
//! Placeholder during Sequencing step 1 (`grid.rs` math + tests) — the
//! plugin, placement, and spawn systems land in later steps of the same
//! slice.

pub mod grid;
