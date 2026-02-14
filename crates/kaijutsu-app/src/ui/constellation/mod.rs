//! Constellation - Context navigation as a full-screen radial tree graph
//!
//! The constellation replaces linear context navigation with a spatial model
//! inspired by 4X strategy games and skill trees. Contexts form nodes in a
//! radial tree layout, with the root at center and children radiating outward.
//!
//! ## Activation
//!
//! Tab toggles between conversation view and full-screen constellation.
//! The constellation takes over the content area, hiding conversation panes.
//! Enter on a focused node switches context and returns to conversation.
//!
//! ## Visual Design
//!
//! - Nodes: Glowing orbs with activity-based pulse
//! - Connections: Lines with distance falloff glow
//! - States: Idle (dim), active (bright), streaming (particle flow), error (red)
//! - "+" node: Create new contexts by clicking

mod create_dialog;
mod mini;
pub mod model_picker;
mod render;

use bevy::prelude::*;
use kaijutsu_client::ContextMembership;

use crate::agents::AgentActivityMessage;

pub use create_dialog::{DialogMode, OpenContextDialog};

// Render module provides visual systems (used by the plugin internally)
// Mini module provides render-to-texture previews for constellation nodes

/// Plugin for constellation-based context navigation
pub struct ConstellationPlugin;

impl Plugin for ConstellationPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Constellation>()
            .init_resource::<ConstellationVisible>()
            .init_resource::<ConstellationCamera>()
            .register_type::<ConstellationVisible>()
            .register_type::<ActivityState>()
            .register_type::<ConstellationContainer>()
            .register_type::<ConstellationNode>()
            .register_type::<ConstellationConnection>()
            .register_type::<DriftConnectionKind>()
            .register_type::<ConstellationCamera>()
            .add_systems(
                Update,
                (
                    track_seat_events,
                    track_agent_activity,
                    // Input handling in input::systems (toggle_constellation + constellation_nav)
                    handle_node_click,
                    update_node_positions,
                    interpolate_camera,
                )
                    .chain(),
            );

        // Add rendering systems from the render module
        render::setup_constellation_rendering(app);

        // Add mini-render systems for context previews
        mini::setup_mini_render_systems(app);

        // Add create context dialog systems
        create_dialog::setup_create_dialog_systems(app);

        // Add model picker systems
        model_picker::setup_model_picker_systems(app);
    }
}

// ============================================================================
// CORE DATA MODEL
// ============================================================================

/// Whether the constellation view is visible (full-takeover of content area).
#[derive(Resource, Default, Reflect)]
#[reflect(Resource)]
pub struct ConstellationVisible(pub bool);

/// Camera for constellation pan/zoom.
///
/// Offset and zoom are smoothly interpolated toward their targets each frame.
#[derive(Resource, Reflect)]
#[reflect(Resource)]
pub struct ConstellationCamera {
    /// Current pan offset in pixels
    pub offset: Vec2,
    /// Current zoom level (1.0 = normal)
    pub zoom: f32,
    /// Target pan offset for smooth interpolation
    pub target_offset: Vec2,
    /// Target zoom level for smooth interpolation
    pub target_zoom: f32,
    /// Interpolation speed (higher = snappier)
    pub speed: f32,
}

impl Default for ConstellationCamera {
    fn default() -> Self {
        Self {
            offset: Vec2::ZERO,
            zoom: 1.0,
            target_offset: Vec2::ZERO,
            target_zoom: 1.0,
            speed: 8.0,
        }
    }
}

impl ConstellationCamera {
    /// Reset camera to default view
    pub fn reset(&mut self) {
        self.target_offset = Vec2::ZERO;
        self.target_zoom = 1.0;
    }
}


/// Constellation of contexts - the spatial navigation model
#[derive(Resource, Default)]
pub struct Constellation {
    /// All context nodes in the constellation
    pub nodes: Vec<ContextNode>,
    /// Currently focused context ID (center of constellation)
    pub focus_id: Option<String>,
    /// Alternate context ID (for Ctrl-^ switching)
    pub alternate_id: Option<String>,
}

impl Constellation {
    /// Get a mutable reference to the focused node
    pub fn focused_node_mut(&mut self) -> Option<&mut ContextNode> {
        let focus_id = self.focus_id.clone();
        focus_id.and_then(move |id| self.nodes.iter_mut().find(|n| n.context_id == id))
    }

