//! HUD System - Configurable heads-up display panels
//!
//! HUDs are glowing overlay panels that display contextual information.
//!
//! ## Configuration
//!
//! Currently hardcoded in `load_hud_config()`.
//! TODO: Load from `~/.config/kaijutsu/hud.rhai`
//!
//! ## HUD Styles
//!
//! - `orbital` - Curved edge, follows screen contour, particle accents
//! - `panel` - Rectangle with glow halo and depth shadow
//! - `minimal` - Text only, no chrome (dense info display)
//!
//! ## Built-in Widgets
//!
//! - `agent_status` - Who's working where, streaming indicators
//! - `keybinds` - Context-sensitive key hints
//! - `git_status` - Branch, dirty files, ahead/behind
//! - `session_info` - Time, kernel, context count
//! - `token_usage` - Session tokens, cost estimate
//! - `build_status` - cargo watch output summary

mod widgets;

use bevy::prelude::*;

use super::constellation::Constellation;
use crate::connection::bridge::ConnectionState;
use crate::shaders::HudPanelMaterial;

// Widgets are currently generated inline in this module

/// Plugin for the HUD system
pub struct HudPlugin;

impl Plugin for HudPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<HudConfig>()
            .init_resource::<WidgetCache>()
            .init_resource::<WidgetPollTimer>()
            .init_resource::<FocusedHud>()
            .register_type::<HudPosition>()
            .register_type::<HudStyle>()
            .register_type::<HudVisibility>()
            .register_type::<FocusedHud>()
            .add_message::<HudConfigChanged>()
            .add_systems(Startup, load_hud_config)
            .add_systems(
                Update,
                (
                    poll_widget_data,
                    despawn_removed_huds,
                    spawn_configured_huds,
                    update_hud_visibility,
                    update_widget_content,
                    handle_hud_navigation_keys,
                    update_hud_focus_visual,
                    spawn_config_editor,
                    despawn_config_editor,
                    handle_config_changes,
                )
                    .chain(),
            );
    }
}

// ============================================================================
// WIDGET CACHE (Performance optimization)
// ============================================================================

/// Cached widget data - updated by polling, read by render systems.
///
/// This decouples expensive widget content generation (file I/O, string formatting)
/// from the render loop. Polling happens on a timer; rendering reads from memory.
#[derive(Resource, Default)]
pub struct WidgetCache {
    pub build_status: String,
    pub keybinds: String,
    pub session_info: String,
    pub connection_status: String,
}

/// Timer for widget polling.
#[derive(Resource)]
pub struct WidgetPollTimer(pub Timer);

impl Default for WidgetPollTimer {
    fn default() -> Self {
        // Poll every 500ms - fast enough for responsiveness, slow enough to not waste cycles
        Self(Timer::from_seconds(0.5, TimerMode::Repeating))
    }
}

// ============================================================================
// CONFIGURATION
// ============================================================================

/// HUD configuration (currently hardcoded, Rhai loading planned)
#[derive(Resource, Default)]
pub struct HudConfig {
    /// Configured HUD definitions
    pub huds: Vec<HudDefinition>,
    /// Whether config has been loaded
    pub loaded: bool,
}

/// Definition of a single HUD panel
#[derive(Clone, Debug)]
pub struct HudDefinition {
    /// Unique name for this HUD
    pub name: String,
    /// Screen position
    pub position: HudPosition,
    /// Visual style
    pub style: HudStyle,
    /// Glow configuration
    pub glow: GlowConfig,
    /// Content widget type
    pub content: HudContent,
    /// Visibility behavior
    pub visibility: HudVisibility,
}

/// Screen position for HUD placement
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Reflect, serde::Deserialize)]
pub enum HudPosition {
    #[default]
    TopRight,
    TopLeft,
    BottomRight,
    BottomLeft,
    Left,
    Right,
    Top,
    Bottom,
}

