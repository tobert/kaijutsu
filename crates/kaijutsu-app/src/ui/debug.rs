//! Debug tools for UI development
//!
//! - F1: Toggle debug overlay (OFF by default)
//! - F12: Save screenshot to design/screenshots/
//! - q: Quit (only in Normal mode)

use bevy::prelude::*;

/// Configure UI debug overlay (OFF by default, F1 to toggle)
///
/// Bevy 0.19 split this in two: `UiDebugOptions` is now a per-node `Component`
/// override, and `GlobalUiDebugOptions` is the app-wide resource. We want the
/// app-wide one. The new `outline_*` fields keep their defaults
/// (`outline_border_box: true`, the rest false) — that is 0.18's behavior.
pub fn setup_debug_overlay(mut debug_options: ResMut<GlobalUiDebugOptions>) {
    debug_options.enabled = false;
    debug_options.line_width = 1.0;
    debug_options.show_hidden = false;
    debug_options.show_clipped = true;
}

// Debug/quit/screenshot input handling in input::systems.
