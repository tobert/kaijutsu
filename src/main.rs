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
                // Console (runs first, captures input when visible)
                ui::console::toggle_console,
                ui::console::handle_console_input,
                ui::console::update_console_display,
                // Mode handling
                state::mode::handle_mode_input,
                ui::shell::update_mode_indicator,
                // Input handling (keyboard + IME) - skipped when console visible
                ui::input::handle_keyboard_input,
                ui::input::handle_ime_input,
                ui::input::update_input_display,
                // Context area
                ui::context::spawn_messages,
                // Navigation and interaction
                ui::context::handle_navigation,
                ui::context::handle_collapse_toggle,
                ui::context::update_selection_highlight,
            ),
        )
        .run();
}

/// Send demo messages on startup to show DAG structure
fn send_welcome_message(mut events: MessageWriter<ui::context::MessageEvent>) {
    use ui::context::MessageEvent;

    // System welcome
    events.write(MessageEvent::system(
        "Welcome to 会術 Kaijutsu! Press 'i' to enter Insert mode, j/k to navigate.",
    ));

    // Demo conversation showing DAG structure
    events.write(MessageEvent::user("amy", "@claude help me refactor this code"));

    events.write(MessageEvent::agent(
        "claude-opus",
        "I'll analyze the codebase and suggest improvements.",
    ));

    // Tool call (would be child of agent message id=2 in real DAG)
    events.write(MessageEvent::tool_call("Read", 2));

    // Tool result
    events.write(MessageEvent::tool_result("src/main.rs (245 lines)", 3));

    events.write(MessageEvent::agent(
        "claude-opus",
        "Here are my suggestions:\n1. Extract the config parsing\n2. Add error handling",
    ));
}
