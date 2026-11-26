//! Kaijutsu App - Cell-based collaborative workspace
//!
//! A fresh implementation with cells as the universal primitive.
//! Vello text rendering.
//!
//! ## UI Architecture
//!
//! The app starts directly in the tiling conversation view.
//! Chrome is handled by the tiling WM system (North/South docks).
//! Connection + context join happens in the background (ActorPlugin bootstrap).

// Bevy ECS idioms that trigger these lints: system functions take one
// argument per injected `Res`/`Query`/etc. (too_many_arguments), and query
// tuples/filters read as deeply nested generic types (type_complexity).
// Deliberately CRATE-WIDE (an inner `#![allow]` in the binary's root, not a
// per-function one): it silences every instance of both lints anywhere in
// this binary, not just the idiomatic ECS cases — a genuinely oversized
// argument list or an actually-too-complex type elsewhere in the app won't
// surface as a warning either. That breadth is the accepted tradeoff for
// not annotating every system function individually; a case that turns out
// to matter still needs eyes-on review, not a lint, to catch.
#![allow(clippy::too_many_arguments)]
#![allow(clippy::type_complexity)]

use bevy::picking::mesh_picking::{MeshPickingPlugin, MeshPickingSettings};
use bevy::prelude::*;
use bevy::window::{
    Monitor, MonitorSelection, PrimaryMonitor, PrimaryWindow, WindowMode, WindowPosition,
};
use bevy::winit::{UpdateMode, WinitSettings};
use bevy_brp_extras::BrpExtrasPlugin;
use clap::Parser;
use kaijutsu_client::{KeyArgs, SshConfig};
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

    #[command(flatten)]
    key: KeyArgs,

    /// Start borderless-fullscreen on the primary monitor (runner/gamescope)
    #[arg(long, conflicts_with = "maximize")]
    fullscreen: bool,

    /// Start with the window maximized
    #[arg(long)]
    maximize: bool,

    /// Open the window at this logical size (`WIDTHxHEIGHT`), skipping the
    /// fit-to-monitor resize. For a compositor that sends no size, such as
    /// gamescope's headless backend.
    #[arg(long, value_name = "WxH", value_parser = parse_window_size, conflicts_with_all = ["fullscreen", "maximize"])]
    window_size: Option<(u32, u32)>,

    /// Start with agent control on: an agent may fire actions over BRP
    /// (`kaijutsu/act`). Off by default; `Ctrl+A g` toggles it at runtime.
    #[arg(long)]
    agent_control: bool,

    /// Share a local directory into `/r/<client-id>/<name>` (repeatable).
    /// Format: `[name=]path[:rw]` — the name defaults to the path's
    /// basename; `:rw` labels the share read-write in the manifest (write
    /// support itself is a later slice — every share is served read-only
    /// today regardless of this flag). See `docs/slash-r.md`.
    #[arg(long = "share", action = clap::ArgAction::Append, value_parser = kaijutsu_client::parse_share_arg)]
    shares: Vec<kaijutsu_client::ShareArg>,
}

