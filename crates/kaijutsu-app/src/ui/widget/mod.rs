//! Widget System - Unified UI primitives for chrome and HUD
//!
//! Widgets are the single UI primitive for all non-content UI. They can:
//! - Dock to any edge (North, South, East, West)
//! - Float freely with x,y positioning
//! - Auto-size based on content with optional min/max constraints
//!
//! ## Architecture
//!
//! ```text
//! ┌────────────────────────────────────────────────────────────────┐
//! │ [title]                                     [connection]       │ ← North
//! ├────────┬──────────────────────────────────────────────┬────────┤
//! │        │                                              │        │
//! │        │              FOCAL CONTEXT                   │        │
//! │        │                                              │        │
//! ├────────┴──────────────────────────────────────────────┴────────┤
//! │ [mode]                                              [hints]    │ ← South
//! └────────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Phase 2 Scope
//!
//! - All four dock containers (N/S/E/W)
//! - Widget ordering within docks
//! - Title and connection status widgets in North dock

use bevy::prelude::*;

use crate::cell::{CurrentMode, EditorMode, InputKind};
use crate::connection::RpcConnectionState;
use crate::text::{bevy_to_rgba8, MsdfUiText, UiTextPositionCache};
use crate::ui::drift::DriftState;
use crate::ui::theme::Theme;

// ============================================================================
// CORE TYPES
// ============================================================================

/// Unique identifier for a widget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Component)]
pub struct WidgetId(pub u32);

/// Which edge a widget docks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Reflect, serde::Deserialize)]
pub enum Edge {
    North,
    #[default]
    South,
    East,
    West,
}

/// Widget positioning state.
#[derive(Debug, Clone, Copy, PartialEq, Reflect)]
pub enum WidgetState {
    /// Docked to an edge with ordering priority
    Docked {
        edge: Edge,
        /// Order within the dock (lower = leftmost/topmost)
        order: i32,
    },
    /// Free-floating at absolute position
    Floating { x: f32, y: f32 },
    /// Temporarily hidden
    Hidden,
}

impl Default for WidgetState {
    fn default() -> Self {
        Self::Docked {
            edge: Edge::South,
            order: 0,
        }
    }
}

/// Size hints for widget layout.
#[derive(Debug, Clone, Copy, PartialEq, Default, Reflect)]
pub struct WidgetSize {
    /// Minimum width in pixels
    pub min_width: Option<f32>,
    /// Minimum height in pixels
    pub min_height: Option<f32>,
    /// Maximum width in pixels
    pub max_width: Option<f32>,
    /// Maximum height in pixels
    pub max_height: Option<f32>,
    /// Priority for space allocation (lower = hide first when constrained)
    pub priority: i32,
}

impl WidgetSize {
    /// Create size hints with just min dimensions.
    pub fn min(width: f32, height: f32) -> Self {
        Self {
            min_width: Some(width),
            min_height: Some(height),
            ..default()
        }
    }
}

/// What a widget displays.
///
/// Each content type knows how to render itself and what data it needs.
#[derive(Debug, Clone, PartialEq, Reflect)]
pub enum WidgetContent {
    /// Simple text with template string
    Text { template: String },
    /// Application title
    Title,
    /// Mode indicator - reactive to CurrentMode
    Mode,
    /// Connection status - reactive to RpcConnectionState
    Connection,
    /// Drift context list - reactive to DriftState
    Contexts,
}

impl Default for WidgetContent {
    fn default() -> Self {
        Self::Text {
            template: String::new(),
        }
    }
}

// ============================================================================
// COMPONENTS
// ============================================================================

/// Core widget component - the unified UI primitive.
#[derive(Component, Debug, Clone, Reflect)]
#[reflect(Component)]
pub struct Widget {
    /// Display name for debugging/config
    pub name: String,
    /// What this widget displays
    pub content: WidgetContent,
    /// Positioning state
    pub state: WidgetState,
    /// Size hints
    pub size: WidgetSize,
}