    /// Get node by context ID
    pub fn node_by_id(&self, id: &str) -> Option<&ContextNode> {
        self.nodes.iter().find(|n| n.context_id == id)
    }

    /// Add a node for a context we've actively joined (have an actor connection).
    pub fn add_node(&mut self, membership: &ContextMembership) {
        let context_id = &membership.context_name;

        // If node already exists (e.g. from DriftState), mark it as joined
        if let Some(node) = self.nodes.iter_mut().find(|n| n.context_id == *context_id) {
            if !node.joined {
                info!("Constellation: Marking existing node {} as joined", context_id);
                node.joined = true;
            }
            return;
        }

        let node = ContextNode {
            context_id: context_id.clone(),
            parent_id: None, // Populated by sync_model_info_to_constellation
            position: Vec2::ZERO, // Will be calculated by layout
            activity: ActivityState::default(),
            entity: None,
            model: None,
            joined: true,
        };

        self.nodes.push(node);

        // If no focus, set this as focus
        if self.focus_id.is_none() {
            self.focus_id = Some(context_id.clone());
        }
    }

    /// Add a placeholder node from DriftState context info (not yet joined).
    pub fn add_node_from_context_info(&mut self, ctx_info: &kaijutsu_client::ContextInfo) {
        let context_id = &ctx_info.name;

        if self.node_by_id(context_id).is_some() {
            return;
        }

        let node = ContextNode {
            context_id: context_id.clone(),
            parent_id: ctx_info.parent_id.clone(),
            position: Vec2::ZERO,
            activity: ActivityState::Idle,
            entity: None,
            model: if ctx_info.model.is_empty() { None } else { Some(ctx_info.model.clone()) },
            joined: false,
        };

        self.nodes.push(node);

        // If no focus, set this as focus
        if self.focus_id.is_none() {
            self.focus_id = Some(context_id.clone());
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
    #[allow(dead_code)]
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
    /// Unique context identifier (context name)
    pub context_id: String,
    /// Parent context ID (from drift router, for tree layout)
    pub parent_id: Option<String>,
    /// Position in constellation space (calculated by layout)
    pub position: Vec2,
    /// Current activity state (affects visual rendering)
    pub activity: ActivityState,
    /// Entity ID when spawned
    pub entity: Option<Entity>,
    /// Model name from DriftState polling (e.g. "claude-sonnet-4-5")
    pub model: Option<String>,
    /// Whether we have an active actor connection to this context
    pub joined: bool,
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



// ============================================================================
// SYSTEMS
// ============================================================================

/// Track context join events to add/update constellation nodes.
fn track_seat_events(
    mut constellation: ResMut<Constellation>,
    mut events: MessageReader<crate::connection::RpcResultMessage>,
) {
    use crate::connection::RpcResultMessage;

    for event in events.read() {
        match event {
            RpcResultMessage::ContextJoined { membership, .. } => {
                info!("Constellation: Adding node for context '{}' (kernel: {})", membership.context_name, membership.kernel_id);
                constellation.add_node(membership);
            }
            RpcResultMessage::ContextLeft => {
                // We don't remove nodes on ContextLeft - contexts persist
                // They just become "idle" in the constellation
                if let Some(node) = constellation.focused_node_mut() {
                    node.activity = ActivityState::Idle;
                }
            }
            _ => {}
        }
    }
}

/// Track agent activity events to update node visual state.
///
/// When agents start/complete work, update the corresponding node's
/// ActivityState to provide visual feedback in the constellation.
fn track_agent_activity(
    mut constellation: ResMut<Constellation>,
    mut events: MessageReader<AgentActivityMessage>,
) {
    for event in events.read() {
        // Update the focused node's activity based on agent events
        // (In the future, we could map block_id to context for more precision)
        match event {
            AgentActivityMessage::Started { nick, action, .. } => {
                info!("Agent {} started: {}", nick, action);
                if let Some(node) = constellation.focused_node_mut() {
                    node.activity = ActivityState::Streaming;
                }
            }
            AgentActivityMessage::Progress { .. } => {
                // Keep streaming state during progress
                if let Some(node) = constellation.focused_node_mut() {
                    if node.activity != ActivityState::Streaming {
                        node.activity = ActivityState::Streaming;
                    }
                }
            }
            AgentActivityMessage::Completed { success, .. } => {
                if let Some(node) = constellation.focused_node_mut() {
                    node.activity = if *success {
                        ActivityState::Completed
                    } else {
                        ActivityState::Error
                    };
                }
            }
            AgentActivityMessage::CursorMoved { .. } => {
                // Cursor movement indicates active editing
                if let Some(node) = constellation.focused_node_mut() {
                    if node.activity == ActivityState::Idle {
                        node.activity = ActivityState::Active;
                    }
                }
            }
        }
    }
}

/// Handle clicks on constellation nodes to focus that context.
fn handle_node_click(
    mut constellation: ResMut<Constellation>,
    mut switch_writer: MessageWriter<crate::cell::ContextSwitchRequested>,
    nodes: Query<(&Interaction, &ConstellationNode), Changed<Interaction>>,
) {
    for (interaction, node) in nodes.iter() {
        if *interaction == Interaction::Pressed {
            // Don't switch if already focused
            if constellation.focus_id.as_deref() == Some(&node.context_id) {
                continue;
            }

            info!("Clicked constellation node: {}", node.context_id);
            constellation.focus(&node.context_id);
            switch_writer.write(crate::cell::ContextSwitchRequested {
                context_name: node.context_id.clone(),
            });
        }
    }
}

// Input handling in input::systems (toggle_constellation + constellation_nav)

/// Update node positions using radial tree layout.
///
/// Root nodes (no parent) are placed at center. Children radiate outward in
/// concentric rings, with angular sectors proportional to descendant count.
fn update_node_positions(
    mut constellation: ResMut<Constellation>,
    theme: Res<crate::ui::theme::Theme>,
) {
    let node_count = constellation.nodes.len();
    if node_count == 0 {
        return;
    }

    if !constellation.is_changed() {
        return;
    }

    let base_radius = theme.constellation_base_radius;
    let ring_spacing = theme.constellation_ring_spacing;

    // Build parent→children adjacency and identify roots
    let ids: Vec<String> = constellation.nodes.iter().map(|n| n.context_id.clone()).collect();
    let parents: Vec<Option<String>> = constellation.nodes.iter().map(|n| n.parent_id.clone()).collect();

    // Map context_id → index
    let id_to_idx: std::collections::HashMap<&str, usize> = ids.iter().enumerate()
        .map(|(i, id)| (id.as_str(), i))
        .collect();

    // Build children lists
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); node_count];
    let mut roots: Vec<usize> = Vec::new();

    for (i, parent) in parents.iter().enumerate() {
        if let Some(pid) = parent {
            if let Some(&parent_idx) = id_to_idx.get(pid.as_str()) {
                children[parent_idx].push(i);
            } else {
                // Parent not in constellation — treat as root
                roots.push(i);
            }
        } else {
            roots.push(i);
        }
    }

    // If no roots found (shouldn't happen), treat all as roots
    if roots.is_empty() {
        roots = (0..node_count).collect();
    }

    // Stable sort: children by context_id so tree layout is deterministic
    for ch in &mut children {
        ch.sort_by(|a, b| ids[*a].cmp(&ids[*b]));
    }
    roots.sort_by(|a, b| ids[*a].cmp(&ids[*b]));

    // BFS layout: assign positions
    let mut positions: Vec<Vec2> = vec![Vec2::ZERO; node_count];

    if roots.len() == 1 {
        // Single root at center
        positions[roots[0]] = Vec2::ZERO;
        layout_children(roots[0], 0.0, std::f32::consts::TAU, 1, base_radius, ring_spacing, &children, &mut positions);
    } else {
        // Multiple roots: distribute around center at ring 0 (or small offset)
        let total_desc: usize = roots.iter().map(|&r| count_tree_descendants(r, &children)).sum();
        let mut angle_start = -std::f32::consts::FRAC_PI_2;
        for &root_idx in &roots {
            let desc = count_tree_descendants(root_idx, &children);
            let sector = std::f32::consts::TAU * (desc as f32 / total_desc.max(1) as f32);
            let mid_angle = angle_start + sector / 2.0;

            // Place root at a small radius to separate them
            let root_radius = if roots.len() > 1 { base_radius * 0.5 } else { 0.0 };
            positions[root_idx] = Vec2::new(mid_angle.cos() * root_radius, mid_angle.sin() * root_radius);

            layout_children(root_idx, angle_start, sector, 1, base_radius, ring_spacing, &children, &mut positions);
            angle_start += sector;
        }
    }

    // Apply positions back to nodes
    for (i, node) in constellation.nodes.iter_mut().enumerate() {
        node.position = positions[i];
    }
}

/// Count descendants (including self) for angular sector sizing.
/// Depth-limited to prevent stack overflow from malformed cyclic parentage data.
fn count_tree_descendants(idx: usize, children: &[Vec<usize>]) -> usize {
    count_tree_descendants_inner(idx, children, 0)
}

fn count_tree_descendants_inner(idx: usize, children: &[Vec<usize>], depth: usize) -> usize {
    const MAX_DEPTH: usize = 64;
    if depth >= MAX_DEPTH {
        return 1;
    }
    let mut count = 1; // self
    for &child in &children[idx] {
        count += count_tree_descendants_inner(child, children, depth + 1);
    }
    count
}

/// Recursively layout children in angular sectors at increasing ring depths.
fn layout_children(
    parent_idx: usize,
    angle_start: f32,
    sector: f32,
    depth: usize,
    base_radius: f32,
    ring_spacing: f32,
    children: &[Vec<usize>],
    positions: &mut [Vec2],
) {
    let child_indices = &children[parent_idx];
    if child_indices.is_empty() {
        return;
    }

    let radius = base_radius + depth as f32 * ring_spacing;

    let total_desc: usize = child_indices.iter().map(|&c| count_tree_descendants(c, children)).sum();
    let mut current_angle = angle_start;

    for &child_idx in child_indices {
        let desc = count_tree_descendants(child_idx, children);
        let child_sector = sector * (desc as f32 / total_desc.max(1) as f32);
        let mid_angle = current_angle + child_sector / 2.0;

        positions[child_idx] = Vec2::new(mid_angle.cos() * radius, mid_angle.sin() * radius);

        // Recurse for grandchildren
        layout_children(child_idx, current_angle, child_sector, depth + 1, base_radius, ring_spacing, children, positions);

        current_angle += child_sector;
    }
}

/// Smoothly interpolate camera offset and zoom toward targets.
fn interpolate_camera(
    mut camera: ResMut<ConstellationCamera>,
    visible: Res<ConstellationVisible>,
    time: Res<Time>,
) {
    if !visible.0 {
        return;
    }

    let dt = time.delta_secs();
    let t = (camera.speed * dt).min(1.0);

    let offset_diff = camera.target_offset - camera.offset;
    if offset_diff.length() > 0.1 {
        camera.offset += offset_diff * t;
    } else {
        camera.offset = camera.target_offset;
    }

    let zoom_diff = camera.target_zoom - camera.zoom;
    if zoom_diff.abs() > 0.001 {
        camera.zoom += zoom_diff * t;
    } else {
        camera.zoom = camera.target_zoom;
    }
}

/// Find the nearest constellation node in a given direction from the focused node.
///
/// Filters nodes to the correct half-plane (dot product with direction > 0),
/// then scores by `distance / cos_angle` to prefer closer, more on-axis nodes.
pub fn find_nearest_in_direction(constellation: &Constellation, direction: Vec2) -> Option<String> {
    let focus_pos = constellation.focus_id.as_ref()
        .and_then(|id| constellation.node_by_id(id))
        .map(|n| n.position)?;

    let mut best: Option<(f32, &str)> = None;

    for node in &constellation.nodes {
        if constellation.focus_id.as_deref() == Some(&node.context_id) {
            continue;
        }

        let delta = node.position - focus_pos;
        let dist = delta.length();
        if dist < 0.001 {
            continue;
        }

        let cos_angle = delta.dot(direction) / dist;
        if cos_angle <= 0.0 {
            continue; // Wrong half-plane
        }

        let score = dist / cos_angle.max(0.01);

        if best.is_none() || score < best.unwrap().0 {
            best = Some((score, &node.context_id));
        }
    }

    best.map(|(_, id)| id.to_string())
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

/// What kind of connection this line represents.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Reflect)]
pub enum DriftConnectionKind {
    /// Parent-child ancestry from fork/thread
    #[default]
    Ancestry,
    /// Active staged drift between contexts
    StagedDrift,
}

/// Marker for a constellation connection line entity
#[derive(Component, Reflect, Default)]
#[reflect(Component)]
pub struct ConstellationConnection {
    pub from: String,
    pub to: String,
    pub kind: DriftConnectionKind,
}
