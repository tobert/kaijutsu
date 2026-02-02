//! Kaijutsu App - Cell-based collaborative workspace
//!
//! A fresh implementation with cells as the universal primitive.
//! CRDT sync via diamond-types, cosmic-text rendering.
//!
//! ## UI Architecture
//!
//! The UI uses Bevy's state system for screen transitions:
//! - `AppScreen::Dashboard` - Kernel/Context/Seat selection
//! - `AppScreen::Conversation` - Active conversation view
//!
//! Chrome (header, status bar) is always visible. Content area switches
//! between views using `Display::None` for efficient layout.

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
mod kaish;
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

    // Load theme from ~/.config/kaijutsu/theme.rhai (or use defaults)
    let theme = ui::theme_loader::load_theme();

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
        // App screen state management (Dashboard vs Conversation)
        .add_plugins(ui::state::AppScreenPlugin)
        // Dashboard/lobby experience
        .add_plugins(dashboard::DashboardPlugin)
        // Commands (vim-style : commands)
        .add_plugins(commands::CommandsPlugin)
        // Constellation - context navigation as visual node graph
        .add_plugins(ui::constellation::ConstellationPlugin)
        // HUD system - configurable overlay panels
        .add_plugins(ui::hud::HudPlugin)
        // Layout system - RON-driven view layouts
        .add_plugins(ui::layout::LayoutPlugin)
        // Animation tweening for smooth mode transitions
        .add_plugins(bevy_tweening::TweeningPlugin)
        // Resources - theme loaded from ~/.config/kaijutsu/theme.rhai
        .insert_resource(theme)
        // Startup
        // MaterialCache must run early so panel builders can access materials
        .add_systems(Startup, (
            setup_camera,
            ui::materials::setup_material_cache,
            setup_ui,
            setup_input_layer,
            ui::debug::setup_debug_overlay,
        ).chain())
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