impl HudPosition {
    /// Get CSS-like position values
    pub fn to_node_position(&self) -> (Val, Val, Val, Val) {
        // Returns (top, right, bottom, left)
        match self {
            Self::TopRight => (Val::Px(60.0), Val::Px(16.0), Val::Auto, Val::Auto),
            Self::TopLeft => (Val::Px(60.0), Val::Auto, Val::Auto, Val::Px(16.0)),
            Self::BottomRight => (Val::Auto, Val::Px(16.0), Val::Px(60.0), Val::Auto),
            Self::BottomLeft => (Val::Auto, Val::Auto, Val::Px(60.0), Val::Px(16.0)),
            Self::Left => (Val::Percent(30.0), Val::Auto, Val::Auto, Val::Px(16.0)),
            Self::Right => (Val::Percent(30.0), Val::Px(16.0), Val::Auto, Val::Auto),
            Self::Top => (Val::Px(60.0), Val::Auto, Val::Auto, Val::Percent(30.0)),
            Self::Bottom => (Val::Auto, Val::Auto, Val::Px(60.0), Val::Percent(30.0)),
        }
    }
}

/// Visual style for HUD panel
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Reflect)]
pub enum HudStyle {
    /// Curved edge, follows screen contour
    Orbital,
    /// Rectangle with glow halo
    #[default]
    Panel,
    /// Text only, no chrome
    Minimal,
}

/// Glow effect configuration
#[derive(Clone, Debug)]
pub struct GlowConfig {
    /// Glow color
    pub color: Color,
    /// Glow intensity (0.0-1.0)
    pub intensity: f32,
}

impl Default for GlowConfig {
    fn default() -> Self {
        Self {
            color: Color::srgb(0.34, 0.65, 1.0), // Cyan accent
            intensity: 0.5,
        }
    }
}

/// Content type for HUD widget
#[derive(Clone, Debug)]
pub enum HudContent {
    /// Built-in keybinds widget
    Keybinds,
    /// Built-in session info widget
    SessionInfo,
    /// Built-in build status widget
    BuildStatus,
    /// Connection status widget (server connectivity, identity)
    ConnectionStatus,
}

/// Visibility behavior for HUD
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Reflect)]
pub enum HudVisibility {
    /// Always visible
    #[default]
    Always,
    /// Only visible when content changes
    OnChange,
    /// Visible on hover or key toggle
    OnDemand,
    /// Currently hidden
    Hidden,
}

// ============================================================================
// FOCUS STATE
// ============================================================================

/// Tracks which HUD widget is currently focused for editing.
///
/// When a HUD is focused:
/// - It shows a visual highlight
/// - Pressing 'i' enters config editing mode
/// - j/k navigates between HUDs
#[derive(Resource, Default, Reflect)]
#[reflect(Resource)]
pub struct FocusedHud {
    /// The focused HUD entity, if any
    pub entity: Option<Entity>,
    /// Whether we're in HUD navigation mode (H key toggles)
    pub navigation_active: bool,
    /// Whether we're editing the focused HUD's config
    pub editing: bool,
}

impl FocusedHud {
    /// Check if a specific entity is focused
    pub fn is_focused(&self, entity: Entity) -> bool {
        self.entity == Some(entity)
    }

    /// Focus a HUD entity
    pub fn focus(&mut self, entity: Entity) {
        self.entity = Some(entity);
        self.navigation_active = true;
    }

    /// Clear focus and exit navigation mode
    pub fn clear(&mut self) {
        self.entity = None;
        self.navigation_active = false;
        self.editing = false;
    }

    /// Enter config editing mode for the focused HUD
    pub fn start_editing(&mut self) {
        if self.entity.is_some() {
            self.editing = true;
        }
    }

    /// Exit config editing mode
    pub fn stop_editing(&mut self) {
        self.editing = false;
    }
}

// ============================================================================
// COMPONENTS
// ============================================================================

/// Marker for a HUD entity
#[derive(Component)]
pub struct Hud {
    /// HUD name (matches config)
    pub name: String,
}

/// Marker indicating a HUD can be focused for config editing
#[derive(Component, Default)]
pub struct HudFocusable;

/// Marker for the HUD content text
#[derive(Component)]
pub struct HudText {
    pub hud_name: String,
}

/// Marker for the config editor overlay
#[derive(Component)]
#[allow(dead_code)] // Fields used for future live editing feature
pub struct HudConfigEditor {
    /// Which HUD is being edited
    pub hud_name: String,
    /// Original config text (for comparison)
    pub original: String,
    /// Current edited text
    pub current: String,
}

/// Message sent when config editing is confirmed
#[derive(bevy::ecs::prelude::Message, Clone)]
pub struct HudConfigChanged {
    pub hud_name: String,
    pub new_config: String,
}

// ============================================================================
// SYSTEMS
// ============================================================================

