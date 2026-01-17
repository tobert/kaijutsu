//! Kaijutsu App - Cell-based collaborative workspace
//!
//! A fresh implementation with cells as the universal primitive.
//! CRDT sync via diamond-types, cosmic-text rendering.

use bevy::prelude::*;
use bevy_brp_extras::BrpExtrasPlugin;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

mod cell;
mod connection;
mod shaders;
mod text;
mod ui;

// Re-export client crate's generated code
pub use kaijutsu_client::kaijutsu_capnp;

fn main() {
    // Set up file logging
    let log_dir = std::env::var("KAIJUTSU_LOG_DIR")
        .unwrap_or_else(|_| "/tmp".to_string());
    let file_appender = tracing_appender::rolling::never(&log_dir, "kaijutsu-app.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            // Default to info for our crates, warn for others
            "kaijutsu_app=debug,kaijutsu_client=debug,warn".into()
        }))
        .with(fmt::layer().with_writer(non_blocking).with_ansi(false))
        .with(fmt::layer().with_writer(std::io::stderr))
        .init();

    info!("Starting Kaijutsu App - logging to {}/kaijutsu-app.log", log_dir);

    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "会術 Kaijutsu".into(),
                resolution: (1280, 800).into(),
                ..default()
            }),
            ..default()
        }))
        // Remote debugging (BRP) - BrpExtrasPlugin includes RemotePlugin
        .add_plugins(BrpExtrasPlugin)
        // Text rendering (glyphon + cosmic-text)
        .add_plugins(text::TextRenderPlugin)
        // Cell editing
        .add_plugins(cell::CellPlugin)
        // Shader effects
        .add_plugins(shaders::ShaderFxPlugin)
        // Connection plugin (spawns background thread)
        .add_plugins(connection::ConnectionBridgePlugin)
        // Resources
        .init_resource::<ui::theme::Theme>()
        // Startup
        .add_systems(Startup, (
            setup_camera,
            setup_placeholder_ui,
            setup_initial_cell,
            ui::debug::setup_debug_overlay,
            ui::mode_indicator::setup_mode_indicator,
            cell::plugin::setup_frame_styles,
        ))
        // Update
        .add_systems(Update, (
            handle_connection_events,
            ui::debug::handle_debug_toggle,
            ui::debug::handle_screenshot,
            ui::debug::handle_quit,
            ui::mode_indicator::update_mode_indicator,
        ))
        .run();
}

/// Setup 2D camera for UI
fn setup_camera(mut commands: Commands, theme: Res<ui::theme::Theme>) {
    commands.spawn((
        Camera2d,
        Camera {
            clear_color: ClearColorConfig::Custom(theme.bg),
            ..default()
        },
    ));
}

/// Temporary placeholder UI - shows connection status
fn setup_placeholder_ui(mut commands: Commands, theme: Res<ui::theme::Theme>) {
    // Root container - NO background so glyphon text shows through
    commands
        .spawn((
            Node {
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                justify_content: JustifyContent::FlexStart, // Top-aligned
                padding: UiRect::top(Val::Px(20.0)),
                ..default()
            },
            // No BackgroundColor - let glyphon text show through
        ))
        .with_children(|parent| {
            // Title
            parent.spawn((
                Text::new("会術 Kaijutsu"),
                TextFont {
                    font_size: 48.0,
                    ..default()
                },
                TextColor(theme.accent),
            ));

            // Subtitle
            parent.spawn((
                Text::new("Cell-based Collaborative Workspace"),
                TextFont {
                    font_size: 18.0,
                    ..default()
                },
                TextColor(theme.fg_dim),
                Node {
                    margin: UiRect::top(Val::Px(8.0)),
                    ..default()
                },
            ));

            // Status marker
            parent.spawn((
                StatusText,
                Text::new("Connecting..."),
                TextFont {
                    font_size: 14.0,
                    ..default()
                },
                TextColor(theme.fg_dim),
                Node {
                    margin: UiRect::top(Val::Px(32.0)),
                    ..default()
                },
            ));

            // Instructions
            parent.spawn((
                Text::new("F1: Debug | F2: New cell | F3: Toggle shaders | F12: Screenshot"),
                TextFont {
                    font_size: 12.0,
                    ..default()
                },
                TextColor(theme.fg_dim),
                Node {
                    margin: UiRect::top(Val::Px(16.0)),
                    ..default()
                },
            ));
        });
}

/// Spawn an initial test cell on startup
fn setup_initial_cell(
    mut commands: Commands,
    font_system: Res<text::SharedFontSystem>,
) {
    use cell::{Cell, CellEditor, CellPosition, CellState};
    use text::{GlyphonText, TextAreaConfig, TextBuffer};

    // Create a proper TextBuffer with the font system
    let text_content = "// Welcome to Kaijutsu cells!\n\
                        // Press i to enter INSERT mode\n\
                        // Press F2 to spawn more cells\n\n\
                        fn main() {\n    \
                            println!(\"Hello, 会術!\");\n\
                        }\n";

    let buffer = {
        let Ok(mut fs) = font_system.0.lock() else {
            warn!("Could not lock font system for initial cell");
            return;
        };
        let metrics = glyphon::Metrics::new(14.0, 20.0);
        let mut buf = glyphon::Buffer::new(&mut fs, metrics);
        buf.set_size(&mut fs, Some(700.0), Some(400.0));
        let attrs = glyphon::Attrs::new().family(glyphon::Family::Monospace);
        buf.set_text(&mut fs, text_content, &attrs, glyphon::Shaping::Advanced, None);
        buf.shape_until_scroll(&mut fs, false);
        buf
    };

    commands.spawn((
        Cell::code("rust"),
        CellEditor::default().with_text(text_content),
        CellState::new(),
        CellPosition::new(0, 0),
        GlyphonText,
        TextBuffer::from_buffer(buffer),
        TextAreaConfig {
            left: 20.0,
            top: 120.0,
            scale: 1.0,
            bounds: glyphon::TextBounds {
                left: 20,
                top: 120,
                right: 720,
                bottom: 520,
            },
            default_color: glyphon::Color::rgb(255, 0, 0), // BRIGHT RED for debugging
        },
    ));

    info!("Spawned initial cell");
}

/// Marker for status text
#[derive(Component)]
struct StatusText;

/// Convert connection events to UI updates
fn handle_connection_events(
    mut conn_events: MessageReader<connection::ConnectionEvent>,
    mut status_text: Query<(&mut Text, &mut TextColor), With<StatusText>>,
    theme: Res<ui::theme::Theme>,
) {
    use connection::ConnectionEvent;

    for event in conn_events.read() {
        for (mut text, mut color) in status_text.iter_mut() {
            match event {
                ConnectionEvent::Connected => {
                    text.0 = "✓ Connected to server".into();
                    *color = TextColor(theme.row_result);
                }
                ConnectionEvent::Disconnected => {
                    text.0 = "⚡ Disconnected (reconnecting...)".into();
                    *color = TextColor(theme.row_tool);
                }
                ConnectionEvent::ConnectionFailed(err) => {
                    text.0 = format!("✗ {}", err);
                    *color = TextColor(theme.accent2);
                }
                ConnectionEvent::Reconnecting { attempt, .. } => {
                    text.0 = format!("⟳ Reconnecting (attempt {})...", attempt);
                    *color = TextColor(theme.fg_dim);
                }
                ConnectionEvent::AttachedKernel(info) => {
                    text.0 = format!("✓ Attached to kernel: {}", info.name);
                    *color = TextColor(theme.row_result);
                }
                _ => {}
            }
        }
    }
}