/// Set up the main UI layout with state-driven screens.
///
/// ## Architecture
///
/// ```text
/// Z-LAYER 0: CHROME (always visible)
/// ┌─────────────────────────────────────────────────────┐
/// │ 会術 Kaijutsu    [status]           [Seat Selector] │  ← Header
/// ├─────────────────────────────────────────────────────┤
/// │ Z-LAYER 10: CONTENT AREA (state-driven)             │
/// │                                                     │
/// │   ┌─ AppScreen::Dashboard ────────────────────────┐ │
/// │   │ KERNELS │ CONTEXTS │ YOUR SEATS               │ │
/// │   │ [lobby] │ [default]│                          │ │
/// │   └───────────────────────────────────────────────┘ │
/// │                                                     │
/// │   ┌─ AppScreen::Conversation ─────────────────────┐ │
/// │   │ (scrollable conversation messages)            │ │
/// │   │ ───────────────────────────────────────────── │ │
/// │   │ [Prompt input area]                           │ │
/// │   └───────────────────────────────────────────────┘ │
/// │                                                     │
/// ├─────────────────────────────────────────────────────┤
/// │ [NORMAL]               Enter: submit │ Esc: normal │  ← Status bar
/// └─────────────────────────────────────────────────────┘
///
/// Z-LAYER 100: MODALS (seat dropdown, command palette)
/// ```
fn setup_ui(
    mut commands: Commands,
    theme: Res<ui::theme::Theme>,
    mut text_glow_materials: ResMut<Assets<shaders::TextGlowMaterial>>,
) {
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
            // CHROME: HEADER (always visible, Z-LAYER 0)
            // ═══════════════════════════════════════════════════════════════
            root.spawn((
                HeaderContainer,
                Node {
                    width: Val::Percent(100.0),
                    flex_direction: FlexDirection::Row,
                    justify_content: JustifyContent::SpaceBetween,
                    align_items: AlignItems::Center,
                    padding: UiRect::axes(Val::Px(16.0), Val::Px(6.0)), // Tighter vertical
                    border: UiRect::bottom(Val::Px(1.0)),
                    ..default()
                },
                BorderColor::all(theme.border),
            ))
            .with_children(|header| {
                // Left: Title with icy sheen plane underneath (absolute positioned)
                header
                    .spawn(Node {
                        min_width: Val::Px(180.0),
                        min_height: Val::Px(36.0),
                        ..default()
                    })
                    .with_children(|title_container| {
                        // Title text (normal flow)
                        title_container.spawn((
                            text::MsdfUiText::new("会術 Kaijutsu")
                                .with_font_size(24.0)
                                .with_color(theme.accent),
                            text::UiTextPositionCache::default(),
                            Node::default(),
                        ));

                        // Icy sheen plane - absolute positioned below text
                        title_container.spawn((
                            MaterialNode(text_glow_materials.add(
                                shaders::TextGlowMaterial::icy_sheen(theme.accent),
                            )),
                            Node {
                                position_type: PositionType::Absolute,
                                bottom: Val::Px(0.0),
                                left: Val::Px(0.0),
                                width: Val::Percent(100.0),
                                height: Val::Px(6.0),
                                ..default()
                            },
                        ));
                    });

                // Right: Connection status (uses glyphon)
                header.spawn((
                    StatusText,
                    text::MsdfUiText::new("Connecting...")
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
            // CONTENT AREA (state-driven, Z-LAYER 10)
            // Contains both Dashboard and Conversation views
            // ═══════════════════════════════════════════════════════════════
            root.spawn((
                ui::state::ContentArea,
                Node {
                    flex_grow: 1.0,
                    flex_direction: FlexDirection::Column,
                    ..default()
                },
                ZIndex(constants::ZLayer::CONTENT),
            ))
            .with_children(|content| {
                // ───────────────────────────────────────────────────────────
                // DASHBOARD VIEW (visible by default)
                // Children (KernelList, ContextList, etc) spawned by layout system
                // ───────────────────────────────────────────────────────────
                content.spawn((
                    dashboard::DashboardRoot,
                    Node {
                        width: Val::Percent(100.0),
                        height: Val::Percent(100.0),
                        flex_direction: FlexDirection::Column,
                        display: Display::Flex, // Visible by default (Dashboard is initial state)
                        ..default()
                    },
                    BackgroundColor(theme.bg),
                    Visibility::Inherited, // Visible by default
                ));

                // ───────────────────────────────────────────────────────────
                // CONVERSATION VIEW (hidden when in Dashboard state)
                // Children (DagView, InputShadow) spawned by layout system
                // ───────────────────────────────────────────────────────────
                content.spawn((
                    ui::state::ConversationRoot,
                    Node {
                        width: Val::Percent(100.0),
                        height: Val::Percent(100.0),
                        flex_direction: FlexDirection::Column,
                        display: Display::None, // Hidden by default, shown via state transition
                        ..default()
                    },
                    Visibility::Hidden, // Hidden by default (glyphon needs this too)
                ));
            });

            // ═══════════════════════════════════════════════════════════════
            // CHROME: STATUS BAR (always visible, Z-LAYER 0)
            // Contains mode indicator integrated as flex child
            // ═══════════════════════════════════════════════════════════════
            root.spawn((
                ui::state::StatusBar,
                Node {
                    width: Val::Percent(100.0),
                    flex_direction: FlexDirection::Row,
                    justify_content: JustifyContent::SpaceBetween,
                    align_items: AlignItems::Center,
                    padding: UiRect::axes(Val::Px(12.0), Val::Px(4.0)),
                    ..default()
                },
                BackgroundColor(theme.panel_bg),
            ))
            .with_children(|status_bar| {
                // Left: Mode indicator (spawned as flex child, not absolute)
                status_bar.spawn((
                    ui::mode_indicator::ModeIndicator,
                    text::MsdfUiText::new("NORMAL")
                        .with_font_size(14.0)
                        .with_color(theme.fg_dim),
                    text::UiTextPositionCache::default(),
                    Node {
                        padding: UiRect::all(Val::Px(8.0)),
                        min_width: Val::Px(80.0),
                        min_height: Val::Px(20.0),
                        ..default()
                    },
                    BackgroundColor(theme.panel_bg),
                ));

                // Spacer
                status_bar.spawn(Node {
                    flex_grow: 1.0,
                    ..default()
                });

                // Right: Key hints
                status_bar.spawn((
                    text::MsdfUiText::new("Enter: submit │ Shift+Enter: newline │ Esc: normal mode")
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

/// Spawn the InputLayer - world-level floating container for the input area.
///
/// The InputLayer floats over the InputShadow and contains:
/// - Backdrop (dim overlay, visible when presence=Overlay)
/// - Input frame content (the 9-slice frame and text are positioned here)
///
/// This is spawned at ZIndex(100) so it floats above all conversation content.
fn setup_input_layer(
    mut commands: Commands,
    theme: Res<ui::theme::Theme>,
) {
    // Spawn the InputLayer at world level (not parented to any UI tree)
    commands.spawn((
        ui::state::InputLayer,
        Node {
            position_type: PositionType::Absolute,
            // Position will be updated by compute_input_position system
            left: Val::Px(0.0),
            top: Val::Px(0.0),
            width: Val::Percent(100.0),
            height: Val::Percent(100.0),
            // No layout - children use absolute positioning
            ..default()
        },
        ZIndex(constants::ZLayer::MODAL),
        // Start hidden - visibility controlled by InputPresence
        Visibility::Hidden,
    ))
    .with_children(|layer| {
        // Backdrop - dim overlay behind centered input (only visible in Overlay mode)
        layer.spawn((
            ui::state::InputBackdrop,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(0.0),
                top: Val::Px(0.0),
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                ..default()
            },
            BackgroundColor(theme.input_backdrop_color),
            Visibility::Hidden, // Toggled by sync_backdrop_visibility
        ));

        // InputFrame container - the 9-slice frame pieces are spawned as children
        // by the frame_assembly system when it sees an InputFrame marker
        layer.spawn((
            ui::state::InputFrame,
            Node {
                position_type: PositionType::Absolute,
                // Position/size updated by apply_input_position system
                ..default()
            },
            // No background - frame pieces render the border
        ));
    });
}


/// Marker for status text
#[derive(Component)]
struct StatusText;

/// Marker for the header container (used by dashboard to attach seat selector)
#[derive(Component)]
pub struct HeaderContainer;

/// Convert connection events to UI updates
fn handle_connection_events(
    mut conn_events: MessageReader<connection::ConnectionEvent>,
    mut status_text: Query<&mut text::MsdfUiText, With<StatusText>>,
    theme: Res<ui::theme::Theme>,
) {
    use connection::ConnectionEvent;

    for event in conn_events.read() {
        for mut ui_text in status_text.iter_mut() {
            match event {
                ConnectionEvent::Connected => {
                    ui_text.text = "✓ Connected to server".into();
                    ui_text.color = text::bevy_to_rgba8(theme.row_result);
                }
                ConnectionEvent::Disconnected => {
                    ui_text.text = "⚡ Disconnected (reconnecting...)".into();
                    ui_text.color = text::bevy_to_rgba8(theme.row_tool);
                }
                ConnectionEvent::ConnectionFailed(err) => {
                    ui_text.text = format!("✗ {}", err);
                    ui_text.color = text::bevy_to_rgba8(theme.accent2);
                }
                ConnectionEvent::Reconnecting { attempt, .. } => {
                    ui_text.text = format!("⟳ Reconnecting (attempt {})...", attempt);
                    ui_text.color = text::bevy_to_rgba8(theme.fg_dim);
                }
                ConnectionEvent::AttachedKernel(info) => {
                    ui_text.text = format!("✓ Attached to kernel: {}", info.name);
                    ui_text.color = text::bevy_to_rgba8(theme.row_result);
                }
                _ => {}
            }
        }
    }
}
