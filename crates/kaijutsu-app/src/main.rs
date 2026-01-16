use bevy::prelude::*;

mod connection;
mod state;
mod ui;

// Re-export client crate's generated code
pub use kaijutsu_client::kaijutsu_capnp;

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
        // Connection plugin (spawns background thread)
        .add_plugins(connection::ConnectionBridgePlugin)
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
        .add_systems(Startup, (ui::shell::setup, ui::console::setup_console, startup_connect))
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
                // Input submission
                handle_input_submit,
                // Context area
                ui::context::spawn_messages,
                // Navigation and interaction
                ui::context::handle_navigation,
                ui::context::handle_collapse_toggle,
                ui::context::update_selection_highlight,
                // Connection events → UI
                handle_connection_events,
            ),
        )
        .run();
}

/// On startup, connect to a local test server (if running)
fn startup_connect(
    cmds: Res<connection::ConnectionCommands>,
    mut events: MessageWriter<ui::context::MessageEvent>,
) {
    // Welcome message
    events.write(ui::context::MessageEvent::system(
        "Welcome to 会術 Kaijutsu! Press 'i' to enter Insert mode, j/k to navigate.",
    ));

    // Try connecting to local test server
    events.write(ui::context::MessageEvent::system(
        "Connecting to localhost:7878...",
    ));
    cmds.send(connection::ConnectionCommand::ConnectTcp {
        addr: "127.0.0.1:7878".to_string(),
    });
}

/// Handle Enter key in Insert mode to submit input
fn handle_input_submit(
    keys: Res<ButtonInput<KeyCode>>,
    mode: Res<State<state::mode::Mode>>,
    mut input: ResMut<state::input::InputBuffer>,
    console_state: Res<ui::console::ConsoleState>,
    conn_state: Res<connection::ConnectionState>,
    cmds: Res<connection::ConnectionCommands>,
    mut events: MessageWriter<ui::context::MessageEvent>,
) {
    // Don't handle if console is visible (console handles its own input)
    if console_state.visible {
        return;
    }

    // Only in Insert mode
    if *mode.get() != state::mode::Mode::Insert {
        return;
    }

    if !keys.just_pressed(KeyCode::Enter) {
        return;
    }

    let content = input.text.trim().to_string();
    if content.is_empty() {
        return;
    }

    // Clear input
    input.text.clear();
    input.preedit.clear();

    // Handle slash commands
    if content.starts_with('/') {
        handle_slash_command(&content, &cmds, &mut events);
        return;
    }

    // Send to server if connected and in a kernel
    if conn_state.connected && conn_state.current_kernel.is_some() {
        // Check for @mention
        if let Some(mention) = content.strip_prefix('@') {
            if let Some((agent, rest)) = mention.split_once(' ') {
                cmds.send(connection::ConnectionCommand::MentionAgent {
                    agent: agent.to_string(),
                    content: rest.to_string(),
                });
            } else {
                events.write(ui::context::MessageEvent::system(
                    "Usage: @agent message",
                ));
            }
        } else {
            cmds.send(connection::ConnectionCommand::SendMessage { content });
        }
    } else if !conn_state.connected {
        events.write(ui::context::MessageEvent::system(
            "Not connected. Use /connect <host:port> to connect.",
        ));
    } else {
        events.write(ui::context::MessageEvent::system(
            "Not attached to a kernel. Use /attach <id> to attach.",
        ));
    }
}

