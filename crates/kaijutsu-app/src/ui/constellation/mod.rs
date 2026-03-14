//! Constellation — force-directed graph context navigator.
//!
//! Contexts are arranged as a force-directed graph. The focused card is pulled
//! to center; other cards are positioned by spring and repulsion forces. Edges
//! connect parent→child forks. A detail sidebar shows focused node info.
//!
//! ## Architecture
//!
//! - `mod.rs` — Data model (Constellation, ContextNode), force-directed layout, camera interpolation
//! - `render2d.rs` — Vello card rendering (spawn/despawn, distance-based scale/opacity, edge curves)
//! - `legend.rs` — Info panel overlay (context count, staged drifts)
//! - `detail.rs` — Detail sidebar (focused node info)
//! - `create_dialog.rs` — "New context" dialog
//! - `fork_form.rs` — Full-viewport fork configuration form
//! - `model_picker.rs` — Model selection overlay
//!
//! ## Activation
//!
//! Backtick toggles between conversation view and full-screen constellation.
//! The constellation takes over the content area, hiding conversation panes.
//! Enter on a focused node switches context and returns to conversation.
//!
//! ## Navigation
//!
//! - `h/l` — Spatial navigation left/right
//! - `j/k` — Spatial navigation up/down
//! - `Shift+hjkl` — Pan camera
//! - `+/-` — Zoom in/out
//! - `0` — Reset camera to default view
//! - `Enter` — Switch to focused context
//! - `f` — Fork focused context (opens fork form)
//! - `n` — Create new context
//! - `m` — Model picker for focused context
//! - `a` — Archive focused context

mod create_dialog;
mod detail;
pub mod fork_form;
mod legend;
pub mod model_picker;
mod render2d;

