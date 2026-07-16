//! Multi-press interrupt state machine.
//!
//! Tracks rapid Ctrl+C presses to implement a graduated cancel gesture.
//! The `InterruptState` resource is checked by `handle_interrupt` each time an
//! `Action::InterruptContext` fires.
//!
//! # Press counts
//! - **1 press**: soft interrupt — stop agentic loop after current tool turn
//! - **2 presses** (< 500ms after first): hard interrupt — abort LLM stream + kill jobs
//! - **3+ presses**: hard interrupt + clear the compose buffer
//!
//! # Key-repeat
//! `dispatch.rs` already filters out key-repeat events with `if !is_repeat`,
//! so the count tracks physical presses only.

use bevy::prelude::*;

use super::tap::TapCounter;

/// Time window for counting consecutive interrupt presses (milliseconds).
const WINDOW_MS: u128 = 500;

/// Per-session state for the multi-press interrupt gesture.
#[derive(Resource)]
pub struct InterruptState(TapCounter);

impl Default for InterruptState {
    fn default() -> Self {
        Self(TapCounter::new(WINDOW_MS, 3))
    }
}

impl InterruptState {
    /// Record a new interrupt press and return the current count (1, 2, or 3).
    ///
    /// If the previous press was more than `WINDOW_MS` ago, the count resets to 1.
    pub fn press(&mut self) -> u8 {
        self.0.press()
    }

    /// Reset the press count and timestamp (after a 3-press clear).
    pub fn reset(&mut self) {
        self.0.reset()
    }
}
