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
            .register_type::<HudPosition>()
            .register_type::<HudStyle>()
            .register_type::<HudVisibility>()
            .add_systems(Startup, load_hud_config)
            .add_systems(
                Update,
                (
                    despawn_removed_huds,
                    spawn_configured_huds,
                    update_hud_visibility,
                    update_widget_content,
                )
                    .chain(),
            );
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Reflect)]
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
// COMPONENTS
// ============================================================================

/// Marker for a HUD entity
#[derive(Component)]
pub struct Hud {
    /// HUD name (matches config)
    pub name: String,
}

/// Marker for the HUD content text
#[derive(Component)]
pub struct HudText {
    pub hud_name: String,
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
            Node {
                position_type: PositionType::Absolute,
                top,
                right,
                bottom,
                left,
                padding: UiRect::all(Val::Px(8.0)),
                min_width: Val::Px(120.0),
                ..default()
            },
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

/// Update widget content text
fn update_widget_content(
    config: Res<HudConfig>,
    constellation: Res<crate::ui::constellation::Constellation>,
    dashboard: Res<crate::dashboard::DashboardState>,
    conn_state: Res<ConnectionState>,
    mut hud_texts: Query<(&HudText, &mut crate::text::MsdfUiText)>,
) {
    for (hud_text, mut text) in hud_texts.iter_mut() {
        // Find the content type for this HUD
        let def = config.huds.iter().find(|d| d.name == hud_text.hud_name);
        let Some(def) = def else { continue };

        // Generate content based on widget type
        text.text = match &def.content {
            HudContent::Keybinds => generate_keybinds_content(&constellation),
            HudContent::SessionInfo => generate_session_info(&constellation, &dashboard),
            HudContent::BuildStatus => generate_build_status(),
            HudContent::ConnectionStatus => generate_connection_status(&conn_state),
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