use std::collections::{HashMap, VecDeque};

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
            .init_resource::<ForceGraph>()
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
                    run_force_simulation,
                    interpolate_camera,
                )
                    .chain(),
            );

        // Constellation container + legend panel
        legend::setup_legend_systems(app);

        // Detail sidebar (focused node info)
        detail::setup_detail_systems(app);

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
            forked_from: None, // Populated by sync_model_info_to_constellation
            label: None,     // Populated by sync_model_info_to_constellation
            position: Vec2::ZERO,
            depth: 0.0,
            ring_index: self.nodes.len(),
            graph_distance: u32::MAX,
            activity: ActivityState::default(),
            model: None,
            provider: None,
            joined: true,
            last_activity_time: 0.0,
            fork_kind: None,
            keywords: Vec::new(),
            top_block_preview: None,
        };

        self.nodes.push(node);

        // If no focus, set this as focus
        if self.focus_id.is_none() {
            self.focus_id = Some(context_id);
        }
    }

    /// Add a placeholder node from DriftState context info (not yet joined).
    ///
    /// Skips archived contexts — they are not shown in the constellation.
    pub fn add_node_from_context_info(&mut self, ctx_info: &kaijutsu_client::ContextInfo) {
        if ctx_info.archived {
            return;
        }

        let context_id = ctx_info.id;

        if self.node_by_id(context_id).is_some() {
            return;
        }

        let node = ContextNode {
            context_id,
            forked_from: ctx_info.forked_from,
            label: if ctx_info.label.is_empty() { None } else { Some(ctx_info.label.clone()) },
            position: Vec2::ZERO,
            depth: 0.0,
            ring_index: self.nodes.len(),
            graph_distance: u32::MAX,
            activity: ActivityState::Idle,
            model: if ctx_info.model.is_empty() { None } else { Some(ctx_info.model.clone()) },
            provider: if ctx_info.provider.is_empty() { None } else { Some(ctx_info.provider.clone()) },
            joined: false,
            last_activity_time: 0.0,
            fork_kind: ctx_info.fork_kind.clone(),
            keywords: ctx_info.keywords.clone(),
            top_block_preview: ctx_info.top_block_preview.clone(),
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

    /// Walk forked_from chain to find the root ancestor of a context.
    pub fn root_of(&self, id: ContextId) -> Option<ContextId> {
        let mut current = id;
        for _ in 0..self.nodes.len() {
            match self.nodes.iter().find(|n| n.context_id == current) {
                Some(node) => match node.forked_from {
                    Some(parent) => current = parent,
                    None => return Some(current),
                },
                None => return None,
            }
        }
        Some(current)
    }

}

/// A node in the constellation representing a context
#[derive(Clone)]
pub struct ContextNode {
    /// Unique context identifier
    pub context_id: ContextId,
    /// Fork source context ID (from drift router, for tree layout)
    pub forked_from: Option<ContextId>,
    /// Human-readable label (e.g. "default", "debug-auth")
    pub label: Option<String>,
    /// Position in constellation space (calculated by force simulation)
    pub position: Vec2,
    /// Depth value: 1.0 = focused, 0.0 = max distance (repurposed from carousel depth)
    pub depth: f32,
    /// Index in the ring order (for tree-ordered cycling)
    pub ring_index: usize,
    /// BFS hop count from focused node (0 = focused, u32::MAX = disconnected)
    pub graph_distance: u32,
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
    /// How this context was forked (e.g. "shallow", "compact", "subtree")
    pub fork_kind: Option<String>,
    /// Synthesis keywords (empty if not yet synthesized)
    pub keywords: Vec<String>,
    /// Preview of the most representative block
    pub top_block_preview: Option<String>,
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

// ============================================================================
// FORCE-DIRECTED LAYOUT
// ============================================================================

/// Force-directed graph simulation state.
#[derive(Resource)]
pub struct ForceGraph {
    /// Per-node velocities for the simulation
    velocities: Vec<Vec2>,
    /// Whether the simulation has settled (max velocity < threshold)
    pub settled: bool,
}

impl Default for ForceGraph {
    fn default() -> Self {
        Self {
            velocities: Vec::new(),
            settled: false,
        }
    }
}

/// Force simulation constants.
const REPULSION_STRENGTH: f32 = 80000.0;
const SPRING_K: f32 = 0.3;
const REST_LENGTH: f32 = 250.0;
const CENTER_K: f32 = 0.8;
/// Weak gravity toward center for ALL nodes — prevents disconnected nodes
/// (no fork edges) from flying away indefinitely under repulsion.
const GRAVITY: f32 = 0.05;
const DAMPING: f32 = 0.85;
const MAX_VELOCITY: f32 = 200.0;
const SETTLE_THRESHOLD: f32 = 0.5;
const ITERATIONS_PER_FRAME: u32 = 3;

/// Run the force-directed graph simulation.
///
/// Replaces the old carousel ring layout. Nodes repel each other (Coulomb),
/// parent→child edges attract (Hooke spring), and the focused node is pulled
/// toward center. Simulation runs until settled, then skips computation.
fn run_force_simulation(
    mut constellation: ResMut<Constellation>,
    mut camera: ResMut<ConstellationCamera>,
    mut force_graph: ResMut<ForceGraph>,
    mut cached_ring: Local<Vec<usize>>,
    mut last_focus: Local<Option<ContextId>>,
    mut last_node_count: Local<usize>,
) {
    let n = constellation.nodes.len();
    if n == 0 {
        return;
    }

    let focus_changed = *last_focus != constellation.focus_id;
    let topology_changed = n != *last_node_count;

    if focus_changed || topology_changed {
        // Rebuild ring order (still useful for tree-ordered cycling)
        if topology_changed || cached_ring.len() != n {
            *cached_ring = build_ring_order(&constellation);
            for (ring_idx, &node_idx) in cached_ring.iter().enumerate() {
                constellation.nodes[node_idx].ring_index = ring_idx;
            }
        }

        // Resize velocities for new nodes
        force_graph.velocities.resize(n, Vec2::ZERO);

        // Initialize positions for new nodes (avoid all-at-origin singularity).
        // Scatter radius scales with sqrt(n) so nodes aren't crammed together.
        let scatter_radius = 150.0 + (n as f32).sqrt() * 50.0;
        for i in 0..n {
            if constellation.nodes[i].position == Vec2::ZERO
                && Some(constellation.nodes[i].context_id) != constellation.focus_id
            {
                let angle = std::f32::consts::TAU * i as f32 / n as f32;
                constellation.nodes[i].position =
                    Vec2::new(angle.cos(), angle.sin()) * scatter_radius;
            }
        }

        // Recompute BFS graph distances
        if let Some(focus_id) = constellation.focus_id {
            let distances = compute_graph_distances(&constellation, focus_id);
            for (i, &dist) in distances.iter().enumerate() {
                constellation.nodes[i].graph_distance = dist;
            }
        }

        // Unsettle to trigger re-simulation
        force_graph.settled = false;
        *last_focus = constellation.focus_id;
        *last_node_count = n;

        // Follow-focus camera pan
        if focus_changed {
            if let Some(focus_id) = constellation.focus_id {
                if let Some(node) = constellation.node_by_id(focus_id) {
                    camera.target_offset = -node.position;
                }
            }
        }
    }

    if force_graph.settled {
        return;
    }

    // Build index maps for O(1) lookups
    let id_to_idx: HashMap<ContextId, usize> = constellation
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.context_id, i))
        .collect();

    // Collect edges (parent → child)
    let edges: Vec<(usize, usize)> = constellation
        .nodes
        .iter()
        .enumerate()
        .filter_map(|(i, node)| {
            node.forked_from
                .and_then(|pid| id_to_idx.get(&pid).map(|&pi| (pi, i)))
        })
        .collect();

    let focused_idx = constellation
        .focus_id
        .and_then(|fid| id_to_idx.get(&fid).copied());

    for _ in 0..ITERATIONS_PER_FRAME {
        let mut forces = vec![Vec2::ZERO; n];

        // Repulsion: all node pairs (Coulomb's law)
        for i in 0..n {
            for j in (i + 1)..n {
                let delta =
                    constellation.nodes[i].position - constellation.nodes[j].position;
                let dist_sq = delta.length_squared().max(100.0); // avoid singularity
                let force_mag = REPULSION_STRENGTH / dist_sq;
                let force = delta.normalize_or_zero() * force_mag;
                forces[i] += force;
                forces[j] -= force;
            }
        }

        // Spring attraction: parent→child edges only (Hooke's law)
        for &(parent, child) in &edges {
            let delta =
                constellation.nodes[child].position - constellation.nodes[parent].position;
            let dist = delta.length();
            let displacement = dist - REST_LENGTH;
            let force = delta.normalize_or_zero() * SPRING_K * displacement;
            forces[parent] += force;
            forces[child] -= force;
        }

        // Weak gravity toward center for all nodes (prevents disconnected nodes escaping)
        for i in 0..n {
            forces[i] += -GRAVITY * constellation.nodes[i].position;
        }

        // Stronger center pull on focused node
        if let Some(fi) = focused_idx {
            forces[fi] += -CENTER_K * constellation.nodes[fi].position;
        }

        // Apply forces with damping and velocity cap
        let mut max_v = 0.0f32;
        for i in 0..n {
            force_graph.velocities[i] =
                (force_graph.velocities[i] + forces[i]) * DAMPING;
            let v = force_graph.velocities[i].length();
            if v > MAX_VELOCITY {
                force_graph.velocities[i] *= MAX_VELOCITY / v;
            }
            max_v = max_v.max(force_graph.velocities[i].length());
            constellation.nodes[i].position += force_graph.velocities[i];
        }

        if max_v < SETTLE_THRESHOLD {
            force_graph.settled = true;
            break;
        }
    }

    // Update depth from graph distances (compatibility: 1.0 = focused, 0.0 = far)
    let max_dist = constellation
        .nodes
        .iter()
        .filter(|n| n.graph_distance != u32::MAX)
        .map(|n| n.graph_distance)
        .max()
        .unwrap_or(1)
        .max(1);

    for i in 0..n {
        let dist = constellation.nodes[i].graph_distance;
        constellation.nodes[i].depth = if dist == u32::MAX {
            0.0
        } else {
            1.0 - (dist as f32 / max_dist as f32).min(1.0)
        };
    }

    // Camera follows focused node during settling
    if let Some(fi) = focused_idx {
        camera.target_offset = -constellation.nodes[fi].position;
    }
}