/// Handle slash commands
fn handle_slash_command(
    cmd: &str,
    conn_cmds: &connection::ConnectionCommands,
    events: &mut MessageWriter<ui::context::MessageEvent>,
) {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    match parts.first().copied() {
        Some("/connect") => {
            if let Some(addr) = parts.get(1) {
                events.write(ui::context::MessageEvent::system(format!(
                    "Connecting to {}...",
                    addr
                )));
                conn_cmds.send(connection::ConnectionCommand::ConnectTcp {
                    addr: addr.to_string(),
                });
            } else {
                events.write(ui::context::MessageEvent::system(
                    "Usage: /connect <host:port>",
                ));
            }
        }
        Some("/disconnect") => {
            conn_cmds.send(connection::ConnectionCommand::Disconnect);
        }
        Some("/attach") => {
            if let Some(id) = parts.get(1) {
                conn_cmds.send(connection::ConnectionCommand::AttachKernel {
                    id: id.to_string(),
                });
            } else {
                events.write(ui::context::MessageEvent::system("Usage: /attach <kernel-id>"));
            }
        }
        Some("/detach") => {
            conn_cmds.send(connection::ConnectionCommand::DetachKernel);
        }
        Some("/kernels") => {
            conn_cmds.send(connection::ConnectionCommand::ListKernels);
        }
        Some("/whoami") => {
            conn_cmds.send(connection::ConnectionCommand::Whoami);
        }
        Some("/history") => {
            let limit = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(50);
            conn_cmds.send(connection::ConnectionCommand::GetHistory { limit });
        }
        Some("/help") => {
            events.write(ui::context::MessageEvent::system(
                "Commands: /connect <addr>, /disconnect, /attach <id>, /detach, /kernels, /whoami, /history [n]",
            ));
        }
        _ => {
            events.write(ui::context::MessageEvent::system(format!(
                "Unknown command: {}. Type /help for help.",
                cmd
            )));
        }
    }
}

/// Convert connection events to UI messages
fn handle_connection_events(
    mut conn_events: MessageReader<connection::ConnectionEvent>,
    mut ui_events: MessageWriter<ui::context::MessageEvent>,
) {
    use connection::ConnectionEvent;
    use ui::context::{MessageEvent, RowType};

    for event in conn_events.read() {
        match event {
            ConnectionEvent::Connected => {
                ui_events.write(MessageEvent::system("✓ Connected to server"));
            }
            ConnectionEvent::Disconnected => {
                ui_events.write(MessageEvent::system("Disconnected from server"));
            }
            ConnectionEvent::ConnectionFailed(err) => {
                ui_events.write(MessageEvent::system(format!("✗ Connection failed: {}", err)));
            }
            ConnectionEvent::Identity(id) => {
                ui_events.write(MessageEvent::system(format!(
                    "Logged in as: {} ({})",
                    id.display_name, id.username
                )));
            }
            ConnectionEvent::KernelList(kernels) => {
                if kernels.is_empty() {
                    ui_events.write(MessageEvent::system("No kernels available"));
                } else {
                    let list = kernels
                        .iter()
                        .map(|k| format!("  {} ({})", k.name, k.id))
                        .collect::<Vec<_>>()
                        .join("\n");
                    ui_events.write(MessageEvent::system(format!("Available kernels:\n{}", list)));
                }
            }
            ConnectionEvent::AttachedKernel(info) => {
                ui_events.write(MessageEvent::system(format!(
                    "Attached to kernel: {} ({})",
                    info.name, info.id
                )));
            }
            ConnectionEvent::DetachedKernel => {
                ui_events.write(MessageEvent::system("Detached from kernel"));
            }
            ConnectionEvent::NewMessage(row) => {
                // Convert server Row to UI MessageEvent
                let row_type = match row.row_type {
                    connection::RowType::Chat => RowType::User,
                    connection::RowType::AgentResponse => RowType::Agent,
                    connection::RowType::ToolCall => RowType::ToolCall,
                    connection::RowType::ToolResult => RowType::ToolResult,
                    connection::RowType::SystemMessage => RowType::System,
                };
                ui_events.write(MessageEvent {
                    sender: row.sender.clone(),
                    content: row.content.clone(),
                    row_type,
                    parent_id: None, // TODO: map server IDs to local IDs
                });
            }
            ConnectionEvent::History(rows) => {
                for row in rows {
                    let row_type = match row.row_type {
                        connection::RowType::Chat => RowType::User,
                        connection::RowType::AgentResponse => RowType::Agent,
                        connection::RowType::ToolCall => RowType::ToolCall,
                        connection::RowType::ToolResult => RowType::ToolResult,
                        connection::RowType::SystemMessage => RowType::System,
                    };
                    ui_events.write(MessageEvent {
                        sender: row.sender.clone(),
                        content: row.content.clone(),
                        row_type,
                        parent_id: None,
                    });
                }
            }
            ConnectionEvent::Error(err) => {
                ui_events.write(MessageEvent::system(format!("Error: {}", err)));
            }
        }
    }
}
