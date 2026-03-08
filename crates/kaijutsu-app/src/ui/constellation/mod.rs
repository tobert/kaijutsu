//! Constellation - Context navigation in 2.5D hyperbolic space
//!
//! Renders the constellation as an H3-inspired hyperbolic cone tree projected
//! to the Poincaré ball, visualized using pure Vello 2D with depth cues for
//! a 2.5D feel. Focus changes are Lorentz boosts — the space moves through
//! the camera, providing navigational stability.
//!
//! ## Architecture
//!
//! - `hyper.rs` — Hyperbolic math (HyperPoint, LorentzTransform, Poincaré projection)
//! - `layout.rs` — H3 layout engine (bottom-up hemisphere sizing, top-down placement)
//! - `render2d.rs` — Vello scene building with depth-based size/opacity/glow
//! - `navigation.rs` — Focus animation via geodesic lerp
//!
//! ## Activation
//!
//! Tab toggles between conversation view and full-screen constellation.
//! The constellation takes over the content area, hiding conversation panes.
//! Enter on a focused node switches context and returns to conversation.
//!
//! ## Navigation
//!
//! - `hjkl` — Spatial focus navigation (Lorentz boost to nearest node)
//! - `Shift+hjkl` — Orbit camera around Poincaré ball
//! - `+/-` — Zoom (orbit distance)
//! - `0` — Reset camera to default view
//! - `Enter` — Switch to focused context
//! - `f` — Fork focused context
//! - `m` — Model picker for focused context

mod create_dialog;
pub mod fork_form;
mod legend;
pub mod model_picker;
mod render2d;

use bevy::prelude::*;
use kaijutsu_client::ContextMembership;
use kaijutsu_types::ContextId;

use crate::agents::AgentActivityMessage;

pub use create_dialog::create_or_fork_context;
pub use fork_form::OpenForkForm;

// Render module provides visual systems (used by the plugin internally)

/// Configuration for the "New" context tile.
///
/// Tools can set `parent_context` via BRP to make "New" fork from a
/// starter/template context instead of creating empty. Future: per-repo
/// starters, template pinning, etc.
#[derive(Resource, Default, Reflect)]
#[reflect(Resource)]
pub struct NewContextConfig {
    /// When set, "New" forks from this context instead of creating empty.
    pub parent_context: Option<String>,
}

/// Plugin for constellation-based context navigation
pub struct ConstellationPlugin;

impl Plugin for ConstellationPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Constellation>()
            .init_resource::<ConstellationCamera>()
            .init_resource::<NewContextConfig>()
            .register_type::<NewContextConfig>()
            .register_type::<ActivityState>()
            .register_type::<ConstellationContainer>()
            .register_type::<ConstellationNode>()
            .register_type::<ConstellationCamera>()
            .add_systems(
                Update,
                (
                    track_context_events,
                    track_agent_activity,
                    // Input handling in input::systems (toggle_constellation + constellation_nav)
                    handle_node_click,
                    update_node_positions,
                    interpolate_camera,
                )
                    .chain(),
            );

        // Constellation container + legend panel (extracted from deleted render.rs)
        legend::setup_legend_systems(app);

        // Add 2D Vello rendering systems
        render2d::setup_render2d_systems(app);

        // Add create context dialog systems
        create_dialog::setup_create_dialog_systems(app);

        // Add model picker systems
        model_picker::setup_model_picker_systems(app);

        // Add fork form systems (full-viewport fork configuration)
        fork_form::setup_fork_form_systems(app);

        // Form primitives (selectable list, tree view, form field sync)
        app.add_plugins(crate::ui::form::FormPlugin);
    }
}

// ============================================================================
// CORE DATA MODEL
// ============================================================================

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
    /// Current carousel rotation angle (radians)
    pub carousel_angle: f32,
    /// Target carousel rotation for smooth interpolation
    pub target_carousel_angle: f32,
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
            carousel_angle: 0.0,
            target_carousel_angle: 0.0,
            speed: 8.0,
        }
    }
}

impl ConstellationCamera {
    /// Reset camera to default view
    pub fn reset(&mut self) {
        self.target_offset = Vec2::ZERO;
        self.target_zoom = 1.0;
        self.target_carousel_angle = 0.0;
    }
}


/// Constellation of contexts - the spatial navigation model
#[derive(Resource, Default)]
pub struct Constellation {
    /// All context nodes in the constellation
    pub nodes: Vec<ContextNode>,
    /// Currently focused context ID (center of constellation)
    pub focus_id: Option<ContextId>,
    /// Alternate context ID (for Ctrl-^ switching)
    pub alternate_id: Option<ContextId>,
}

