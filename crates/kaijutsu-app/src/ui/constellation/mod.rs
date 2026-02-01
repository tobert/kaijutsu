//! Constellation - Context navigation as a visual node graph
//!
//! The constellation replaces linear context navigation with a spatial model
//! inspired by 4X strategy games and skill trees. Contexts form nodes around
//! a central focus, with glowing connections showing relationships.
//!
//! ## View Modes
//!
//! - **Focused**: Just the center document, constellation hidden
//! - **Map**: Full constellation visible, center shrinks to ~60%
//! - **Orbital**: Contexts as animated orbiting rings
//!
//! ## Visual Design
//!
//! - Nodes: Glowing orbs with activity-based pulse
//! - Connections: Lines with distance falloff glow
//! - States: Idle (dim), active (bright), streaming (particle flow), error (red)
//! - "+" node: Create new contexts by clicking

mod create_dialog;
mod mini;
mod render;

use bevy::prelude::*;
use kaijutsu_client::SeatInfo;

// Re-export ModalDialogOpen for use by other systems (e.g., prompt input)
pub use create_dialog::ModalDialogOpen;

// Render module provides visual systems (used by the plugin internally)
// Mini module provides render-to-texture previews for constellation nodes

/// Plugin for constellation-based context navigation
pub struct ConstellationPlugin;

impl Plugin for ConstellationPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Constellation>()
            .init_resource::<OrbitalAnimation>()
            .register_type::<ConstellationMode>()
            .register_type::<ActivityState>()
            .register_type::<ConstellationContainer>()
            .register_type::<ConstellationNode>()
            .register_type::<ConstellationConnection>()
            .add_systems(
                Update,
                (
                    track_seat_events,
                    handle_mode_toggle,
                    handle_focus_navigation,
                    update_orbital_animation,
                    update_node_positions,
                )
                    .chain(),
            );

        // Add rendering systems from the render module
        render::setup_constellation_rendering(app);

        // Add mini-render systems for context previews
        mini::setup_mini_render_systems(app);

        // Add create context dialog systems
        create_dialog::setup_create_dialog_systems(app);
    }
}

// ============================================================================
// CORE DATA MODEL
// ============================================================================

/// Orbital animation state - decoupled from Constellation to avoid triggering
/// change detection on all Constellation readers every frame during orbital mode.
#[derive(Resource, Default)]
pub struct OrbitalAnimation {
    /// Current rotation angle in radians (accumulates over time)
    pub angle: f32,
    /// Whether orbital mode is active (cached to avoid reading Constellation)
    pub active: bool,
}

/// Constellation of contexts - the spatial navigation model
#[derive(Resource, Default)]
pub struct Constellation {
    /// All context nodes in the constellation
    pub nodes: Vec<ContextNode>,
    /// Currently focused context ID (center of constellation)
    pub focus_id: Option<String>,
    /// Visible relationship lines between nodes
    pub connections: Vec<Connection>,
    /// Current view mode
    pub mode: ConstellationMode,
    /// Layout algorithm for positioning nodes
    pub layout: LayoutStrategy,
    /// Alternate context ID (for Ctrl-^ switching)
    pub alternate_id: Option<String>,
}

impl Constellation {
    /// Get the currently focused node
    pub fn focused_node(&self) -> Option<&ContextNode> {
        self.focus_id
            .as_ref()
            .and_then(|id| self.nodes.iter().find(|n| &n.context_id == id))
    }

    /// Get a mutable reference to the focused node
    pub fn focused_node_mut(&mut self) -> Option<&mut ContextNode> {
        let focus_id = self.focus_id.clone();
        focus_id.and_then(move |id| self.nodes.iter_mut().find(|n| n.context_id == id))
    }

    /// Get node by context ID
    pub fn node_by_id(&self, id: &str) -> Option<&ContextNode> {
        self.nodes.iter().find(|n| n.context_id == id)
    }

    /// Get mutable node by context ID
    pub fn node_by_id_mut(&mut self, id: &str) -> Option<&mut ContextNode> {
        self.nodes.iter_mut().find(|n| n.context_id == id)
    }