impl Widget {
    /// Create a new widget with the given name and content.
    pub fn new(name: impl Into<String>, content: WidgetContent) -> Self {
        Self {
            name: name.into(),
            content,
            state: WidgetState::default(),
            size: WidgetSize::default(),
        }
    }

    /// Set the widget state (docked/floating/hidden).
    pub fn with_state(mut self, state: WidgetState) -> Self {
        self.state = state;
        self
    }

    /// Set size hints.
    pub fn with_size(mut self, size: WidgetSize) -> Self {
        self.size = size;
        self
    }
}

/// Marker for widget text content (the MsdfUiText child).
#[derive(Component, Debug, Clone)]
pub struct WidgetText {
    /// Parent widget's name for lookup
    pub widget_name: String,
}

// ============================================================================
// DOCK CONTAINERS
// ============================================================================

/// Marker for a dock container (holds widgets for one edge).
#[derive(Component, Debug, Clone, Copy, Reflect)]
#[reflect(Component)]
pub struct DockContainer {
    pub edge: Edge,
}

/// Marker for spacer elements within docks.
#[derive(Component, Debug, Clone, Copy)]
pub struct DockSpacer;

// ============================================================================
// CONFIGURATION
// ============================================================================

/// Widget configuration resource.
///
/// Defines which widgets to spawn and their initial configuration.
/// Phase 2: Hardcoded. Future: Load from ~/.config/kaijutsu/widgets.toml
#[derive(Resource, Default, Reflect)]
#[reflect(Resource)]
pub struct WidgetConfig {
    /// Whether config has been loaded
    pub loaded: bool,
    /// Next widget ID to assign
    pub next_id: u32,
}

impl WidgetConfig {
    /// Get the next widget ID and increment the counter.
    pub fn next_widget_id(&mut self) -> WidgetId {
        let id = WidgetId(self.next_id);
        self.next_id += 1;
        id
    }
}

// ============================================================================
// PLUGIN
// ============================================================================

/// Plugin for the widget system.
pub struct WidgetPlugin;

impl Plugin for WidgetPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<WidgetConfig>()
            .register_type::<Edge>()
            .register_type::<WidgetState>()
            .register_type::<WidgetSize>()
            .register_type::<WidgetContent>()
            .register_type::<Widget>()
            .register_type::<DockContainer>()
            .register_type::<WidgetConfig>()
            .add_systems(Startup, setup_dock_containers)
            .add_systems(
                Update,
                (
                    spawn_initial_widgets,
                    update_mode_widget,
                    update_connection_widget,
                    update_contexts_widget,
                )
                    .chain(),
            );
    }
}

// ============================================================================
// SYSTEMS
// ============================================================================

/// Spawn dock containers for each edge.
///
/// Dock containers are flex containers that hold widgets for their edge.
/// North/South use horizontal (Row) layout, East/West use vertical (Column).
fn setup_dock_containers(mut commands: Commands, theme: Res<Theme>) {
    // North dock - header area
    commands.spawn((
        DockContainer { edge: Edge::North },
        Node {
            position_type: PositionType::Absolute,
            top: Val::Px(0.0),
            left: Val::Px(0.0),
            width: Val::Percent(100.0),
            height: Val::Auto,
            flex_direction: FlexDirection::Row,
            justify_content: JustifyContent::SpaceBetween,
            align_items: AlignItems::Center,
            padding: UiRect::axes(Val::Px(16.0), Val::Px(6.0)),
            border: UiRect::bottom(Val::Px(1.0)),
            ..default()
        },
        BorderColor::all(theme.border),
        ZIndex(crate::constants::ZLayer::HUD),
    ));

    // South dock - status bar area (now at bottom edge)
    commands.spawn((
        DockContainer { edge: Edge::South },
        Node {
            position_type: PositionType::Absolute,
            bottom: Val::Px(0.0),
            left: Val::Px(0.0),
            width: Val::Percent(100.0),
            height: Val::Auto,
            flex_direction: FlexDirection::Row,
            justify_content: JustifyContent::SpaceBetween,
            align_items: AlignItems::Center,
            padding: UiRect::axes(Val::Px(12.0), Val::Px(4.0)),
            border: UiRect::top(Val::Px(1.0)),
            ..default()
        },
        BorderColor::all(theme.border),
        ZIndex(crate::constants::ZLayer::HUD),
    ));

    // East dock - right sidebar (future)
    // West dock - left sidebar (future)

    info!("Spawned dock containers: North, South");
}

