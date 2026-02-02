//! Timeline module - temporal navigation through conversation history.
//!
//! The timeline replaces traditional scroll as the primary navigation paradigm.
//! Instead of scrolling through space, users scrub through time.
//!
//! ## Fork-First Temporal Model
//!
//! The past is read-only, but forking is ubiquitous:
//! - **Fork from here**: Creates new context branching from any point in history
//! - **Cherry-pick block**: Pull a block (with lineage) into another context
//!
//! This isn't "edit the past" - it's "branch a new future from the past."
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────┐
//! │ Conversation Content (fades based on timeline position) │
//! │                                                          │
//! │ [past blocks dim] ─────────── [current blocks bright]   │
//! │                                                          │
//! └─────────────────────────────────────────────────────────┘
//! ┌─────────────────────────────────────────────────────────┐
//! │ │░░░░░░░░░░░░░░░░░░░░│▓▓▓▓▓▓│ Timeline Scrubber        │
//! │ ↑                     ↑                                 │
//! │ past                  now   [Fork] [Jump to Now]        │
//! └─────────────────────────────────────────────────────────┘
//! ```

mod components;
mod plugin;
mod systems;

pub use plugin::TimelinePlugin;

// Re-export key types (components are used internally by systems)
// Some aren't consumed externally yet but will be when RPC integration completes
#[allow(unused_imports)]
pub use components::{
    ForkRequest, ForkResult, CherryPickRequest, CherryPickResult,
    TimelineState, TimelineViewMode, TimelineVisibility,
};