    /// Add a new context node
    pub fn add_node(&mut self, seat_info: SeatInfo) {
        // Use context name as the unique identifier (not document_id which may be shared)
        let context_id = seat_info.id.context.clone();

        // Check if node already exists
        if self.node_by_id(&context_id).is_some() {
            info!("Constellation: Node for context {} already exists, skipping", context_id);
            return;
        }

        let node = ContextNode {
            context_id: context_id.clone(),
            seat_info,
            position: Vec2::ZERO, // Will be calculated by layout
            activity: ActivityState::default(),
            mini_render: None,
            entity: None,
        };

        self.nodes.push(node);

        // If no focus, set this as focus
        if self.focus_id.is_none() {
            self.focus_id = Some(context_id);
        }
    }

    /// Remove a context node
    pub fn remove_node(&mut self, context_id: &str) {
        self.nodes.retain(|n| n.context_id != context_id);

        // Update focus if we removed the focused node
        if self.focus_id.as_deref() == Some(context_id) {
            self.focus_id = self.nodes.first().map(|n| n.context_id.clone());
        }

        // Update alternate if we removed it
        if self.alternate_id.as_deref() == Some(context_id) {
            self.alternate_id = None;
        }
    }

    /// Switch focus to a different context
    pub fn focus(&mut self, context_id: &str) {
        if self.node_by_id(context_id).is_some() {
            // Save current focus as alternate
            if let Some(current) = self.focus_id.take() {
                if current != context_id {
                    self.alternate_id = Some(current);
                }
            }
            self.focus_id = Some(context_id.to_string());
        }
    }

    /// Switch to alternate context (Ctrl-^)
    pub fn toggle_alternate(&mut self) {
        if let Some(alt) = self.alternate_id.take() {
            let current = self.focus_id.take();
            self.focus_id = Some(alt);
            self.alternate_id = current;
        }
    }

    /// Get the next context ID (for gt navigation)
    pub fn next_context_id(&self) -> Option<&str> {
        let focus_idx = self.focus_id.as_ref().and_then(|id| {
            self.nodes.iter().position(|n| &n.context_id == id)
        })?;

        let next_idx = (focus_idx + 1) % self.nodes.len();
        Some(&self.nodes[next_idx].context_id)
    }

    /// Get the previous context ID (for gT navigation)
    pub fn prev_context_id(&self) -> Option<&str> {
        let focus_idx = self.focus_id.as_ref().and_then(|id| {
            self.nodes.iter().position(|n| &n.context_id == id)
        })?;

        let prev_idx = if focus_idx == 0 {
            self.nodes.len() - 1
        } else {
            focus_idx - 1
        };
        Some(&self.nodes[prev_idx].context_id)
    }
}

/// A node in the constellation representing a context
#[derive(Clone)]
pub struct ContextNode {
    /// Unique context identifier (document_id from seat)
    pub context_id: String,
    /// Full seat information from server
    pub seat_info: SeatInfo,
    /// Position in constellation space (calculated by layout)
    pub position: Vec2,
    /// Current activity state (affects visual rendering)
    pub activity: ActivityState,
    /// Cached mini-render texture (for Map mode)
    pub mini_render: Option<Handle<Image>>,
    /// Entity ID when spawned
    pub entity: Option<Entity>,
}

/// Connection line between nodes
#[derive(Clone, Debug)]
pub struct Connection {
    /// Source node context ID
    pub from: String,
    /// Target node context ID
    pub to: String,
    /// Connection type (affects visual style)
    pub kind: ConnectionKind,
    /// Glow intensity (0.0-1.0, based on activity)
    pub glow_intensity: f32,
}

/// Type of connection between nodes
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConnectionKind {
    /// Normal relationship (same kernel)
    #[default]
    Related,
    /// Parent-child (forked from)
    ParentChild,
    /// Data flow (streaming from one to another)
    DataFlow,
}