/// Load HUD configuration (hardcoded for now)
fn load_hud_config(mut config: ResMut<HudConfig>) {
    config.huds = vec![
        HudDefinition {
            name: "keybinds".to_string(),
            position: HudPosition::BottomRight,
            style: HudStyle::Minimal,
            glow: GlowConfig::default(),
            content: HudContent::Keybinds,
            visibility: HudVisibility::Always,
        },
        HudDefinition {
            name: "session".to_string(),
            position: HudPosition::TopRight,
            style: HudStyle::Panel,
            glow: GlowConfig {
                color: Color::srgb(0.49, 0.85, 0.82), // Cyan
                intensity: 0.4,
            },
            content: HudContent::SessionInfo,
            visibility: HudVisibility::Always,
        },
        HudDefinition {
            name: "connection".to_string(),
            position: HudPosition::TopLeft,
            style: HudStyle::Panel,
            glow: GlowConfig {
                color: Color::srgb(0.34, 0.65, 1.0), // Blue
                intensity: 0.3,
            },
            content: HudContent::ConnectionStatus,
            visibility: HudVisibility::Always,
        },
        HudDefinition {
            name: "build".to_string(),
            position: HudPosition::BottomLeft,
            style: HudStyle::Minimal,
            glow: GlowConfig::default(),
            content: HudContent::BuildStatus,
            visibility: HudVisibility::Always,
        },
    ];

    config.loaded = true;
    info!("Loaded {} HUD definitions", config.huds.len());
}

/// Remove HUDs that are no longer in the config
fn despawn_removed_huds(
    mut commands: Commands,
    config: Res<HudConfig>,
    huds: Query<(Entity, &Hud)>,
) {
    // Only check when config changes
    if !config.is_changed() {
        return;
    }

    for (entity, hud) in huds.iter() {
        let still_exists = config.huds.iter().any(|d| d.name == hud.name);
        if !still_exists {
            commands.entity(entity).despawn(); // Bevy 0.18: despawn handles children
            info!("Despawned removed HUD: {}", hud.name);
        }
    }
}

/// Spawn HUD entities from configuration
fn spawn_configured_huds(
    mut commands: Commands,
    config: Res<HudConfig>,
    theme: Res<crate::ui::theme::Theme>,
    existing: Query<&Hud>,
    screen: Res<State<crate::ui::state::AppScreen>>,
    mut panel_materials: ResMut<Assets<HudPanelMaterial>>,
) {
    // Only spawn in Conversation state
    if *screen.get() != crate::ui::state::AppScreen::Conversation {
        return;
    }

    if !config.loaded {
        return;
    }

    // Collect existing HUD names
    let existing_names: Vec<&str> = existing.iter().map(|h| h.name.as_str()).collect();

    for def in &config.huds {
        if existing_names.contains(&def.name.as_str()) {
            continue;
        }

        let (top, right, bottom, left) = def.position.to_node_position();

        // Create panel material for Panel style
        let is_panel = matches!(def.style, HudStyle::Panel);
        let panel_material = if is_panel {
            let glow_linear = def.glow.color.to_linear();
            Some(panel_materials.add(HudPanelMaterial {
                color: Vec4::new(
                    theme.hud_panel_bg.to_linear().red,
                    theme.hud_panel_bg.to_linear().green,
                    theme.hud_panel_bg.to_linear().blue,
                    theme.hud_panel_bg.alpha(),
                ),
                glow_color: Vec4::new(
                    glow_linear.red,
                    glow_linear.green,
                    glow_linear.blue,
                    0.8,
                ),
                params: Vec4::new(def.glow.intensity, 0.0, 1.5, 0.0),
                time: Vec4::ZERO,
            }))
        } else {
            None
        };

        // Spawn HUD container
        let mut entity = commands.spawn((
            Hud {
                name: def.name.clone(),
            },
            HudFocusable,
            Node {
                position_type: PositionType::Absolute,
                top,
                right,
                bottom,
                left,
                padding: UiRect::all(Val::Px(8.0)),
                min_width: Val::Px(120.0),
                border: UiRect::all(Val::Px(2.0)), // Reserve space for focus border
                ..default()
            },
            BorderColor::all(Color::NONE), // Invisible by default, shown when focused
            ZIndex(crate::constants::ZLayer::HUD),
        ));

        // Add style-specific components
        if let Some(material) = panel_material {
            // Panel style uses shader material
            entity.insert(MaterialNode(material));
        } else {
            // Non-Panel styles use simple background/border
            entity.insert((
                BackgroundColor(match def.style {
                    HudStyle::Panel => theme.hud_panel_bg, // Unreachable
                    HudStyle::Orbital => theme.hud_panel_bg.with_alpha(0.8),
                    HudStyle::Minimal => Color::NONE,
                }),
                BorderColor::all(if matches!(def.style, HudStyle::Minimal) {
                    Color::NONE
                } else {
                    theme.hud_panel_glow.with_alpha(theme.hud_panel_glow_intensity)
                }),
            ));
        }

        // Add children (text content)
        entity.with_children(|parent| {
            parent.spawn((
                HudText {
                    hud_name: def.name.clone(),
                },
                crate::text::MsdfUiText::new("")
                    .with_font_size(theme.hud_font_size)
                    .with_color(theme.hud_text_color),
                crate::text::UiTextPositionCache::default(),
                Node {
                    min_width: Val::Px(100.0),
                    min_height: Val::Px(14.0),
                    ..default()
                },
            ));
        });

        info!("Spawned HUD: {} at {:?} (style: {:?})", def.name, def.position, def.style);
    }
}

