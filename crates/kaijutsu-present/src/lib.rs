//! Presentation logic shared by kaijutsu's clients.
//!
//! Everything here maps kernel data to something a renderer can draw, and
//! nothing here knows what draws it: no toolkit, no ECS, no I/O, no clock
//! beyond the `Instant` a caller hands in. The Bevy app
//! (`kaijutsu-app`) and the terminal client both consume it, which is what
//! keeps one projection of a block instead of two.
//!
//! Color is the seam. A block resolves to a [`format::BlockTone`] and a
//! markdown span to a [`markdown::SpanTone`] — the roles a theme has names
//! for — and each client turns a tone into its own color type. See
//! `docs/tui.md`, "What is reused, what is new".
//!
//! | Module | What it maps |
//! |---|---|
//! | [`format`] | a `BlockSnapshot` to display text and a tone |
//! | [`markdown`] | markdown source to styled spans |
//! | [`kaish`] | a kaish command to a validity verdict and tokens |
//! | [`action`] | the user-intent vocabulary every input device maps onto |
//! | [`beats`] | per-track beat phasors and the pulse envelope |

pub mod action;
pub mod beats;
pub mod format;
pub mod kaish;
pub mod markdown;