/// Activity state of a context node
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Reflect)]
pub enum ActivityState {
    /// No activity, dim glow
    #[default]
    Idle,
    /// User is actively working here
    Active,
    /// Agent is streaming output
    Streaming,
    /// Waiting for response
    Waiting,
    /// Error occurred
    Error,
    /// Task completed recently
    Completed,
}

impl ActivityState {
    /// Get the glow intensity for this state
    pub fn glow_intensity(&self) -> f32 {
        match self {
            Self::Idle => 0.2,
            Self::Active => 0.6,
            Self::Streaming => 0.9,
            Self::Waiting => 0.5,
            Self::Error => 0.8,
            Self::Completed => 0.7,
        }
    }
}

/// View mode for the constellation
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Reflect)]
pub enum ConstellationMode {
    /// Just the focus document, constellation hidden
    #[default]
    Focused,
    /// Full constellation visible, center shrinks to ~60%
    Map,
    /// Contexts as animated orbiting rings
    Orbital,
}

impl ConstellationMode {
    /// Cycle to the next mode
    pub fn next(&self) -> Self {
        match self {
            Self::Focused => Self::Map,
            Self::Map => Self::Orbital,
            Self::Orbital => Self::Focused,
        }
    }
}

/// Strategy for laying out nodes in the constellation
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LayoutStrategy {
    /// Circular layout around center
    #[default]
    Circular,
    /// Force-directed graph layout
    ForceDirected,
    /// Manual positions (user-arranged)
    Manual,
}

// ============================================================================
// SYSTEMS
// ============================================================================

/// Track seat events to add/remove constellation nodes
fn track_seat_events(
    mut constellation: ResMut<Constellation>,
    mut events: MessageReader<crate::connection::ConnectionEvent>,
) {
    use crate::connection::ConnectionEvent;

    for event in events.read() {
        match event {
            ConnectionEvent::SeatTaken { seat } => {
                info!("Constellation: Adding node for context '{}' (doc: {})", seat.id.context, seat.document_id);
                constellation.add_node(seat.clone());
            }
            ConnectionEvent::SeatLeft => {
                // We don't remove nodes on SeatLeft - contexts persist
                // They just become "idle" in the constellation
                if let Some(node) = constellation.focused_node_mut() {
                    node.activity = ActivityState::Idle;
                }
            }
            _ => {}
        }
    }
}

/// Handle Tab/Space to cycle constellation mode
fn handle_mode_toggle(
    keys: Res<ButtonInput<KeyCode>>,
    screen: Res<State<crate::ui::state::AppScreen>>,
    current_mode: Res<crate::cell::CurrentMode>,
    modal_open: Res<ModalDialogOpen>,
    mut constellation: ResMut<Constellation>,
) {
    // Skip when a modal dialog is open
    if modal_open.0 {
        return;
    }

    // Only in Conversation state and Normal mode
    if *screen.get() != crate::ui::state::AppScreen::Conversation {
        return;
    }
    if current_mode.0 != crate::cell::EditorMode::Normal {
        return;
    }

    // Tab toggles constellation mode
    if keys.just_pressed(KeyCode::Tab) {
        constellation.mode = constellation.mode.next();
        info!("Constellation mode: {:?}", constellation.mode);
    }
}