/// Update HUD visibility based on state
fn update_hud_visibility(
    config: Res<HudConfig>,
    screen: Res<State<crate::ui::state::AppScreen>>,
    mut huds: Query<(&Hud, &mut Visibility)>,
) {
    let in_conversation = *screen.get() == crate::ui::state::AppScreen::Conversation;

    for (hud, mut vis) in huds.iter_mut() {
        // Find the definition for this HUD
        let def = config.huds.iter().find(|d| d.name == hud.name);

        let should_show = in_conversation
            && def
                .map(|d| !matches!(d.visibility, HudVisibility::Hidden))
                .unwrap_or(true);

        *vis = if should_show {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
    }
}

/// Poll and cache widget data on a timer.
///
/// This is the only system that performs file I/O or expensive string formatting.
/// Runs every 500ms instead of every frame.
fn poll_widget_data(
    time: Res<Time>,
    mut timer: ResMut<WidgetPollTimer>,
    mut cache: ResMut<WidgetCache>,
    constellation: Res<Constellation>,
    dashboard: Res<crate::dashboard::DashboardState>,
    conn_state: Res<ConnectionState>,
) {
    if !timer.0.tick(time.delta()).just_finished() {
        return;
    }

    // Regenerate all widget content when timer fires
    cache.build_status = generate_build_status();
    cache.keybinds = generate_keybinds_content(&constellation);
    cache.session_info = generate_session_info(&constellation, &dashboard);
    cache.connection_status = generate_connection_status(&conn_state);
}

/// Update widget content text from cache.
///
/// Runs every frame but only reads from memory. The actual content generation
/// happens in poll_widget_data on a timer.
fn update_widget_content(
    config: Res<HudConfig>,
    cache: Res<WidgetCache>,
    mut hud_texts: Query<(&HudText, &mut crate::text::MsdfUiText)>,
) {
    // Early return if cache hasn't changed
    if !cache.is_changed() {
        return;
    }

    for (hud_text, mut text) in hud_texts.iter_mut() {
        // Find the content type for this HUD
        let def = config.huds.iter().find(|d| d.name == hud_text.hud_name);
        let Some(def) = def else { continue };

        // Read from cache instead of regenerating
        text.text = match &def.content {
            HudContent::Keybinds => cache.keybinds.clone(),
            HudContent::SessionInfo => cache.session_info.clone(),
            HudContent::BuildStatus => cache.build_status.clone(),
            HudContent::ConnectionStatus => cache.connection_status.clone(),
        };
    }
}

// ============================================================================
// WIDGET CONTENT GENERATORS
// ============================================================================

fn generate_keybinds_content(constellation: &Constellation) -> String {
    let mode_hint = match constellation.mode {
        crate::ui::constellation::ConstellationMode::Focused => "Tab: map view",
        crate::ui::constellation::ConstellationMode::Map => "Tab: orbital | hjkl: navigate",
        crate::ui::constellation::ConstellationMode::Orbital => "Tab: focused | gt/gT: cycle",
    };

    format!("i: chat | s: shell | {}", mode_hint)
}

fn generate_session_info(constellation: &Constellation, dashboard: &crate::dashboard::DashboardState) -> String {
    let context_count = constellation.nodes.len();
    let kernel_name = dashboard
        .selected_kernel()
        .map(|k| k.name.as_str())
        .unwrap_or("none");

    format!("{} | {} contexts", kernel_name, context_count)
}

fn generate_connection_status(conn: &ConnectionState) -> String {
    if conn.connected {
        conn.identity
            .as_ref()
            .map(|i| format!("@{}", i.username))
            .unwrap_or_else(|| "connected".to_string())
    } else if conn.reconnect_attempt > 0 {
        format!("reconnecting ({})", conn.reconnect_attempt)
    } else {
        "disconnected".to_string()
    }
}

fn generate_build_status() -> String {
    std::fs::read_to_string("/tmp/kj.status")
        .map(|s| s.lines().next().unwrap_or("?").to_string())
        .unwrap_or_else(|_| "build: ?".to_string())
}

// ============================================================================
// HUD NAVIGATION SYSTEMS
// ============================================================================

/// Handle keyboard navigation for HUD focus.
///
/// - `H` (shift+h): Toggle HUD navigation mode
/// - `j`/`k`: Move focus between HUDs (when in navigation mode)
/// - `i`: Enter config editing mode for focused HUD
/// - `Esc`: Exit navigation/editing mode
fn handle_hud_navigation_keys(
    keys: Res<ButtonInput<KeyCode>>,
    screen: Res<State<crate::ui::state::AppScreen>>,
    current_mode: Res<crate::cell::CurrentMode>,
    mut focused_hud: ResMut<FocusedHud>,
    focusable_huds: Query<Entity, With<HudFocusable>>,
) {
    // Only in Conversation state and Normal mode (not editing a cell)
    if *screen.get() != crate::ui::state::AppScreen::Conversation {
        return;
    }
    if current_mode.0 != crate::cell::EditorMode::Normal {
        return;
    }

    // Don't handle keys if we're editing a HUD config
    if focused_hud.editing {
        // Escape exits editing mode
        if keys.just_pressed(KeyCode::Escape) {
            focused_hud.stop_editing();
            info!("Exited HUD config editing");
        }
        return;
    }

    // Shift+H toggles HUD navigation mode
    let shift = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);
    if shift && keys.just_pressed(KeyCode::KeyH) {
        if focused_hud.navigation_active {
            focused_hud.clear();
            info!("Exited HUD navigation mode");
        } else {
            // Focus the first HUD
            if let Some(first) = focusable_huds.iter().next() {
                focused_hud.focus(first);
                info!("Entered HUD navigation mode");
            }
        }
        return;
    }

    // Only process navigation keys when in navigation mode
    if !focused_hud.navigation_active {
        return;
    }

    // Escape clears focus
    if keys.just_pressed(KeyCode::Escape) {
        focused_hud.clear();
        info!("Exited HUD navigation mode");
        return;
    }

    // i enters editing mode
    if keys.just_pressed(KeyCode::KeyI) && focused_hud.entity.is_some() {
        focused_hud.start_editing();
        info!("Entered HUD config editing mode");
        return;
    }

    // j/k navigate between HUDs
    let huds: Vec<Entity> = focusable_huds.iter().collect();
    if huds.is_empty() {
        return;
    }

    let current_idx = focused_hud
        .entity
        .and_then(|e| huds.iter().position(|&h| h == e))
        .unwrap_or(0);

    if keys.just_pressed(KeyCode::KeyJ) {
        // Next HUD
        let next_idx = (current_idx + 1) % huds.len();
        focused_hud.entity = Some(huds[next_idx]);
    } else if keys.just_pressed(KeyCode::KeyK) {
        // Previous HUD
        let prev_idx = if current_idx == 0 {
            huds.len() - 1
        } else {
            current_idx - 1
        };
        focused_hud.entity = Some(huds[prev_idx]);
    }
}

