//! Kaijutsu App - Cell-based collaborative workspace
//!
//! A fresh implementation with cells as the universal primitive.
//! CRDT sync via diamond-types, Vello text rendering.
//!
//! ## UI Architecture
//!
//! The app starts directly in the tiling conversation view.
//! Chrome is handled by the tiling WM system (North/South docks).
//! Connection + context join happens in the background (ActorPlugin bootstrap).

// Bevy ECS idioms that trigger these lints
#![allow(clippy::too_many_arguments)]
#![allow(clippy::type_complexity)]

use bevy::picking::mesh_picking::{MeshPickingPlugin, MeshPickingSettings};
use bevy::prelude::*;
use bevy::window::{Monitor, MonitorSelection, PrimaryMonitor, PrimaryWindow, WindowPosition};
use bevy_brp_extras::BrpExtrasPlugin;
use clap::Parser;
use kaijutsu_client::SshConfig;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

/// 会術 Kaijutsu — collaborative workspace
#[derive(Parser, Debug)]
#[command(name = "kaijutsu", version)]
struct Cli {
    /// Server host to connect to
    #[arg(long, default_value = kaijutsu_client::constants::DEFAULT_SSH_HOST)]
    host: String,

    /// Server SSH port
    #[arg(long, default_value_t = kaijutsu_client::constants::DEFAULT_SSH_PORT)]
    port: u16,

    /// Skip SSH known_hosts verification (TOFU)
    #[arg(long)]
    insecure: bool,
}

mod agents;
mod cell;
mod commands;
mod config;
mod connection;
mod constants;
mod input;
mod kaish;
mod shaders;
mod text;
mod ui;
mod view;

// Re-export client crate's generated code
pub use kaijutsu_client::kaijutsu_capnp;

