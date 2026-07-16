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
use bevy::winit::{UpdateMode, WinitSettings};
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

    /// Share a local directory into `/r/<client-id>/<name>` (repeatable).
    /// Format: `[name=]path[:rw]` — the name defaults to the path's
    /// basename; `:rw` labels the share read-write in the manifest (write
    /// support itself is a later slice — every share is served read-only
    /// today regardless of this flag). See `docs/slash-r.md`.
    #[arg(long = "share", action = clap::ArgAction::Append, value_parser = kaijutsu_client::parse_share_arg)]
    shares: Vec<kaijutsu_client::ShareArg>,
}

mod audio;
mod audio_sched;
mod cell;
mod commands;
mod config;
mod connection;
mod constants;
mod input;
mod kaish;
mod metronome;
mod midi;
mod midi_in;
mod patch_graph;
mod peers;
mod shaders;
mod text;
mod ui;
mod view;

// Re-export client crate's generated code
pub use kaijutsu_client::kaijutsu_capnp;

fn main() {
    let cli = Cli::parse();

    // Cross-arg validation clap's per-value parser can't express: two shares
    // defaulting to the same name (`--share a/x --share b/x`) must be a
    // parse error, not a silent shadow (docs/slash-r.md "Open questions").
    if let Err(e) = kaijutsu_client::validate_unique_names(&cli.shares) {
        use clap::CommandFactory;
        Cli::command()
            .error(clap::error::ErrorKind::ValueValidation, e)
            .exit();
    }

    let ssh_config = SshConfig {
        host: cli.host,
        port: cli.port,
        insecure: cli.insecure,
        ..SshConfig::default()
    };

    // Stable per-installation client id (docs/kernel-kv.md), loaded once and
    // reused both as the `ClientId` resource below and (if shares are
    // configured) as the `/r` share manifest's claimed identity
    // (docs/slash-r.md — "namespace, not authority": the SSH principal, not
    // this string, is what the kernel actually trusts).
    let client_id = connection::client_id::load_or_seed();

    // Build the share-server config eagerly, before the window even opens:
    // a bad `--share` path (doesn't exist, not a directory) is a startup
    // error, not a silently-dropped share discovered later from an empty
    // `/r/<id>` (fail loud, docs/slash-r.md).
    let share_config = if cli.shares.is_empty() {
        None
    } else {
        let nick = hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(whoami::username);
        match kaijutsu_client::ShareServerConfig::new(&cli.shares, client_id.to_string(), nick) {
            Ok(config) => Some(config),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(2);
            }
        }
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

    // Load theme and bindings from ~/.config/kaijutsu/ (or use defaults).
    // Errors (bad TOML, unknown action/key tokens) are already logged and
    // collected into app_config.errors, which is inserted as a resource
    // below so a startup system can surface them via GlobalErrorQueue.
    let app_config = config::load_app_config();
    let theme = app_config.theme;
    let startup_errors = config::StartupConfigErrors(app_config.errors);

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
                .disable::<bevy::log::LogPlugin>()
                // The app owns rodio directly now (docs/pcm.md R5 — audio_sched.rs's
                // dedicated scheduler thread); bevy_audio never opens a device.
                .disable::<bevy::audio::AudioPlugin>(),
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
        // Offscreen vello rasterizer (kaijutsu-owned)
        .add_plugins(view::vello_rasterizer::VelloRasterizerPlugin)
        // Generic vello-scene → UI texture primitive
        .add_plugins(view::ui_rtt::UiRttPlugin)
        // Per-block Vello texture rendering
        .add_plugins(view::block_render::BlockRenderPlugin)
        // Peer transport (drift navigation: kernel → app invocations)
        .add_plugins(peers::PeersPlugin)
        // Shader effects
        .add_plugins(shaders::ShaderFxPlugin)
        // Stable per-installation client id, for per-client KV namespacing
        // (docs/kernel-kv.md). Seeded before the connection plugin's systems run.
        .insert_resource(connection::client_id::ClientId(client_id))
        // Connection plugin (spawns background thread)
        .add_plugins(connection::ActorPlugin { ssh_config: ssh_config.clone() })
        // /r client shares (docs/slash-r.md): dials the kaijutsu-share
        // subsystem on (re)connect and serves any --share directories. A
        // no-op plugin when no --share flag was given.
        .add_plugins(connection::ShareDialPlugin { ssh_config, share_config })
        // Render sinks (docs/pcm.md, docs/midi.md): ServerEvent::RenderCue,
        // dispatched by mime — audio/* → AudioPlayer, text/vnd.abc → ALSA MIDI.
        .add_plugins(audio::AudioOutPlugin)
        .add_plugins(midi::MidiOutPlugin)
        // The ear (docs/midi.md M2): device MIDI → ring → windowed batches →
        // commitCapture, landing as data-only cells on the current context's track.
        .add_plugins(midi_in::MidiInPlugin)
        // The metronome: the app's continuous local timebase made audible —
        // a phasor slaved to ServerEvent::BeatSync, clicking through midi's port.
        .add_plugins(metronome::MetronomePlugin)
        // App screen state management
        .add_plugins(ui::state::AppScreenPlugin)
        // Screen state machine (single Conversation screen)
        .add_plugins(ui::screen::ScreenPlugin)
        // Commands (vim-style : commands)
        .add_plugins(commands::CommandsPlugin)
        // Tiling WM — layout tree, reconciler, and widget update systems
        .add_plugins(ui::tiling::TilingPlugin)
        .add_plugins(ui::tiling_reconciler::TilingReconcilerPlugin)
        .add_plugins(ui::dock::DockPlugin)
        // Drift state - context list + staged queue polling
        .add_plugins(ui::drift::DriftPlugin)
        // Room level + patch bay station + time well (docs/scenes/): dive into
        // a zoomed station via `RoomState::zoomed`, Ctrl+W to jump straight
        // into the well. RoomPlugin MUST be added before any zoomable
        // station's plugin: `room_keyboard` early-returns on
        // `room.zoomed.is_some()`, and a zoomed station's own keyboard system
        // (e.g. `well_keyboard`/`patch_bay_keyboard`) clears `zoomed` on
        // Escape — if the station's plugin ran BEFORE RoomPlugin in the same
        // Update tick, `room_keyboard` would observe the just-cleared
        // `zoomed` in the SAME frame and immediately fire its own
        // Escape-to-Conversation branch too, skipping the room-overview stop
        // entirely (found live, BRP-driven: time-well/room integration
        // Slice C — `TimeWellPlugin` used to sit BEFORE `RoomPlugin`, which
        // was harmless while the well was its own `Screen::TimeWell` and
        // never ran in the same tick as `room_keyboard` at all; it stopped
        // being harmless the moment the well became a `RoomState::zoomed`
        // station like patch bay, whose plugin already relied on this order).
        .add_plugins(view::room::RoomPlugin)
        .add_plugins(view::patch_bay::PatchBayPlugin)
        // Time-well context browser (radial 3D well; Ctrl+W to enter, Esc to leave)
        .add_plugins(view::time_well::TimeWellPlugin)
        // Tracker station — the pattern-grid face at E (Tracker Station
        // slice 0, `snazzy-jumping-hejlsberg.md`).
        .add_plugins(view::tracker::TrackerPlugin)
        // The FSN landscape (VFS-as-terrain world; N-dive from the room, Esc
        // to surface) — a genuine `Screen::Fsn` transition, not a
        // `RoomState::zoomed` station, so it has no ordering dependency on
        // `RoomPlugin` the way the zoomable stations above do.
        .add_plugins(view::fsn::FsnPlugin)
        // In-app vi editor — screen/landing foundation (open_editor signal → Screen::Editor)
        .add_plugins(view::editor::EditorPlugin)
        // Timeline navigation - temporal scrubbing through history
        .add_plugins(ui::timeline::TimelinePlugin)
        // Animation tweening for smooth mode transitions
        .add_plugins(bevy_tweening::TweeningPlugin)
        // Resources - theme loaded from ~/.config/kaijutsu/theme.toml
        .insert_resource(theme)
        // The 3D scene lane's palette ([scene] in theme.toml, docs/color.md):
        // compiled defaults until the kernel's theme arrives over RPC.
        .init_resource::<view::scene_palette::ScenePalette>()
        // Startup config errors (drained into GlobalErrorQueue on first frame)
        .insert_resource(startup_errors)
        // Power management — sleep between events instead of spinning every vsync tick.
        // Input events (keyboard, mouse, window) wake immediately with zero added latency.
        .insert_resource(WinitSettings {
            focused_mode: UpdateMode::reactive(std::time::Duration::from_millis(100)), // 10Hz idle
            unfocused_mode: UpdateMode::reactive_low_power(std::time::Duration::from_millis(500)), // 2Hz background
        })
        // Startup
        .add_systems(
            Startup,
            (setup_camera, setup_ui, ui::debug::setup_debug_overlay).chain(),
        )
        // Drain config-load errors into the dock error HUD on first frame
        .add_systems(Update, config::drain_startup_errors)
        // [scene.post] hot-applies to the camera when the palette changes
        .add_systems(Update, view::scene_palette::apply_scene_post_on_change)
        // Adapt window to monitor on first frame (Monitor not available at Startup)
        .add_systems(Update, adapt_window_to_monitor)
        // Update
        // NOTE: handle_debug_toggle, handle_screenshot, handle_quit
        // migrated to input::systems — they consume ActionFired now
        .run();
}

