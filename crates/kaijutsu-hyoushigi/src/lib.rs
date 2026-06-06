//! # 拍子木 Hyoushigi — kaijutsu's heart of time
//!
//! Hyoushigi is kaijutsu's timing substrate: the engine that gives every context
//! a sense of *when*, a memory of *what already happened*, and a machine for
//! *staging what comes next*. It is the same engine whether a context is writing
//! code, composing MIDI, or rendering audio — those differ only in how fast their
//! clock ticks and whether that clock is allowed to wait.
//!
//! See `docs/hyoushigi.md` for the full design. The core contract:
//!
//! - A [`Cell`] is exactly three things: a **position** ([`Span`]), a way to
//!   **produce content** ([`Body`] — [`ContentRef`] or [`Recipe`]), and a
//!   **state** ([`CellState`]).
//! - The one capability is the [`Resolver`] trait. Everything content-specific is
//!   a resolver; the scheduler never sees anything narrower.
//! - Content *type* lives inside [`ContentRef`] (`hash + open-string mime`), opaque
//!   to the core — a new modality is a downstream `impl Resolver`, zero substrate
//!   change.

pub mod cell;
pub mod content;
pub mod engine;
pub mod materialize;
pub mod resolver;

pub use cell::{Body, Cell, CellState, ContextQuery, Fallback, Recipe, ResolverId};
pub use content::{ContentRef, ContextHash};
pub use engine::{Recovery, ScheduleError, SquashEvent, TickClock, Timeline};
pub use resolver::{ResolveError, Resolution, Resolver, ResolverCtx};

// The CAS hash newtype is the cell-body contract's anchor; re-export it so callers
// don't reach into `kaijutsu-cas` for the common case.
pub use kaijutsu_cas::ContentHash;

// Logical timeline coordinates live in `kaijutsu-types` (so blocks can name them
// without a dependency cycle); re-export for convenience.
pub use kaijutsu_types::{Span, Tick, TickDelta};