impl Constellation {
    /// Get a mutable reference to the focused node
    pub fn focused_node_mut(&mut self) -> Option<&mut ContextNode> {
        let focus_id = self.focus_id?;
        self.nodes.iter_mut().find(|n| n.context_id == focus_id)
    }

    /// Get node by context ID
    pub fn node_by_id(&self, id: ContextId) -> Option<&ContextNode> {
        self.nodes.iter().find(|n| n.context_id == id)
    }

    /// Find node by block_id string (format: `{context_hex}_{agent_hex}_{seq}`).
    ///
    /// Extracts the context hex prefix and matches against node context_id.
    /// Falls back to the focused node if the block_id can't be parsed.
    fn node_by_block_id_mut(&mut self, block_id: &str) -> Option<&mut ContextNode> {
        let ctx_hex = block_id.split('_').next()?;
        if let Ok(ctx_id) = ContextId::parse(ctx_hex) {
            if let Some(idx) = self.nodes.iter().position(|n| n.context_id == ctx_id) {
                return Some(&mut self.nodes[idx]);
            }
        }
        // Fallback to focused node
        let focus_id = self.focus_id?;
        self.nodes.iter_mut().find(|n| n.context_id == focus_id)
    }

    /// Add a node for a context we've actively joined (have an actor connection).
    pub fn add_node(&mut self, membership: &ContextMembership) {
        let context_id = membership.context_id;

        // If node already exists (e.g. from DriftState), mark it as joined
        if let Some(node) = self.nodes.iter_mut().find(|n| n.context_id == context_id) {
            if !node.joined {
                info!("Constellation: Marking existing node {} as joined", context_id);
                node.joined = true;
            }
            return;
        }

        let node = ContextNode {
            context_id,
            parent_id: None, // Populated by sync_model_info_to_constellation
            label: None,     // Populated by sync_model_info_to_constellation
            position: Vec2::ZERO,
            depth: 0.0,
            ring_index: self.nodes.len(),
            activity: ActivityState::default(),
            model: None,
            provider: None,
            joined: true,
            last_activity_time: 0.0,
        };

        self.nodes.push(node);

        // If no focus, set this as focus
        if self.focus_id.is_none() {
            self.focus_id = Some(context_id);
        }
    }

    /// Add a placeholder node from DriftState context info (not yet joined).
    pub fn add_node_from_context_info(&mut self, ctx_info: &kaijutsu_client::ContextInfo) {
        let context_id = ctx_info.id;

        if self.node_by_id(context_id).is_some() {
            return;
        }

        let node = ContextNode {
            context_id,
            parent_id: ctx_info.parent_id,
            label: if ctx_info.label.is_empty() { None } else { Some(ctx_info.label.clone()) },
            position: Vec2::ZERO,
            depth: 0.0,
            ring_index: self.nodes.len(),
            activity: ActivityState::Idle,
            model: if ctx_info.model.is_empty() { None } else { Some(ctx_info.model.clone()) },
            provider: if ctx_info.provider.is_empty() { None } else { Some(ctx_info.provider.clone()) },
            joined: false,
            last_activity_time: 0.0,
        };

        self.nodes.push(node);

        // If no focus, set this as focus
        if self.focus_id.is_none() {
            self.focus_id = Some(context_id);
        }
    }

    /// Switch focus to a different context
    pub fn focus(&mut self, context_id: ContextId) {
        if self.node_by_id(context_id).is_some() {
            // Save current focus as alternate
            if let Some(current) = self.focus_id.take() {
                if current != context_id {
                    self.alternate_id = Some(current);
                }
            }
            self.focus_id = Some(context_id);
        }
    }

}

