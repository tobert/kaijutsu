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
//! Chrome is handled by the tiling WM system (North/South docks).
//! Content area switches between views using `Display::None`.

// Bevy ECS idioms that trigger these lints
#![allow(clippy::too_many_arguments)]
#![allow(clippy::type_complexity)]

use bevy::prelude::*;
use bevy_brp_extras::BrpExtrasPlugin;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

mod agents;
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

    let registry = tracing_subscriber::registry()
        .with(EnvFilter::new("warn,kaijutsu_app=debug,kaijutsu_client=debug"))
        .with(fmt::layer().with_writer(non_blocking).with_ansi(false))
        .with(fmt::layer().with_writer(std::io::stderr));

    #[cfg(feature = "telemetry")]
    let _otel_guard = if kaijutsu_telemetry::otel_enabled() {
        let (otel_layer, guard) = kaijutsu_telemetry::otel_layer("kaijutsu-app");
        registry.with(otel_layer).init();
        Some(guard)
    } else {
        registry.init();
        None
    };

    #[cfg(not(feature = "telemetry"))]
    registry.init();

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
        // Agent attachment and collaboration
        .add_plugins(agents::AgentsPlugin)
        // Shader effects
        .add_plugins(shaders::ShaderFxPlugin)
        // Connection plugin (spawns background thread)
        .add_plugins(connection::ActorPlugin)
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
        // Tiling WM — layout tree, reconciler, and widget update systems
        .add_plugins(ui::tiling::TilingPlugin)
        .add_plugins(ui::tiling_reconciler::TilingReconcilerPlugin)
        .add_plugins(ui::tiling_widgets::TilingWidgetsPlugin)
        // Drift state - context list + staged queue polling
        .add_plugins(ui::drift::DriftPlugin)
        // Layout system - RON-driven view layouts
        .add_plugins(ui::layout::LayoutPlugin)
        // Timeline navigation - temporal scrubbing through history
        .add_plugins(ui::timeline::TimelinePlugin)
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
            // NOTE: Legacy InputLayer disabled - ComposeBlock is the new inline input
            // setup_input_layer,
            ui::debug::setup_debug_overlay,
        ).chain())
        // Update
        .add_systems(Update, (
            ui::debug::handle_debug_toggle,
            ui::debug::handle_screenshot,
            ui::debug::handle_quit,
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

/// Set up the structural UI skeleton.
///
/// The tiling reconciler populates docks and conversation content.
/// This function spawns the fixed structure that the reconciler needs:
///
/// ```text
/// TilingRoot (column, 100%x100%)
///   [NorthDock — spawned by tiling reconciler]
///   ContentArea (column, flex-grow: 1)
///     DashboardRoot (100%, toggled by AppScreen state)
///     ConversationRoot (100%, toggled by AppScreen state)
///       [ConversationContainer — spawned by tiling reconciler]
///       [ComposeBlock — spawned by tiling reconciler]
///   [SouthDock — spawned by tiling reconciler]
/// ```
fn setup_ui(
    mut commands: Commands,
    theme: Res<ui::theme::Theme>,
) {
    // Root container — marked with TilingRoot for the reconciler to find.
    // Docks are inserted as children by the tiling reconciler.
    commands
        .spawn((
            ui::tiling_reconciler::TilingRoot,
            Node {
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                flex_direction: FlexDirection::Column,
                ..default()
            },
        ))
        .with_children(|root| {
            // ═══════════════════════════════════════════════════════════════
            // CONTENT AREA (state-driven)
            // Docks are inserted before/after this by the reconciler
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
                // DASHBOARD VIEW (visible by default)
                content.spawn((
                    dashboard::DashboardRoot,
                    Node {
                        width: Val::Percent(100.0),
                        height: Val::Percent(100.0),
                        flex_direction: FlexDirection::Column,
                        display: Display::Flex,
                        ..default()
                    },
                    BackgroundColor(theme.bg),
                    Visibility::Inherited,
                ));

                // CONVERSATION VIEW (hidden until AppScreen::Conversation)
                // The tiling reconciler spawns ConversationContainer + ComposeBlock inside
                content.spawn((
                    ui::state::ConversationRoot,
                    Node {
                        width: Val::Percent(100.0),
                        height: Val::Percent(100.0),
                        flex_direction: FlexDirection::Column,
                        display: Display::None,
                        ..default()
                    },
                    Visibility::Hidden,
                ));
            });

            // HEADER CONTAINER (attachment point for seat selector dropdown)
            root.spawn((
                HeaderContainer,
                Node {
                    width: Val::Percent(100.0),
                    height: Val::Px(0.0),
                    position_type: PositionType::Absolute,
                    top: Val::Px(6.0),
                    right: Val::Px(16.0),
                    flex_direction: FlexDirection::Row,
                    justify_content: JustifyContent::FlexEnd,
                    ..default()
                },
                ZIndex(constants::ZLayer::HUD + 1),
            ));
        });
}

/// Marker for the header container (used by dashboard to attach seat selector).
///
/// The actual header content (title, connection status) is now in the tiling system.
/// This component exists only as an attachment point for the seat selector dropdown.
#[derive(Component)]
pub struct HeaderContainer;