/// Spawn initial widgets from configuration.
fn spawn_initial_widgets(
    mut commands: Commands,
    mut config: ResMut<WidgetConfig>,
    theme: Res<Theme>,
    docks: Query<(Entity, &DockContainer)>,
    existing_widgets: Query<&Widget>,
) {
    // Only run once
    if config.loaded {
        return;
    }

    // Check if we already have widgets
    if !existing_widgets.is_empty() {
        config.loaded = true;
        return;
    }

    // Find dock entities
    let mut north_dock = None;
    let mut south_dock = None;

    for (entity, dock) in docks.iter() {
        match dock.edge {
            Edge::North => north_dock = Some(entity),
            Edge::South => south_dock = Some(entity),
            _ => {}
        }
    }

    // Need both docks to proceed
    let (Some(north), Some(south)) = (north_dock, south_dock) else {
        return; // Docks not spawned yet
    };

    // ═══════════════════════════════════════════════════════════════════════
    // NORTH DOCK WIDGETS
    // ═══════════════════════════════════════════════════════════════════════

    // Title widget (left side)
    let title_widget = spawn_widget(
        &mut commands,
        &mut config,
        &theme,
        Widget::new("title", WidgetContent::Title)
            .with_state(WidgetState::Docked {
                edge: Edge::North,
                order: 0,
            })
            .with_size(WidgetSize::min(180.0, 36.0)),
        "会術 Kaijutsu",
        24.0,
        theme.accent,
    );
    commands.entity(north).add_child(title_widget);

    // Spacer
    let north_spacer = commands.spawn((DockSpacer, Node { flex_grow: 1.0, ..default() })).id();
    commands.entity(north).add_child(north_spacer);

    // Connection status widget (right side)
    let conn_widget = spawn_widget(
        &mut commands,
        &mut config,
        &theme,
        Widget::new("connection", WidgetContent::Connection)
            .with_state(WidgetState::Docked {
                edge: Edge::North,
                order: 100,
            })
            .with_size(WidgetSize::min(200.0, 20.0)),
        "Connecting...",
        14.0,
        theme.fg_dim,
    );
    commands.entity(north).add_child(conn_widget);

    // ═══════════════════════════════════════════════════════════════════════
    // SOUTH DOCK WIDGETS
    // ═══════════════════════════════════════════════════════════════════════

    // Mode indicator widget (left side)
    let mode_widget = spawn_widget(
        &mut commands,
        &mut config,
        &theme,
        Widget::new("mode", WidgetContent::Mode)
            .with_state(WidgetState::Docked {
                edge: Edge::South,
                order: 0,
            })
            .with_size(WidgetSize::min(80.0, 20.0)),
        "NORMAL",
        14.0,
        theme.mode_normal,
    );
    commands.entity(south).add_child(mode_widget);

    // Spacer (left)
    let south_spacer_l = commands.spawn((DockSpacer, Node { flex_grow: 1.0, ..default() })).id();
    commands.entity(south).add_child(south_spacer_l);

    // Context list widget (center)
    let contexts_widget = spawn_widget(
        &mut commands,
        &mut config,
        &theme,
        Widget::new("contexts", WidgetContent::Contexts)
            .with_state(WidgetState::Docked {
                edge: Edge::South,
                order: 50,
            })
            .with_size(WidgetSize::min(200.0, 16.0)),
        "",
        11.0,
        theme.fg_dim,
    );
    commands.entity(south).add_child(contexts_widget);

    // Spacer (right)
    let south_spacer_r = commands.spawn((DockSpacer, Node { flex_grow: 1.0, ..default() })).id();
    commands.entity(south).add_child(south_spacer_r);

    // Key hints widget (right side)
    let hints_widget = spawn_widget(
        &mut commands,
        &mut config,
        &theme,
        Widget::new(
            "hints",
            WidgetContent::Text {
                template: "Enter: submit │ Shift+Enter: newline │ Esc: normal".to_string(),
            },
        )
        .with_state(WidgetState::Docked {
            edge: Edge::South,
            order: 100,
        })
        .with_size(WidgetSize::min(400.0, 16.0)),
        "Enter: submit │ Shift+Enter: newline │ Esc: normal",
        11.0,
        theme.fg_dim,
    );
    commands.entity(south).add_child(hints_widget);

    config.loaded = true;
    info!("Spawned widgets: title, connection (North); mode, hints (South)");
}

