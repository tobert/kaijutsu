use bevy::prelude::*;

mod state;
mod ui;

fn main() {
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "会術 Kaijutsu".into(),
                resolution: (1280, 800).into(),
                ..default()
            }),
            ..default()
        }))
        // State
        .init_state::<state::mode::Mode>()
        // Resources
        .init_resource::<ui::theme::Theme>()
        .init_resource::<state::input::InputBuffer>()
        .init_resource::<state::nav::NavigationState>()
        .init_resource::<ui::context::MessageCount>()
        .init_resource::<ui::console::ConsoleState>()
        // Messages
        .add_message::<ui::context::MessageEvent>()
        // Startup
        .add_systems(Startup, (ui::shell::setup, ui::console::setup_console, send_welcome_message))
        // Update
        .add_systems(
            Update,
            (
                // Mode handling
                state::mode::handle_mode_input,
                ui::shell::update_mode_indicator,
                // Input handling (keyboard + IME)
                ui::input::handle_keyboard_input,
                ui::input::handle_ime_input,
                ui::input::update_input_display,
                // Context area
                ui::context::spawn_messages,
                // Navigation
                ui::context::handle_navigation,
                ui::context::update_selection_highlight,
                // Console
                ui::console::toggle_console,
            ),
        )
        .run();
}

/// Send a welcome message on startup
fn send_welcome_message(mut events: MessageWriter<ui::context::MessageEvent>) {
    events.write(ui::context::MessageEvent {
        sender: "system".to_string(),
        content: "Welcome to 会術 Kaijutsu! Press 'i' to enter Insert mode.".to_string(),
    });
}