/// Update visual appearance of HUDs based on focus state.
///
/// Focused HUDs show a bright border, unfocused ones hide the border.
fn update_hud_focus_visual(
    focused_hud: Res<FocusedHud>,
    theme: Res<crate::ui::theme::Theme>,
    mut huds: Query<(Entity, &mut BorderColor), With<HudFocusable>>,
) {
    if !focused_hud.is_changed() {
        return;
    }

    for (entity, mut border_color) in huds.iter_mut() {
        if focused_hud.is_focused(entity) {
            // Show bright focus border
            let focus_color = if focused_hud.editing {
                theme.accent2 // Different color when editing
            } else {
                theme.accent // Normal focus color
            };
            *border_color = BorderColor::all(focus_color);
        } else {
            // Hide border
            *border_color = BorderColor::all(Color::NONE);
        }
    }
}

/// Spawn config editor overlay when entering edit mode.
fn spawn_config_editor(
    mut commands: Commands,
    focused_hud: Res<FocusedHud>,
    config: Res<HudConfig>,
    theme: Res<crate::ui::theme::Theme>,
    huds: Query<&Hud>,
    existing_editor: Query<Entity, With<HudConfigEditor>>,
) {
    // Only spawn when just started editing
    if !focused_hud.is_changed() || !focused_hud.editing {
        return;
    }

    // Don't spawn if already exists
    if !existing_editor.is_empty() {
        return;
    }

    // Find the focused HUD's name
    let Some(hud_entity) = focused_hud.entity else {
        return;
    };
    let Ok(hud) = huds.get(hud_entity) else {
        return;
    };

    // Find the config definition
    let Some(def) = config.huds.iter().find(|d| d.name == hud.name) else {
        return;
    };

    // Generate config text in a readable format
    let config_text = format!(
        "# HUD Configuration: {}\n\
         # Edit values below, press Esc to save\n\
         \n\
         name: \"{}\"\n\
         position: {:?}\n\
         style: {:?}\n\
         visibility: {:?}\n\
         glow_intensity: {:.2}\n",
        def.name,
        def.name,
        def.position,
        def.style,
        def.visibility,
        def.glow.intensity,
    );

    // Spawn the config editor overlay
    commands.spawn((
        HudConfigEditor {
            hud_name: hud.name.clone(),
            original: config_text.clone(),
            current: config_text.clone(),
        },
        Node {
            position_type: PositionType::Absolute,
            left: Val::Percent(25.0),
            top: Val::Percent(20.0),
            width: Val::Percent(50.0),
            min_height: Val::Px(200.0),
            padding: UiRect::all(Val::Px(16.0)),
            flex_direction: FlexDirection::Column,
            border: UiRect::all(Val::Px(2.0)),
            border_radius: BorderRadius::all(Val::Px(8.0)),
            ..default()
        },
        BackgroundColor(theme.panel_bg),
        BorderColor::all(theme.accent2),
        ZIndex(crate::constants::ZLayer::MODAL),
    ))
    .with_children(|parent| {
        // Title
        parent.spawn((
            crate::text::MsdfUiText::new(&format!("Config: {}", hud.name))
                .with_font_size(16.0)
                .with_color(theme.accent),
            crate::text::UiTextPositionCache::default(),
            Node {
                margin: UiRect::bottom(Val::Px(12.0)),
                min_height: Val::Px(20.0),
                ..default()
            },
        ));

        // Config content (read-only display for now)
        parent.spawn((
            crate::text::MsdfUiText::new(&config_text)
                .with_font_size(12.0)
                .with_color(theme.fg),
            crate::text::UiTextPositionCache::default(),
            Node {
                min_height: Val::Px(120.0),
                ..default()
            },
        ));

        // Instructions
        parent.spawn((
            crate::text::MsdfUiText::new("Press Esc to close • Full editing coming soon")
                .with_font_size(11.0)
                .with_color(theme.fg_dim),
            crate::text::UiTextPositionCache::default(),
            Node {
                margin: UiRect::top(Val::Px(12.0)),
                min_height: Val::Px(16.0),
                ..default()
            },
        ));
    });

    info!("Spawned config editor for HUD: {}", hud.name);
}

/// Despawn config editor when exiting edit mode.
fn despawn_config_editor(
    mut commands: Commands,
    focused_hud: Res<FocusedHud>,
    editors: Query<Entity, With<HudConfigEditor>>,
) {
    // Only despawn when editing state changed to false
    if !focused_hud.is_changed() || focused_hud.editing {
        return;
    }

    for entity in editors.iter() {
        commands.entity(entity).despawn();
        info!("Despawned config editor");
    }
}

/// Handle config change events (placeholder for future implementation).
fn handle_config_changes(
    mut events: MessageReader<HudConfigChanged>,
    mut _config: ResMut<HudConfig>,
) {
    for event in events.read() {
        info!(
            "Config change for HUD '{}': {}",
            event.hud_name,
            event.new_config.lines().next().unwrap_or("")
        );

        // TODO: Parse the config text and update HudConfig
        // This would involve:
        // 1. Parse the key-value pairs from the text
        // 2. Update the corresponding HudDefinition
        // 3. The spawn/update systems will react to the config change
    }
}