/// Helper to spawn a widget with text content.
fn spawn_widget(
    commands: &mut Commands,
    config: &mut WidgetConfig,
    theme: &Theme,
    widget: Widget,
    initial_text: &str,
    font_size: f32,
    color: Color,
) -> Entity {
    let widget_name = widget.name.clone();
    let min_width = widget.size.min_width.unwrap_or(60.0);
    let min_height = widget.size.min_height.unwrap_or(16.0);
    let has_padding = matches!(widget.content, WidgetContent::Mode | WidgetContent::Title);

    let mut entity = commands.spawn((
        widget,
        config.next_widget_id(),
        Node {
            padding: if has_padding {
                UiRect::all(Val::Px(8.0))
            } else {
                UiRect::ZERO
            },
            min_width: Val::Px(min_width),
            min_height: Val::Px(min_height),
            ..default()
        },
        BackgroundColor(if has_padding { theme.panel_bg } else { Color::NONE }),
    ));

    entity.with_children(|parent| {
        parent.spawn((
            WidgetText { widget_name },
            MsdfUiText::new(initial_text)
                .with_font_size(font_size)
                .with_color(color),
            UiTextPositionCache::default(),
            Node {
                min_width: Val::Px(if has_padding { min_width - 16.0 } else { min_width }),
                min_height: Val::Px(min_height),
                ..default()
            },
        ));
    });

    entity.id()
}

/// Update mode widget content when CurrentMode changes.
fn update_mode_widget(
    mode: Res<CurrentMode>,
    theme: Res<Theme>,
    mut widget_texts: Query<(&WidgetText, &mut MsdfUiText)>,
) {
    if !mode.is_changed() {
        return;
    }

    for (widget_text, mut msdf_text) in widget_texts.iter_mut() {
        if widget_text.widget_name != "mode" {
            continue;
        }

        let color = match mode.0 {
            EditorMode::Normal => theme.mode_normal,
            EditorMode::Input(InputKind::Chat) => theme.mode_chat,
            EditorMode::Input(InputKind::Shell) => theme.mode_shell,
            EditorMode::Visual => theme.mode_visual,
        };

        msdf_text.text = mode.0.name().to_string();
        msdf_text.color = bevy_to_rgba8(color);
    }
}

/// Update connection widget when RpcConnectionState changes.
fn update_connection_widget(
    conn_state: Res<RpcConnectionState>,
    theme: Res<Theme>,
    mut widget_texts: Query<(&WidgetText, &mut MsdfUiText)>,
) {
    if !conn_state.is_changed() {
        return;
    }

    for (widget_text, mut msdf_text) in widget_texts.iter_mut() {
        if widget_text.widget_name != "connection" {
            continue;
        }

        // Generate connection status text
        let (text, color) = if conn_state.connected {
            let status = conn_state
                .identity
                .as_ref()
                .map(|i| format!("✓ @{}", i.username))
                .unwrap_or_else(|| "✓ Connected".to_string());
            (status, theme.success)
        } else if conn_state.reconnect_attempt > 0 {
            (
                format!("⟳ Reconnecting ({})...", conn_state.reconnect_attempt),
                theme.warning,
            )
        } else {
            ("⚡ Disconnected".to_string(), theme.error)
        };

        msdf_text.text = text;
        msdf_text.color = bevy_to_rgba8(color);
    }
}