/// BFS from focused node to compute hop counts for all reachable nodes.
fn compute_graph_distances(constellation: &Constellation, focus_id: ContextId) -> Vec<u32> {
    let n = constellation.nodes.len();
    let mut distances = vec![u32::MAX; n];

    let id_to_idx: HashMap<ContextId, usize> = constellation
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.context_id, i))
        .collect();

    // Build undirected adjacency list from fork edges
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, node) in constellation.nodes.iter().enumerate() {
        if let Some(parent_id) = node.forked_from {
            if let Some(&parent_idx) = id_to_idx.get(&parent_id) {
                adj[i].push(parent_idx);
                adj[parent_idx].push(i);
            }
        }
    }

    // BFS
    if let Some(&focus_idx) = id_to_idx.get(&focus_id) {
        distances[focus_idx] = 0;
        let mut queue = VecDeque::new();
        queue.push_back(focus_idx);
        while let Some(idx) = queue.pop_front() {
            for &neighbor in &adj[idx] {
                if distances[neighbor] == u32::MAX {
                    distances[neighbor] = distances[idx] + 1;
                    queue.push_back(neighbor);
                }
            }
        }
    }

    distances
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
        if let Some(pid) = node.forked_from {
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

/// Smoothly interpolate camera offset and zoom toward targets.
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
