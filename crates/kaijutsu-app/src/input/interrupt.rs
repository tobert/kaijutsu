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

/// Time window for counting consecutive interrupt presses (milliseconds).
const WINDOW_MS: u128 = 500;

/// Per-session state for the multi-press interrupt gesture.
#[derive(Resource, Default)]
pub struct InterruptState {
    count: u8,
    last_press: Option<std::time::Instant>,
}

impl InterruptState {
    /// Record a new interrupt press and return the current count (1, 2, or 3).
    ///
    /// If the previous press was more than `WINDOW_MS` ago, the count resets to 1.
    pub fn press(&mut self) -> u8 {
        let now = std::time::Instant::now();
        if let Some(last) = self.last_press {
            if now.duration_since(last).as_millis() < WINDOW_MS {
                self.count = (self.count + 1).min(3);
            } else {
                self.count = 1;
            }
        } else {
            self.count = 1;
        }
        self.last_press = Some(now);
        self.count
    }

    /// Reset the press count and timestamp (after a 3-press clear).
    pub fn reset(&mut self) {
        self.count = 0;
        self.last_press = None;
    }
}