/// Update contexts widget when DriftState or DocumentCache changes.
///
/// Shows MRU context badges from DocumentCache, with active context highlighted.
/// Falls back to drift state contexts if DocumentCache is empty.
/// When a notification is active, temporarily shows the notification instead.
///
/// With cache: `[@abc main] [@def explore]  ·2 staged`
/// Notification: `← @abc: "Found the auth bug in..."`
fn update_contexts_widget(
    drift_state: Res<DriftState>,
    doc_cache: Res<crate::cell::DocumentCache>,
    theme: Res<Theme>,
    mut widget_texts: Query<(&WidgetText, &mut MsdfUiText)>,
) {
    if !drift_state.is_changed() && !doc_cache.is_changed() {
        return;
    }

    for (widget_text, mut msdf_text) in widget_texts.iter_mut() {
        if widget_text.widget_name != "contexts" {
            continue;
        }

        // If there's an active notification, show it instead of the context list
        if let Some(ref notif) = drift_state.notification {
            let text = format!("← @{}: \"{}\"", notif.source_ctx, notif.preview);
            msdf_text.text = text;
            msdf_text.color = bevy_to_rgba8(theme.accent);
            continue;
        }

        let mru_ids = doc_cache.mru_ids();
        let active_doc_id = doc_cache.active_id();

        // If we have cached documents, show MRU badges
        if !mru_ids.is_empty() {
            let mut parts: Vec<String> = Vec::new();
            let max_display = 5;

            for (i, doc_id) in mru_ids.iter().enumerate() {
                if i >= max_display {
                    let remaining = mru_ids.len() - max_display;
                    parts.push(format!("+{}", remaining));
                    break;
                }

                // Look up context name from cache
                let ctx_name = doc_cache
                    .get(doc_id)
                    .map(|c| c.context_name.as_str())
                    .unwrap_or("?");

                // Truncate long context names
                let short = if ctx_name.len() > 12 {
                    &ctx_name[..12]
                } else {
                    ctx_name
                };

                // Active context gets brackets
                if active_doc_id == Some(doc_id.as_str()) {
                    parts.push(format!("[{}]", short));
                } else {
                    parts.push(short.to_string());
                }
            }

            let mut text = parts.join(" ");

            // Append staged count if any
            let staged = drift_state.staged_count();
            if staged > 0 {
                text.push_str(&format!("  ·{} staged", staged));
            }

            msdf_text.text = text;
            msdf_text.color = bevy_to_rgba8(theme.accent);
            continue;
        }

        // Fall back to drift state contexts
        if drift_state.contexts.is_empty() {
            msdf_text.text = String::new();
            continue;
        }

        let mut parts: Vec<String> = Vec::new();
        let max_display = 5;

        for (i, ctx) in drift_state.contexts.iter().enumerate() {
            if i >= max_display {
                let remaining = drift_state.contexts.len() - max_display;
                parts.push(format!("+{} more", remaining));
                break;
            }
            parts.push(format!("@{}", ctx.short_id));
        }

        let mut text = parts.join(" ");

        // Append staged count if any
        let staged = drift_state.staged_count();
        if staged > 0 {
            text.push_str(&format!("  ·{} staged", staged));
        }

        // Use accent for the local context, fg_dim for rest
        let color = if drift_state.local_context_id.is_some() {
            theme.accent
        } else {
            theme.fg_dim
        };

        msdf_text.text = text;
        msdf_text.color = bevy_to_rgba8(color);
    }
}
