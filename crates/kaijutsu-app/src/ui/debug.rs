//! Debug tools for UI development
//!
//! - F1: Toggle debug overlay OFF (ON by default during development)
//! - F12: Save screenshot to design/screenshots/

use bevy::prelude::*;
use bevy::render::view::screenshot::{save_to_disk, Screenshot};

/// Enable the UI debug overlay by default for development
pub fn setup_debug_overlay(mut debug_options: ResMut<UiDebugOptions>) {
    debug_options.enabled = true;
    debug_options.line_width = 1.0;
    debug_options.show_hidden = false;
    debug_options.show_clipped = true;
}

/// F1 toggles debug overlay
pub fn handle_debug_toggle(
    keys: Res<ButtonInput<KeyCode>>,
    mut debug_options: ResMut<UiDebugOptions>,
) {
    if keys.just_pressed(KeyCode::F1) {
        debug_options.toggle();
        info!(
            "UI debug overlay: {}",
            if debug_options.enabled { "ON" } else { "OFF" }
        );
    }
}

/// F12 saves a screenshot
pub fn handle_screenshot(mut commands: Commands, keys: Res<ButtonInput<KeyCode>>) {
    if keys.just_pressed(KeyCode::F12) {
        // Create timestamped filename
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = format!("design/screenshots/screenshot-{}.png", timestamp);

        // Ensure directory exists
        let _ = std::fs::create_dir_all("design/screenshots");

        info!("📸 Saving screenshot to {}", path);
        commands
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk(path));
    }
}
