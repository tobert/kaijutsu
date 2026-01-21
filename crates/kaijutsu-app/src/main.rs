//! Kaijutsu App - Cell-based collaborative workspace
//!
//! A fresh implementation with cells as the universal primitive.
//! CRDT sync via diamond-types, cosmic-text rendering.

// Bevy ECS idioms that trigger these lints
#![allow(clippy::too_many_arguments)]
#![allow(clippy::type_complexity)]

use bevy::prelude::*;
use bevy_brp_extras::BrpExtrasPlugin;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

mod cell;
mod commands;
mod connection;
mod constants;
mod conversation;
mod dashboard;
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
        .with(EnvFilter::new("warn,kaijutsu_app=debug,kaijutsu_client=debug"))
        .with(fmt::layer().with_writer(non_blocking).with_ansi(false))
        .with(fmt::layer().with_writer(std::io::stderr))
        .init();

    info!("Starting Kaijutsu App - logging to {}/kaijutsu-app.log", log_dir);

    App::new()
        .add_plugins(DefaultPlugins
            .set(WindowPlugin {
                primary_window: Some(Window {
                    title: "会術 Kaijutsu".into(),
                    resolution: (constants::DEFAULT_WINDOW_WIDTH, constants::DEFAULT_WINDOW_HEIGHT).into(),
                    ..default()
                }),
                ..default()
            })
            .set(AssetPlugin {
                // Assets are at workspace root, not crate directory
                file_path: "../../assets".into(),
                ..default()
            })
            // Disable Bevy's LogPlugin - we set up our own tracing subscriber
            .disable::<bevy::log::LogPlugin>()
        )
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
        // Conversation management
        .add_plugins(conversation::ConversationPlugin)
        // Dashboard/lobby experience
        .add_plugins(dashboard::DashboardPlugin)
        // Commands (vim-style : commands)
        .add_plugins(commands::CommandsPlugin)
        // Resources
        .init_resource::<ui::theme::Theme>()
        // Startup
        .add_systems(Startup, (
            setup_camera,
            setup_placeholder_ui,
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

/// Set up the main conversation UI layout.
///
/// Layout structure:
/// ```text
/// ┌─────────────────────────────────────────────────────┐
/// │ 会術 Kaijutsu                    [status: kernel]   │  ← Header
/// ├─────────────────────────────────────────────────────┤
/// │ ┌─────────────────────────────────────────────────┐ │
/// │ │  (scrollable conversation messages)             │ │  ← Conversation
/// │ │                                               ▼ │ │    Container
/// │ └─────────────────────────────────────────────────┘ │
/// ├─────────────────────────────────────────────────────┤
/// │ ┌─────────────────────────────────────────────────┐ │
/// │ │ Type your message...                            │ │  ← Prompt
/// │ └─────────────────────────────────────────────────┘ │    Container
/// │ [NORMAL]                              [Ctrl+Enter] │  ← Status bar
/// └─────────────────────────────────────────────────────┘
/// ```
fn setup_placeholder_ui(mut commands: Commands, theme: Res<ui::theme::Theme>) {
    // Root container - fills window, flex column layout
    commands
        .spawn((
            Node {
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                flex_direction: FlexDirection::Column,
                ..default()
            },
            // Transparent - let camera clear color show through
        ))
        .with_children(|root| {
            // ═══════════════════════════════════════════════════════════════
            // HEADER SECTION
            // ═══════════════════════════════════════════════════════════════
            root.spawn((
                HeaderContainer,
                Node {
                    width: Val::Percent(100.0),
                    flex_direction: FlexDirection::Row,
                    justify_content: JustifyContent::SpaceBetween,
                    align_items: AlignItems::Center,
                    padding: UiRect::all(Val::Px(16.0)),
                    border: UiRect::bottom(Val::Px(1.0)),
                    ..default()
                },
                BorderColor::all(theme.border),
            ))
            .with_children(|header| {
                // Left: Title (uses glyphon for CJK support)
                header.spawn((
                    text::GlyphonUiText::new("会術 Kaijutsu")
                        .with_font_size(24.0)
                        .with_color(theme.accent),
                    text::UiTextPositionCache::default(),
                    Node {
                        min_width: Val::Px(180.0),
                        min_height: Val::Px(30.0),
                        ..default()
                    },
                ));

                // Right: Connection status (uses glyphon)
                header.spawn((
                    StatusText,
                    text::GlyphonUiText::new("Connecting...")
                        .with_font_size(14.0)
                        .with_color(theme.fg_dim),
                    text::UiTextPositionCache::default(),
                    Node {
                        min_width: Val::Px(250.0),
                        min_height: Val::Px(20.0),
                        ..default()
                    },
                ));
            });

            // ═══════════════════════════════════════════════════════════════
            // CONVERSATION CONTAINER (scrollable)
            // ═══════════════════════════════════════════════════════════════
            root.spawn((
                cell::ConversationContainer,
                Node {
                    flex_grow: 1.0,
                    flex_direction: FlexDirection::Column,
                    overflow: Overflow::scroll_y(),
                    padding: UiRect::axes(Val::Px(20.0), Val::Px(12.0)),
                    ..default()
                },
            ));

            // ═══════════════════════════════════════════════════════════════
            // PROMPT CONTAINER (fixed at bottom)
            // ═══════════════════════════════════════════════════════════════
            root.spawn((
                cell::PromptContainer,
                Node {
                    width: Val::Percent(100.0),
                    min_height: Val::Px(70.0),
                    max_height: Val::Px(150.0),
                    flex_direction: FlexDirection::Row,
                    justify_content: JustifyContent::FlexEnd,
                    align_items: AlignItems::FlexStart,
                    padding: UiRect::new(Val::Px(12.0), Val::Px(12.0), Val::Px(8.0), Val::Px(4.0)),
                    border: UiRect::top(Val::Px(1.0)),
                    ..default()
                },
                BorderColor::all(theme.border),
            ))
            .with_children(|prompt_area| {
                // Subtle hint text aligned to the right (uses glyphon)
                prompt_area.spawn((
                    PromptHint,
                    text::GlyphonUiText::new("'i' to type")
                        .with_font_size(11.0)
                        .with_color(Color::srgba(0.4, 0.4, 0.4, 0.6)), // Very dim
                    text::UiTextPositionCache::default(),
                    Node {
                        min_width: Val::Px(80.0),
                        min_height: Val::Px(16.0),
                        ..default()
                    },
                ));
            });

            // ═══════════════════════════════════════════════════════════════
            // STATUS BAR (bottom)
            // ═══════════════════════════════════════════════════════════════
            root.spawn((
                Node {
                    width: Val::Percent(100.0),
                    flex_direction: FlexDirection::Row,
                    justify_content: JustifyContent::SpaceBetween,
                    padding: UiRect::axes(Val::Px(12.0), Val::Px(4.0)),
                    ..default()
                },
                BackgroundColor(theme.panel_bg),
            ))
            .with_children(|status_bar| {
                // Left: Mode indicator placeholder (actual mode indicator is in ui::mode_indicator)
                // Empty node for layout purposes - flex_grow pushes key hints to the right
                status_bar.spawn(Node {
                    flex_grow: 1.0,
                    ..default()
                });

                // Right: Key hints (uses glyphon)
                status_bar.spawn((
                    text::GlyphonUiText::new("Enter: submit │ Shift+Enter: newline │ Esc: normal mode")
                        .with_font_size(11.0)
                        .with_color(theme.fg_dim),
                    text::UiTextPositionCache::default(),
                    Node {
                        min_width: Val::Px(450.0),
                        min_height: Val::Px(16.0),
                        ..default()
                    },
                ));
            });
        });
}

/// Marker for the prompt hint text (shows when prompt is empty).
#[derive(Component)]
struct PromptHint;

/// Marker for status text
#[derive(Component)]
struct StatusText;

/// Marker for the header container (used by dashboard to attach seat selector)
#[derive(Component)]
pub struct HeaderContainer;

/// Convert connection events to UI updates
fn handle_connection_events(
    mut conn_events: MessageReader<connection::ConnectionEvent>,
    mut status_text: Query<&mut text::GlyphonUiText, With<StatusText>>,
    theme: Res<ui::theme::Theme>,
) {
    use connection::ConnectionEvent;

    for event in conn_events.read() {
        for mut ui_text in status_text.iter_mut() {
            match event {
                ConnectionEvent::Connected => {
                    ui_text.text = "✓ Connected to server".into();
                    ui_text.color = text::bevy_to_glyphon_color(theme.row_result);
                }
                ConnectionEvent::Disconnected => {
                    ui_text.text = "⚡ Disconnected (reconnecting...)".into();
                    ui_text.color = text::bevy_to_glyphon_color(theme.row_tool);
                }
                ConnectionEvent::ConnectionFailed(err) => {
                    ui_text.text = format!("✗ {}", err);
                    ui_text.color = text::bevy_to_glyphon_color(theme.accent2);
                }
                ConnectionEvent::Reconnecting { attempt, .. } => {
                    ui_text.text = format!("⟳ Reconnecting (attempt {})...", attempt);
                    ui_text.color = text::bevy_to_glyphon_color(theme.fg_dim);
                }
                ConnectionEvent::AttachedKernel(info) => {
                    ui_text.text = format!("✓ Attached to kernel: {}", info.name);
                    ui_text.color = text::bevy_to_glyphon_color(theme.row_result);
                }
                _ => {}
            }
        }
    }
}
