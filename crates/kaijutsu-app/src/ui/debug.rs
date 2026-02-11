//! Debug tools for UI development
//!
//! - F1: Toggle debug overlay (OFF by default)
//! - F12: Save screenshot to design/screenshots/
//! - q: Quit (only in Normal mode)

use bevy::app::AppExit;
use bevy::prelude::*;
use bevy::render::view::screenshot::{save_to_disk, Screenshot};

use crate::cell::{CurrentMode, EditorMode};

/// Configure UI debug overlay (OFF by default, F1 to toggle)
pub fn setup_debug_overlay(mut debug_options: ResMut<UiDebugOptions>) {
    debug_options.enabled = false;
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

/// q quits (only in Normal mode)
pub fn handle_quit(
    keys: Res<ButtonInput<KeyCode>>,
    mode: Res<CurrentMode>,
    mut exit: MessageWriter<AppExit>,
) {
    // Don't quit when tiling modifier (Alt/Super) is held — that's Alt+q close pane
    let tiling_mod = keys.pressed(KeyCode::AltLeft)
        || keys.pressed(KeyCode::AltRight)
        || keys.pressed(KeyCode::SuperLeft)
        || keys.pressed(KeyCode::SuperRight);
    if mode.0 == EditorMode::Normal && keys.just_pressed(KeyCode::KeyQ) && !tiling_mod {
        info!("👋 Quitting...");
        exit.write(AppExit::Success);
    }
}