/// Setup the single, always-on app camera.
///
/// It is a `Camera3d` (not `Camera2d`) so the time well's 3D card meshes and the
/// conversation UI share **one** camera — the well no longer spawns its own, and
/// the old two-camera composite (3D background + 2D overlay) is gone. Bevy UI
/// renders on whatever camera is the default UI target; with a single camera that
/// is this one (`IsDefaultUiCamera` makes it explicit), and the UI pass runs
/// *after* tonemapping/bloom, so conversation UI is untouched by either.
///
/// `Hdr` + `Bloom` make the 3D well cards glow (the main pass is tonemapped); the
/// conversation UI renders after tonemapping, so its colors are placeholder-only
/// and not a concern here (per Amy).
fn setup_camera(
    mut commands: Commands,
    theme: Res<ui::theme::Theme>,
    palette: Res<view::scene_palette::ScenePalette>,
) {
    use bevy::post_process::bloom::{Bloom, BloomPrefilter};
    use bevy::render::view::Hdr;
    commands.spawn((
        Camera3d::default(),
        Camera {
            clear_color: ClearColorConfig::Custom(theme.bg),
            ..default()
        },
        Hdr,
        // Thresholded additive bloom: only HDR (>1.0) pixels bloom, so the LDR card
        // bodies stay crisp and the well's HDR "bling" (selection rim, status
        // pulse — see `well_card.wgsl`) reads as a deliberate glow signal rather
        // than an all-over wash. (`OLD_SCHOOL` is already additive + thresholded;
        // we raise the threshold above the body brightness.) The low-frequency
        // shelf is pulled way down from the preset's 0.7/0.95: that shelf is the
        // wide sideways scatter — the "fuzzy halo" — and the well wants a tight
        // rim glow, not a fog. Intensity/boost + the tonemapper come from
        // `[scene.post]` in theme.toml (ScenePalette; hot-applies on theme
        // change — docs/color.md). The THRESHOLD stays a literal on purpose:
        // 1.0 is the HDR-tell boundary contract, not a style knob.
        Bloom {
            intensity: palette.bloom_intensity,
            low_frequency_boost: palette.bloom_low_frequency_boost,
            low_frequency_boost_curvature: 0.7,
            prefilter: BloomPrefilter {
                threshold: 1.0,
                threshold_softness: 0.3,
            },
            ..Bloom::OLD_SCHOOL
        },
        palette.tonemapper,
        IsDefaultUiCamera,
        Name::new("AppCamera"),
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
        monitor.physical_width,
        monitor.physical_height,
        monitor.scale_factor,
        logical_w,
        logical_h,
        w,
        h,
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
                // CONVERSATION VIEW (visible immediately — the only screen).
                // The tiling reconciler spawns ConversationContainer inside.
                content.spawn((
                    ui::state::ConversationRoot,
                    Node {
                        width: Val::Percent(100.0),
                        flex_grow: 1.0, // Participate in flex layout properly
                        flex_direction: FlexDirection::Column,
                        ..default()
                    },
                    Visibility::Inherited,
                ));
            });
        });
}