mod agent_control;
mod cell;
mod commands;
mod config;
mod connection;
mod constants;
mod input;
mod kaish;
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

    let key_source = match cli.key.key_source() {
        Ok(key_source) => key_source,
        Err(e) => {
            use clap::CommandFactory;
            Cli::command()
                .error(clap::error::ErrorKind::ValueValidation, e.to_string())
                .exit();
        }
    };
    let ssh_config = SshConfig {
        host: cli.host,
        port: cli.port,
        insecure: cli.insecure,
        key_source,
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

    // `icu_provider=error`: parley 0.9 line-breaks through icu_segmenter built
    // with `LineSegmenter::new_for_non_complex_scripts`, which loads no
    // dictionary for the unspaced scripts — so every layout containing Japanese
    // warns `ICU4X data error: No segmentation model ...: ja`, hundreds of lines
    // a minute with 会術 on screen. It is the *expected* result of parley's
    // configuration, not a fault we can fix from here.
    //
    // This line only bites because `kaijutsu-app`'s Cargo.toml takes a direct
    // `icu_provider` dependency solely to enable its `logging` feature — see the
    // comment there. Without that, icu's warning is a bare `eprintln!` in debug
    // builds and no filter can reach it. Dropped to `error` rather than `off` so
    // a real ICU data failure still surfaces.
    let registry = tracing_subscriber::registry()
        .with(EnvFilter::new(
            // sctk=info: inert with stock smithay-client-toolkit, but when
            // the parked hi-res-wheel fork is wired in (docs/issues.md
            // "Hi-res wheel") it logs the wl_seat bind version — the receipt
            // that says whether axis_value120 (v8+) can flow this session.
            "warn,kaijutsu_app=debug,kaijutsu_client=debug,sctk=info,icu_provider=error",
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
                        resolution: cli
                            .window_size
                            .unwrap_or((
                                constants::INITIAL_WINDOW_WIDTH,
                                constants::INITIAL_WINDOW_HEIGHT,
                            ))
                            .into(),
                        position: WindowPosition::Centered(MonitorSelection::Primary),
                        mode: if cli.fullscreen {
                            WindowMode::BorderlessFullscreen(MonitorSelection::Primary)
                        } else {
                            WindowMode::Windowed
                        },
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
        // Text rendering (Parley shaping + MSDF glyphs)
        .add_plugins(text::KjTextPlugin)
        // Focus-based input dispatch (Phase 1: emits alongside old handlers)
        .add_plugins(input::InputPlugin)
        .add_plugins(agent_control::AgentControlPlugin {
            enabled: cli.agent_control,
        })
        // Cell editing
        .add_plugins(cell::CellPlugin)
        // The conversation renderer (docs/conversation-surface.md): draws
        // block text/chrome/rich content directly into a per-pane RTT — no
        // Bevy UI node per block. Must come after CellPlugin — its
        // Update-schedule `.after`/`.before` anchors
        // (`sync_conversation_geometry`, `smooth_scroll`) are registered
        // there.
        .add_plugins(view::surface::ConversationSurfacePlugin)
        // MSDF texture rendering for the non-conversation surfaces (editor, diff, dock chrome)
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
        // The approval ledger's open asks, mirrored once for every
        // renderer that shows an ask. After ActorPlugin: its systems read
        // `RpcActor`, and the switchboard lamps and dock band both take
        // `Res<LedgerMirror>`.
        .add_plugins(connection::ledger::LedgerMirrorPlugin)
        // /r client shares (docs/slash-r.md): dials the kaijutsu-share
        // subsystem on (re)connect and serves any --share directories. A
        // no-op plugin when no --share flag was given.
        .add_plugins(connection::ShareDialPlugin { ssh_config, share_config })
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
        // The kernel's live roster (`/run/roster/index`) — MUST be added
        // before the surfaces that read it: DockPlugin's presence badge and
        // QuickContextPlugin's panel both take `Res<RosterFeed>`, and a
        // missing resource is a startup panic rather than a quiet blank.
        .add_plugins(connection::roster::RosterFeedPlugin)
        // The Ctrl+A prefix's own overlay — peeks while the prefix is armed,
        // `Ctrl+A h` holds it (docs/input.md).
        .add_plugins(ui::quick_context::QuickContextPlugin)
        // Approval review (docs/tui.md "Asks" and "The ledger"): the ask
        // sheet raises itself on an open ask in the context on screen that
        // this player may answer, and `Ctrl+A l` opens the ledger ribbon over
        // it. Both read `LedgerMirror` and send their decisions through it, so they
        // must come after LedgerMirrorPlugin; the dock's hints line reads
        // `AskNotice`, which AskSheetPlugin owns.
        .add_plugins(ui::msdf_panel::LayoutViewportPlugin)
        .add_plugins(ui::ask_sheet::AskSheetPlugin)
        .add_plugins(ui::ledger_ribbon::LedgerRibbonPlugin)
        // Drift state - context list + staged queue polling
        .add_plugins(connection::drift::DriftPlugin)
        // Attached-peer roster polling (kernel peer registry) — powers the
        // room's "seats at the table" wisps, one per connected client.
        .add_plugins(connection::peers::PeerRosterPlugin)
        // Room level + time well (docs/scenes/): dive into a zoomed station
        // via `RoomState::zoomed`, Ctrl+W to jump straight into the well.
        // RoomPlugin MUST be added before any zoomable station's plugin:
        // `room_keyboard` early-returns on `room.zoomed.is_some()`, and a
        // zoomed station's own keyboard system (`well_keyboard`) clears
        // `zoomed` on Escape — if the station's plugin ran BEFORE RoomPlugin
        // in the same Update tick, `room_keyboard` would observe the
        // just-cleared `zoomed` in the SAME frame and immediately fire its
        // own Escape-to-Conversation branch too, skipping the room-overview
        // stop entirely (found live, BRP-driven).
        .add_plugins(view::room::RoomPlugin)
        // Time-well context browser (radial 3D well; Ctrl+W to enter, Esc to leave)
        .add_plugins(view::time_well::TimeWellPlugin)
        // Full-window 3D heroic view (Ctrl+A k to enter, Esc to leave)
        .add_plugins(view::heroic::HeroicPlugin)
        .add_plugins(view::neo::NeoPlugin)
        // In-app vi editor — screen/landing foundation (open_editor signal → Screen::Editor)
        .add_plugins(view::editor::EditorPlugin)
        .add_plugins(view::diff_view::DiffViewPlugin)
        // Resources - theme loaded from ~/.config/kaijutsu/theme.toml
        .insert_resource(theme)
        .register_type::<ui::theme::Theme>()
        // The 3D scene lane's palette ([scene] in theme.toml, docs/color.md):
        // compiled defaults until the kernel's theme arrives over RPC.
        .init_resource::<view::scene_palette::ScenePalette>()
        .register_type::<view::scene_palette::ScenePalette>()
        // Startup config errors (drained into GlobalErrorQueue on first frame)
        .insert_resource(startup_errors)
        // Startup window-geometry choice (--fullscreen / --maximize); read by
        // adapt_window_to_monitor, which otherwise imposes its 75%×80% resize.
        .insert_resource(StartupWindowArgs {
            fullscreen: cli.fullscreen,
            maximize: cli.maximize,
            sized: cli.window_size.is_some(),
        })
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

/// The default UI camera draws the well and the conversation to the window.
/// Heroic and Room share this camera. Neo renders its cards to a UI image
/// with a separate camera. The window's UI pass runs after bloom and tonemapping.
fn setup_camera(
    mut commands: Commands,
    theme: Res<ui::theme::Theme>,
    palette: Res<view::scene_palette::ScenePalette>,
) {
    use bevy::post_process::bloom::{Bloom, BloomPrefilter};
    use bevy::camera::Hdr;
    commands.spawn((
        Camera3d::default(),
        Camera {
            clear_color: ClearColorConfig::Custom(theme.bg),
            ..default()
        },
        Hdr,
        // Thresholded additive bloom: only HDR (>1.0) pixels bloom, so the LDR card
        // bodies stay crisp and the well's HDR "bling" (selection rim, status
        // pulse — see `well_card.wesl`) reads as a deliberate glow signal rather
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

/// Startup window-geometry flags (`--fullscreen` / `--maximize`).
///
/// Fullscreen is applied at window construction (`WindowMode`); maximize has no
/// `Window` field and must be requested post-creation via `set_maximized`.
/// Either flag suppresses the default fit-to-monitor resize below.
#[derive(Resource)]
struct StartupWindowArgs {
    fullscreen: bool,
    maximize: bool,
    /// `--window-size` named the size; there is nothing to fit.
    sized: bool,
}

/// Parse `WIDTHxHEIGHT` (logical pixels, both above zero).
fn parse_window_size(s: &str) -> Result<(u32, u32), String> {
    let (w, h) = s
        .split_once('x')
        .ok_or_else(|| format!("'{s}' is not WIDTHxHEIGHT"))?;
    let dim = |v: &str| {
        v.parse::<u32>()
            .ok()
            .filter(|n| *n >= 1)
            .ok_or_else(|| format!("'{v}' is not a size of at least 1"))
    };
    Ok((dim(w)?, dim(h)?))
}

/// Resize window to fit the primary monitor on the first frame.
///
/// Bevy's `Monitor` entities aren't populated at `Startup` (winit hasn't pumped
/// events yet), so this runs in `Update` with a `Local<bool>` guard. Computes
/// 75%×80% of the monitor's logical resolution and resizes the window — unless
/// `--fullscreen`/`--maximize` asked for the whole screen (runner, gamescope).
fn adapt_window_to_monitor(
    mut window_q: Query<&mut Window, With<PrimaryWindow>>,
    monitor_q: Query<&Monitor, With<PrimaryMonitor>>,
    args: Res<StartupWindowArgs>,
    mut done: Local<bool>,
) {
    if *done {
        return;
    }
    if args.fullscreen || args.sized {
        // Mode or size was set at construction; nothing to adapt.
        *done = true;
        return;
    }
    if args.maximize {
        // Doesn't need Monitor data — request as soon as the window exists.
        if let Ok(mut window) = window_q.single_mut() {
            window.set_maximized(true);
            *done = true;
        }
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

#[cfg(test)]
mod layout_tests {
    use super::*;
    use bevy::app::{HierarchyPropagatePlugin, PropagateSet, TaskPoolPlugin};
    use bevy::camera::{ComputedCameraValues, RenderTargetInfo, Viewport};
    use bevy::text::{FontCx, RemSize, ScaleCx, TextPipeline};
    use bevy::ui::{
        ComputedUiRenderTargetInfo, ComputedUiTargetCamera, UiScale,
        sync_font_size_to_em_size, ui_layout_system, ui_surface::UiSurface,
        update::propagate_ui_target_cameras, widget::update_image_content_size_system,
    };

    fn layout_app(scale: f32) -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins(TaskPoolPlugin::default());
        app.add_plugins(HierarchyPropagatePlugin::<ComputedUiTargetCamera>::new(PostUpdate));
        app.add_plugins(HierarchyPropagatePlugin::<ComputedUiRenderTargetInfo>::new(PostUpdate));
        app.init_resource::<UiScale>();
        app.init_resource::<UiSurface>();
        app.init_resource::<TextPipeline>();
        app.init_resource::<FontCx>();
        app.init_resource::<ScaleCx>();
        app.init_resource::<RemSize>();
        app.init_resource::<ui::theme::Theme>();
        app.init_resource::<Assets<Image>>();
        app.init_resource::<Assets<TextureAtlasLayout>>();
        app.init_resource::<Assets<shaders::BlockFxMaterial>>();
        app.add_systems(Startup, setup_ui);
        app.add_systems(PostStartup, ui::dock::spawn_docks);
        app.add_systems(PostUpdate, (
            ApplyDeferred,
            propagate_ui_target_cameras,
            sync_font_size_to_em_size,
            update_image_content_size_system,
            ui_layout_system,
        ).chain());
        app.configure_sets(PostUpdate,
            PropagateSet::<ComputedUiTargetCamera>::default()
                .after(propagate_ui_target_cameras)
                .before(update_image_content_size_system));
        app.configure_sets(PostUpdate,
            PropagateSet::<ComputedUiRenderTargetInfo>::default()
                .after(propagate_ui_target_cameras)
                .before(update_image_content_size_system));

        let size = UVec2::new((2347.0 * scale) as u32, (1091.0 * scale) as u32);
        let camera = app.world_mut().spawn((Camera2d, Camera {
            computed: ComputedCameraValues {
                target_info: Some(RenderTargetInfo { physical_size: size, scale_factor: scale }),
                ..default()
            },
            viewport: Some(Viewport { physical_size: size, ..default() }),
            ..default()
        })).id();
        (app, camera)
    }

    #[test]
    fn root_splits_and_nested_rebuild_follow_their_directions() {
        use ui::tiling::{PaneId, PaneMarker, SplitDirection, TilingTree};
        use ui::tiling_reconciler::{ReconcilerState, reconcile_tiling_tree};

        fn position(app: &mut App, id: PaneId) -> Vec2 {
            let world = app.world_mut();
            let mut panes = world.query::<(&PaneMarker, &bevy::ui::UiGlobalTransform)>();
            panes.iter(world).find(|(marker, _)| marker.pane_id == id)
                .unwrap().1.to_scale_angle_translation().2
        }

        for direction in [SplitDirection::Row, SplitDirection::Column] {
            let (mut app, _) = layout_app(1.0);
            let mut tree = TilingTree::default_layout();
            let first = tree.focused;
            let second = tree.split(first, direction).unwrap();
            app.insert_resource(tree).init_resource::<ReconcilerState>();
            app.add_systems(Update, reconcile_tiling_tree);
            app.update();
            let (a, b) = (position(&mut app, first), position(&mut app, second));
            match direction {
                SplitDirection::Row => assert!(b.x > a.x && (b.y - a.y).abs() < 1.0, "row: {a:?}, {b:?}"),
                SplitDirection::Column => assert!(b.y > a.y && (b.x - a.x).abs() < 1.0, "column: {a:?}, {b:?}"),
            }

            let nested_direction = match direction {
                SplitDirection::Row => SplitDirection::Column,
                SplitDirection::Column => SplitDirection::Row,
            };
            let third = app.world_mut().resource_mut::<TilingTree>().split(first, nested_direction).unwrap();
            app.update();
            let (a, b, c) = (position(&mut app, first), position(&mut app, second), position(&mut app, third));
            match direction {
                SplitDirection::Row => assert!(b.x > a.x && c.y > a.y && (c.x - a.x).abs() < 1.0, "nested: {a:?}, {b:?}, {c:?}"),
                SplitDirection::Column => assert!(b.y > a.y && c.x > a.x && (c.y - a.y).abs() < 1.0, "nested: {a:?}, {b:?}, {c:?}"),
            }
            let world = app.world_mut();
            let count = world.query_filtered::<Entity, With<crate::cell::ConversationContainer>>().iter(world).count();
            assert_eq!(count, 3, "rebuild leaves exactly the current panes");
        }
    }

    #[test]
    fn stale_dock_images_do_not_expand_the_conversation_after_resize() {
        for scale in [1.0_f32, 2.0] {
            let (mut app, camera) = layout_app(scale);
            app.update();

            let images = {
                let mut images = app.world_mut().resource_mut::<Assets<Image>>();
                [40, 31].map(|height| view::ui_rtt::create_ui_rtt_texture(
                    &mut images, (2347.0 * scale) as u32, (height as f32 * scale) as u32,
                ))
            };
            let world = app.world_mut();
            let north = world.query_filtered::<Entity, With<ui::dock::NorthDock>>().single(world).unwrap();
            let south = world.query_filtered::<Entity, With<ui::dock::SouthDock>>().single(world).unwrap();
            world.get_mut::<ImageNode>(north).unwrap().image = images[0].clone();
            world.get_mut::<ImageNode>(south).unwrap().image = images[1].clone();
            app.update();

            for width in [1895_u32, 800] {
                let size = UVec2::new((width as f32 * scale) as u32, (1091.0 * scale) as u32);
                let mut camera = app.world_mut().get_mut::<Camera>(camera).unwrap();
                camera.computed.target_info.as_mut().unwrap().physical_size = size;
                camera.viewport.as_mut().unwrap().physical_size = size;
                app.update();
                let world = app.world_mut();
                let root = world.query_filtered::<Entity, With<ui::tiling_reconciler::TilingRoot>>().single(world).unwrap();
                let content = world.query_filtered::<Entity, With<ui::state::ContentArea>>().single(world).unwrap();
                let conversation = world.query_filtered::<Entity, With<ui::state::ConversationRoot>>().single(world).unwrap();
                for entity in [root, north, south, content, conversation] {
                    let actual = world.get::<ComputedNode>(entity).unwrap().size().x;
                    assert!((actual - size.x as f32).abs() <= 1.0,
                        "entity {entity:?} is {actual}px wide in a {}px viewport at scale {scale}", size.x);
                }
            }
        }
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn cli_is_well_formed() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn window_size_parses_width_x_height() {
        assert_eq!(parse_window_size("1920x1080"), Ok((1920, 1080)));
        let cli = Cli::try_parse_from(["kaijutsu", "--window-size", "1280x720"]).unwrap();
        assert_eq!(cli.window_size, Some((1280, 720)));
    }

    #[test]
    fn window_size_rejects_what_is_not_a_size() {
        for bad in ["1920", "axb", "0x600", "800x0", "-1x5", "800x", "x600", "1.5x2"] {
            assert!(parse_window_size(bad).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn window_size_conflicts_with_fullscreen_and_maximize() {
        assert!(Cli::try_parse_from(["kaijutsu", "--window-size", "800x600", "--fullscreen"]).is_err());
        assert!(Cli::try_parse_from(["kaijutsu", "--window-size", "800x600", "--maximize"]).is_err());
    }

    #[test]
    fn fullscreen_and_maximize_conflict() {
        assert!(Cli::try_parse_from(["kaijutsu", "--fullscreen", "--maximize"]).is_err());
        assert!(Cli::try_parse_from(["kaijutsu", "--fullscreen"]).is_ok());
        assert!(Cli::try_parse_from(["kaijutsu", "--maximize"]).is_ok());
    }

    /// Both flags parse fine at the clap level now — mutual exclusivity
    /// moved to `kaijutsu_client::resolve_key_source` (its own tests cover
    /// that refusal), the same resolver `kaijutsu-mcp`, `kaijutsu-acp`, and
    /// `kaijutsu-tui` call through `KeyArgs::key_source`. This test only
    /// confirms the flags reach the fields the resolver reads.
    #[test]
    fn key_selectors_flatten_and_reach_their_fields() {
        let both = Cli::try_parse_from([
            "kaijutsu",
            "--key-fingerprint",
            "SHA256:chosen",
            "--key-file",
            "/tmp/key",
        ])
        .expect("both flags parse; the resolver, not clap, refuses the combination");
        assert_eq!(both.key.key_fingerprint.as_deref(), Some("SHA256:chosen"));
        assert_eq!(both.key.key_file.as_deref(), Some(std::path::Path::new("/tmp/key")));

        let neither = Cli::try_parse_from(["kaijutsu"]).expect("parses");
        assert!(neither.key.key_fingerprint.is_none());
        assert!(neither.key.key_file.is_none());
    }
}