/// Handle gt/gT and Ctrl-^ for context navigation
fn handle_focus_navigation(
    keys: Res<ButtonInput<KeyCode>>,
    screen: Res<State<crate::ui::state::AppScreen>>,
    current_mode: Res<crate::cell::CurrentMode>,
    modal_open: Res<ModalDialogOpen>,
    mut constellation: ResMut<Constellation>,
    mut pending_g: Local<bool>,
) {
    // Skip when a modal dialog is open
    if modal_open.0 {
        return;
    }

    // Only in Conversation state and Normal mode
    if *screen.get() != crate::ui::state::AppScreen::Conversation {
        return;
    }
    if current_mode.0 != crate::cell::EditorMode::Normal {
        return;
    }

    // Ctrl-^ (Ctrl-6) for alternate
    if keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight) {
        if keys.just_pressed(KeyCode::Digit6) {
            constellation.toggle_alternate();
            info!("Switched to alternate context");
            return;
        }
    }

    // gt/gT navigation (g then t or T)
    if keys.just_pressed(KeyCode::KeyG) {
        *pending_g = true;
        return;
    }

    if *pending_g {
        if keys.just_pressed(KeyCode::KeyT) {
            *pending_g = false;
            let shift = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);
            if shift {
                // gT = previous
                if let Some(id) = constellation.prev_context_id().map(|s| s.to_string()) {
                    constellation.focus(&id);
                    info!("Focus: previous context {}", id);
                }
            } else {
                // gt = next
                if let Some(id) = constellation.next_context_id().map(|s| s.to_string()) {
                    constellation.focus(&id);
                    info!("Focus: next context {}", id);
                }
            }
        } else if keys.any_just_pressed([
            KeyCode::Escape,
            KeyCode::KeyA,
            KeyCode::KeyB,
            KeyCode::KeyC,
        ]) {
            // Cancel g prefix on other keys
            *pending_g = false;
        }
    }
}

/// Update orbital animation state (runs every frame in orbital mode, but doesn't
/// trigger change detection on Constellation)
fn update_orbital_animation(
    constellation: Res<Constellation>,
    time: Res<Time>,
    theme: Res<crate::ui::theme::Theme>,
    mut orbital: ResMut<OrbitalAnimation>,
) {
    let is_orbital = constellation.mode == ConstellationMode::Orbital;

    // Track mode changes
    if orbital.active != is_orbital {
        orbital.active = is_orbital;
    }

    // Accumulate angle in orbital mode
    if is_orbital {
        orbital.angle += time.delta_secs() * theme.constellation_orbital_speed;
        // Wrap to prevent float precision issues over long sessions
        if orbital.angle > std::f32::consts::TAU {
            orbital.angle -= std::f32::consts::TAU;
        }
    }
}

/// Update node positions based on layout strategy
/// Only mutates Constellation when layout actually changes (not every frame in orbital)
fn update_node_positions(
    mut constellation: ResMut<Constellation>,
    orbital: Res<OrbitalAnimation>,
    theme: Res<crate::ui::theme::Theme>,
) {
    let node_count = constellation.nodes.len();
    if node_count == 0 {
        return;
    }

    // Only recalculate positions when:
    // - Constellation data changed (new node, focus change, etc.)
    // - Orbital animation is active AND orbital angle changed
    let needs_update = constellation.is_changed() || (orbital.active && orbital.is_changed());
    if !needs_update {
        return;
    }

    let orbital_offset = if orbital.active { orbital.angle } else { 0.0 };

    match constellation.layout {
        LayoutStrategy::Circular => {
            // Position nodes in a circle around center
            let radius = theme.constellation_layout_radius;
            let angle_step = std::f32::consts::TAU / node_count as f32;

            for (i, node) in constellation.nodes.iter_mut().enumerate() {
                let base_angle = angle_step * i as f32 - std::f32::consts::FRAC_PI_2; // Start at top
                let angle = base_angle + orbital_offset;
                node.position = Vec2::new(angle.cos() * radius, angle.sin() * radius);
            }
        }
        LayoutStrategy::ForceDirected => {
            // TODO: Implement force-directed layout
            // For now, fall back to circular
        }
        LayoutStrategy::Manual => {
            // Don't update positions - user has arranged them
        }
    }
}

// ============================================================================
// MARKERS
// ============================================================================

/// Marker for the constellation container entity
#[derive(Component, Reflect, Default)]
#[reflect(Component)]
pub struct ConstellationContainer;

/// Marker for a constellation node entity
#[derive(Component, Reflect, Default)]
#[reflect(Component)]
pub struct ConstellationNode {
    pub context_id: String,
}

/// Marker for a constellation connection line entity
#[derive(Component, Reflect, Default)]
#[reflect(Component)]
pub struct ConstellationConnection {
    pub from: String,
    pub to: String,
}
