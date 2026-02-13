//! Debug tools for UI development
//!
//! - F1: Toggle debug overlay (OFF by default)
//! - F12: Save screenshot to design/screenshots/
//! - q: Quit (only in Normal mode)

use bevy::prelude::*;

/// Configure UI debug overlay (OFF by default, F1 to toggle)
pub fn setup_debug_overlay(mut debug_options: ResMut<UiDebugOptions>) {
    debug_options.enabled = false;
    debug_options.line_width = 1.0;
    debug_options.show_hidden = false;
    debug_options.show_clipped = true;
}

// Debug/quit/screenshot input handling in input::systems.