/// A node in the constellation representing a context
#[derive(Clone)]
pub struct ContextNode {
    /// Unique context identifier
    pub context_id: ContextId,
    /// Parent context ID (from drift router, for tree layout)
    pub parent_id: Option<ContextId>,
    /// Human-readable label (e.g. "default", "debug-auth")
    pub label: Option<String>,
    /// Position in constellation space (calculated by layout)
    pub position: Vec2,
    /// Depth in carousel space: 1.0 = front (closest), -1.0 = back (furthest)
    pub depth: f32,
    /// Index in the ring order (for h/l navigation)
    pub ring_index: usize,
    /// Current activity state (affects visual rendering)
    pub activity: ActivityState,
    /// Model name from DriftState polling (e.g. "claude-sonnet-4-5")
    pub model: Option<String>,
    /// LLM provider name (e.g. "anthropic", "google", "deepseek") for agent coloring
    pub provider: Option<String>,
    /// Whether we have an active actor connection to this context
    pub joined: bool,
    /// When activity last changed (from `Time::elapsed_secs_f64()`)
    pub last_activity_time: f64,
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


// ============================================================================
// SYSTEMS
// ============================================================================

/// Track context join events to add/update constellation nodes.
fn track_context_events(
    mut constellation: ResMut<Constellation>,
    mut events: MessageReader<crate::connection::RpcResultMessage>,
) {
    use crate::connection::RpcResultMessage;

    for event in events.read() {
        match event {
            RpcResultMessage::ContextJoined { membership, .. } => {
                info!("Constellation: Adding node for context '{}' (kernel: {})", membership.context_id, membership.kernel_id);
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
    time: Res<Time>,
) {
    let now = time.elapsed_secs_f64();
    for event in events.read() {
        match event {
            AgentActivityMessage::Started { nick, action, block_id } => {
                info!("Agent {} started: {}", nick, action);
                if let Some(node) = constellation.node_by_block_id_mut(block_id) {
                    node.activity = ActivityState::Streaming;
                    node.last_activity_time = now;
                }
            }
            AgentActivityMessage::Progress { block_id, .. } => {
                if let Some(node) = constellation.node_by_block_id_mut(block_id) {
                    if node.activity != ActivityState::Streaming {
                        node.activity = ActivityState::Streaming;
                    }
                    node.last_activity_time = now;
                }
            }
            AgentActivityMessage::Completed { block_id, success, .. } => {
                if let Some(node) = constellation.node_by_block_id_mut(block_id) {
                    node.activity = if *success {
                        ActivityState::Completed
                    } else {
                        ActivityState::Error
                    };
                    node.last_activity_time = now;
                }
            }
            AgentActivityMessage::CursorMoved { block_id, .. } => {
                if let Some(node) = constellation.node_by_block_id_mut(block_id) {
                    if node.activity == ActivityState::Idle {
                        node.activity = ActivityState::Active;
                    }
                    node.last_activity_time = now;
                }
            }
        }
    }
}

/// Handle clicks on constellation nodes to focus that context.
///
/// Click only focuses — Enter or double-click switches context.
/// Guarded by `FocusStack::is_modal()` — clicks are ignored when a dialog
/// is open over the constellation, preventing focus theft.
fn handle_node_click(
    mut constellation: ResMut<Constellation>,
    nodes: Query<(&Interaction, &ConstellationNode), Changed<Interaction>>,
    focus_stack: Res<crate::input::focus::FocusStack>,
) {
    if focus_stack.is_modal() {
        return;
    }

    for (interaction, node) in nodes.iter() {
        if *interaction == Interaction::Pressed {
            info!("Clicked constellation node: {}", node.context_id);
            if let Ok(ctx_id) = ContextId::parse(&node.context_id) {
                constellation.focus(ctx_id);
            }
        }
    }
}

// Input handling in input::systems (toggle_constellation + constellation_nav)

/// How much the ring curves upward as cards recede (px per unit).
/// Higher = more vertical spread. The ring appears as a tilted plane.
const CAROUSEL_TILT: f32 = 0.3;

/// Minimum spacing between adjacent cards on the ring circumference (px).
const CAROUSEL_MIN_SPACING: f32 = 240.0;

/// Update node positions using a tilted carousel ring.
///
/// The focused card sits at (0, 0) — viewport center. Other cards fan out
/// left/right and curve upward, like a ring on a tilted plane viewed from
/// slightly above. The ring radius scales with node count so cards never overlap.
/// h/l spins the carousel, bringing adjacent cards to front.
fn update_node_positions(
    mut constellation: ResMut<Constellation>,
    camera: Res<ConstellationCamera>,
) {
    let n = constellation.nodes.len();
    if n == 0 {
        return;
    }

    // Build DFS ring order: parent before children, sorted by context_id
    let ring_order = build_ring_order(&constellation);

    // Assign ring indices
    for (ring_idx, &node_idx) in ring_order.iter().enumerate() {
        constellation.nodes[node_idx].ring_index = ring_idx;
    }

    // Ring radius: ensure minimum angular spacing for card width
    let circumference = n as f32 * CAROUSEL_MIN_SPACING;
    let radius = (circumference / std::f32::consts::TAU).max(400.0);

    for &node_idx in &ring_order {
        let ring_idx = constellation.nodes[node_idx].ring_index;
        let base_angle = std::f32::consts::TAU * ring_idx as f32 / n as f32;
        let angle = base_angle + camera.carousel_angle;

        // Front card (angle≈0) at origin, others fan out and curve up.
        // x = R * sin(θ)  — horizontal spread
        // y = -R * (1 - cos(θ)) * TILT — curves upward (negative = up in screen coords)
        let x = radius * angle.sin();
        let y = -radius * (1.0 - angle.cos()) * CAROUSEL_TILT;

        // Depth: cos(angle) → 1.0 at front (angle=0), -1.0 at back (angle=π)
        let depth = angle.cos();

        constellation.nodes[node_idx].position = Vec2::new(x, y);
        constellation.nodes[node_idx].depth = depth;
    }
}

/// Build a DFS traversal order for the ring: parents before children,
/// siblings sorted by context_id. This keeps parent-child groups adjacent.
fn build_ring_order(constellation: &Constellation) -> Vec<usize> {
    let n = constellation.nodes.len();

    // Map context_id → index
    let id_to_idx: std::collections::HashMap<ContextId, usize> = constellation
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.context_id, i))
        .collect();

    // Build children lists
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut roots: Vec<usize> = Vec::new();

    for (i, node) in constellation.nodes.iter().enumerate() {
        if let Some(pid) = node.parent_id {
            if let Some(&parent_idx) = id_to_idx.get(&pid) {
                children[parent_idx].push(i);
            } else {
                roots.push(i);
            }
        } else {
            roots.push(i);
        }
    }

    if roots.is_empty() {
        roots = (0..n).collect();
    }

    // Stable sort by context_id (ContextId is Ord via UUIDv7)
    let nodes = &constellation.nodes;
    for ch in &mut children {
        ch.sort_by(|a, b| nodes[*a].context_id.cmp(&nodes[*b].context_id));
    }
    roots.sort_by(|a, b| nodes[*a].context_id.cmp(&nodes[*b].context_id));

    // DFS
    let mut order = Vec::with_capacity(n);
    let mut stack: Vec<usize> = roots.into_iter().rev().collect();
    while let Some(idx) = stack.pop() {
        order.push(idx);
        for &child in children[idx].iter().rev() {
            stack.push(child);
        }
    }

    // Append any nodes missed due to cycles in parent_id
    if order.len() < n {
        let in_order: std::collections::HashSet<usize> = order.iter().copied().collect();
        for i in 0..n {
            if !in_order.contains(&i) {
                order.push(i);
            }
        }
    }

    order
}

/// Get the ring index of the focused node (for carousel navigation).
pub fn focused_ring_index(constellation: &Constellation) -> Option<usize> {
    let focus_id = constellation.focus_id?;
    constellation.nodes.iter().find(|n| n.context_id == focus_id).map(|n| n.ring_index)
}

/// Get context_id of the node at a given ring index.
pub fn context_id_at_ring_index(constellation: &Constellation, ring_idx: usize) -> Option<ContextId> {
    constellation.nodes.iter()
        .find(|n| n.ring_index == ring_idx)
        .map(|n| n.context_id)
}


/// Smoothly interpolate camera offset, zoom, and carousel rotation toward targets.
fn interpolate_camera(
    mut camera: ResMut<ConstellationCamera>,
    screen: Res<State<crate::ui::screen::Screen>>,
    time: Res<Time>,
) {
    if !matches!(screen.get(), crate::ui::screen::Screen::Constellation) {
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

    // Carousel rotation (shortest-path angular interpolation)
    let mut angle_diff = camera.target_carousel_angle - camera.carousel_angle;
    // Normalize to [-π, π] for shortest rotation
    while angle_diff > std::f32::consts::PI {
        angle_diff -= std::f32::consts::TAU;
    }
    while angle_diff < -std::f32::consts::PI {
        angle_diff += std::f32::consts::TAU;
    }
    if angle_diff.abs() > 0.001 {
        camera.carousel_angle += angle_diff * t;
    } else {
        camera.carousel_angle = camera.target_carousel_angle;
    }
}

/// Find the nearest constellation node in a given direction from the focused node.
///
/// Filters nodes to the correct half-plane (dot product with direction > 0),
/// then scores by `distance / cos_angle` to prefer closer, more on-axis nodes.
pub fn find_nearest_in_direction(constellation: &Constellation, direction: Vec2) -> Option<ContextId> {
    let focus_id = constellation.focus_id?;
    let focus_pos = constellation.node_by_id(focus_id)?.position;

    let mut best: Option<(f32, ContextId)> = None;

    for node in &constellation.nodes {
        if node.context_id == focus_id {
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
            best = Some((score, node.context_id));
        }
    }

    best.map(|(_, id)| id)
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