fn main() {
    let cli = Cli::parse();

    let ssh_config = SshConfig {
        host: cli.host,
        port: cli.port,
        insecure: cli.insecure,
        ..SshConfig::default()
    };

    // Set up file logging
    let log_dir = std::env::var("KAIJUTSU_LOG_DIR").unwrap_or_else(|_| "/tmp".to_string());
    let file_appender = tracing_appender::rolling::never(&log_dir, "kaijutsu-app.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    let registry = tracing_subscriber::registry()
        .with(EnvFilter::new(
            "warn,kaijutsu_app=debug,kaijutsu_client=debug",
        ))
        .with(fmt::layer().with_writer(non_blocking).with_ansi(false))
        .with(fmt::layer().with_writer(std::io::stderr));

    let _otel_guard = if kaijutsu_telemetry::otel_enabled() {
        let (otel_layer, guard) = kaijutsu_telemetry::otel_layer("kaijutsu-app");
        registry.with(otel_layer).init();
        Some(guard)
    } else {
        registry.init();
        None
    };

    info!(
        "Starting Kaijutsu App - logging to {}/kaijutsu-app.log",
        log_dir
    );

    // Load theme and bindings from ~/.config/kaijutsu/ (or use defaults)
    let app_config = config::load_app_config();
    let theme = app_config.theme;

    App::new()
        .add_plugins(
            DefaultPlugins
                .set(WindowPlugin {
                    primary_window: Some(Window {
                        title: "会術 Kaijutsu".into(),
                        resolution: (
                            constants::INITIAL_WINDOW_WIDTH,
                            constants::INITIAL_WINDOW_HEIGHT,
                        )
                            .into(),
                        position: WindowPosition::Centered(MonitorSelection::Primary),
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
                .disable::<bevy::log::LogPlugin>(),
        )
        // 3D mesh picking for constellation node clicks
        .add_plugins(MeshPickingPlugin)
        .insert_resource(MeshPickingSettings {
            require_markers: true,
            ..default()
        })
        // Remote debugging (BRP) - BrpExtrasPlugin includes RemotePlugin
        .add_plugins(BrpExtrasPlugin)
        // Text rendering (Vello vector graphics)
        .add_plugins(text::KjTextPlugin)
        // Focus-based input dispatch (Phase 1: emits alongside old handlers)
        .add_plugins(input::InputPlugin)
        // Cell editing
        .add_plugins(cell::CellPlugin)
        // Per-block Vello texture rendering
        .add_plugins(view::block_render::BlockRenderPlugin)
        // Agent attachment and collaboration
        .add_plugins(agents::AgentsPlugin)
        // Shader effects
        .add_plugins(shaders::ShaderFxPlugin)
        // Connection plugin (spawns background thread)
        .add_plugins(connection::ActorPlugin { ssh_config })
        // App screen state management
        .add_plugins(ui::state::AppScreenPlugin)
        // Screen state machine (Constellation/Conversation/ForkForm transitions)
        .add_plugins(ui::screen::ScreenPlugin)
        // Commands (vim-style : commands)
        .add_plugins(commands::CommandsPlugin)
        // Constellation - context navigation as visual node graph
        .add_plugins(ui::constellation::ConstellationPlugin)
        // Conversation Stack — 3D cascading card view
        .add_plugins(ui::card_stack::CardStackPlugin)
        // Tiling WM — layout tree, reconciler, and widget update systems
        .add_plugins(ui::tiling::TilingPlugin)
        .add_plugins(ui::tiling_reconciler::TilingReconcilerPlugin)
        .add_plugins(ui::dock::DockPlugin)
        // Drift state - context list + staged queue polling
        .add_plugins(ui::drift::DriftPlugin)
        // Timeline navigation - temporal scrubbing through history
        .add_plugins(ui::timeline::TimelinePlugin)
        // Animation tweening for smooth mode transitions
        .add_plugins(bevy_tweening::TweeningPlugin)
        // Resources - theme loaded from ~/.config/kaijutsu/theme.rhai
        .insert_resource(theme)
        // Startup
        .add_systems(
            Startup,
            (setup_camera, setup_ui, ui::debug::setup_debug_overlay).chain(),
        )
        // Adapt window to monitor on first frame (Monitor not available at Startup)
        .add_systems(Update, adapt_window_to_monitor)
        // Update
        // NOTE: handle_debug_toggle, handle_screenshot, handle_quit
        // migrated to input::systems — they consume ActionFired now
        .run();
}

/// Setup 2D camera for UI.
///
/// `VelloView` is required for bevy_vello to render text and scenes on this camera.
fn setup_camera(mut commands: Commands, theme: Res<ui::theme::Theme>) {
    commands.spawn((
        Camera2d,
        Camera {
            clear_color: ClearColorConfig::Custom(theme.bg),
            ..default()
        },
        bevy_vello::render::VelloView,
    ));
}

/// Resize window to fit the primary monitor on the first frame.
///
/// Bevy's `Monitor` entities aren't populated at `Startup` (winit hasn't pumped
/// events yet), so this runs in `Update` with a `Local<bool>` guard. Computes
/// 75%×80% of the monitor's logical resolution and resizes the window.
fn adapt_window_to_monitor(
    mut window_q: Query<&mut Window, With<PrimaryWindow>>,
    monitor_q: Query<&Monitor, With<PrimaryMonitor>>,
    mut done: Local<bool>,
) {
    if *done {
        return;
    }
    let Ok(monitor) = monitor_q.single() else {
        return;
    };
    let Ok(mut window) = window_q.single_mut() else {
        return;
    };

    let logical_w = monitor.physical_width as f32 / monitor.scale_factor as f32;
    let logical_h = monitor.physical_height as f32 / monitor.scale_factor as f32;

    let w = logical_w * constants::WINDOW_WIDTH_FRACTION;
    let h = logical_h * constants::WINDOW_HEIGHT_FRACTION;

    info!(
        "Adapting window to monitor: {}x{} physical, {:.0}x scale → {:.0}x{:.0} logical → {:.0}x{:.0} window",
        monitor.physical_width, monitor.physical_height, monitor.scale_factor,
        logical_w, logical_h, w, h,
    );

    window.resolution.set(w, h);
    window.position.center(MonitorSelection::Primary);
    *done = true;
}

/// Set up the structural UI skeleton.
///
/// Docks are spawned by `DockPlugin` (PostStartup). The tiling reconciler
/// populates conversation content within the content area.
///
/// ```text
/// TilingRoot (column, 100%x100%)
///   [NorthDock — spawned by DockPlugin]
///   ContentArea (column, flex-grow: 1)
///     ConversationRoot (100%, visible immediately)
///       [ConversationContainer — spawned by tiling reconciler]
///   [SouthDock — spawned by DockPlugin]
/// ```
fn setup_ui(mut commands: Commands) {
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
            // CONTENT AREA
            // Docks are inserted before/after this by the reconciler
            // ═══════════════════════════════════════════════════════════════
            root.spawn((
                ui::state::ContentArea,
                Node {
                    flex_grow: 1.0,
                    height: Val::Percent(100.0),
                    flex_direction: FlexDirection::Column,
                    overflow: Overflow::clip(), // Hard boundary for all children
                    ..default()
                },
                ZIndex(constants::ZLayer::CONTENT),
            ))
            .with_children(|content| {
                // CONVERSATION VIEW (hidden initially — Constellation is default)
                // The tiling reconciler spawns ConversationContainer inside
                content.spawn((
                    ui::state::ConversationRoot,
                    Node {
                        width: Val::Percent(100.0),
                        flex_grow: 1.0, // Participate in flex layout properly
                        flex_direction: FlexDirection::Column,
                        ..default()
                    },
                    Visibility::Hidden,
                ));
            });
        });
}
