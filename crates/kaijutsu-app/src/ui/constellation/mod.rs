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
//! - `n` — Create new context
//! - `m` — Model picker for focused context
//! - `a` — Archive focused context

mod create_dialog;
mod detail;
mod legend;
pub mod model_picker;
mod render2d;

use std::collections::{HashMap, HashSet, VecDeque};

use avian2d::prelude::*;
use bevy::prelude::*;
use kaijutsu_client::ContextMembership;
use kaijutsu_types::ContextId;

use crate::agents::AgentActivityMessage;

pub use create_dialog::create_or_fork_context;

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
        // Avian2D physics (pixel-scale: 100px ≈ 1m for threshold tuning)
        app.add_plugins(PhysicsPlugins::default().with_length_unit(100.0));
        app.insert_resource(Gravity::ZERO);

        app.init_resource::<Constellation>()
            .init_resource::<ConstellationCamera>()
            .init_resource::<PhysicsEntityMap>()
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
                    sync_physics_entities,
                    wake_on_topology_change,
                    update_constellation_graph,
                    apply_radial_gravity,
                    update_center_exclusion_radius,
                    sync_physics_to_constellation,
                    interpolate_camera,
                )
                    .chain(),
            );

        // Pause physics when not on constellation screen
        app.add_systems(
            OnEnter(crate::ui::screen::Screen::Constellation),
            unpause_constellation_physics,
        );
        app.add_systems(
            OnExit(crate::ui::screen::Screen::Constellation),
            pause_constellation_physics,
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
// PHYSICS LAYOUT (Avian2D)
// ============================================================================

/// Marker on physics entities representing constellation nodes.
#[derive(Component)]
struct PhysicsNode {
    context_id: ContextId,
}

/// Maps ContextId → physics Entity for lifecycle management.
#[derive(Resource, Default)]
struct PhysicsEntityMap {
    /// Node entities keyed by ContextId
    map: HashMap<ContextId, Entity>,
    /// Center exclusion zone (static body)
    center_body: Option<Entity>,
    /// Joint entities keyed by child ContextId
    joints: HashMap<ContextId, Entity>,
}

/// Collision layers for constellation physics.
#[derive(PhysicsLayer, Clone, Copy, Debug, Default)]
enum ConstellationLayer {
    #[default]
    Node,
    CenterZone,
}

// ── Physics constants ──

/// Radial gravity strength for all nodes toward origin.
const RADIAL_GRAVITY_K: f32 = 0.3;
/// Additional center pull for the focused node.
const FOCUS_PULL_K: f32 = 1.5;
/// Node collider radius in simulation-space pixels.
const NODE_COLLIDER_RADIUS: f32 = 45.0;
/// DistanceJoint rest length range.
const JOINT_MIN_DIST: f32 = 200.0;
const JOINT_MAX_DIST: f32 = 300.0;
/// Joint compliance (inverse stiffness: smaller = stiffer).
const JOINT_COMPLIANCE: f32 = 0.001;
/// Linear damping coefficient.
const NODE_DAMPING: f32 = 3.0;
/// Center exclusion zone radius as fraction of min(viewport_w, viewport_h).
const CENTER_EXCLUSION_FRACTION: f32 = 0.15;

// ── Pause/unpause ──

fn pause_constellation_physics(mut physics_time: ResMut<Time<Physics>>) {
    physics_time.pause();
}

fn unpause_constellation_physics(mut physics_time: ResMut<Time<Physics>>) {
    physics_time.unpause();
}

// ── Physics entity sync ──

/// Sync physics entities with constellation topology.
///
/// Spawns/despawns Avian2D rigid bodies to mirror constellation nodes.
/// Creates DistanceJoint entities for fork edges.
/// Spawns the center exclusion zone body once.
fn sync_physics_entities(
    mut commands: Commands,
    constellation: Res<Constellation>,
    mut entity_map: ResMut<PhysicsEntityMap>,
) {
    let n = constellation.nodes.len();

    // Build set of current context IDs
    let current_ids: HashSet<ContextId> =
        constellation.nodes.iter().map(|n| n.context_id).collect();

    // Despawn entities for removed nodes
    let removed: Vec<ContextId> = entity_map
        .map
        .keys()
        .filter(|id| !current_ids.contains(id))
        .copied()
        .collect();
    for id in &removed {
        if let Some(entity) = entity_map.map.remove(id) {
            commands.entity(entity).despawn();
        }
        if let Some(joint_entity) = entity_map.joints.remove(id) {
            commands.entity(joint_entity).despawn();
        }
    }

    // Spawn entities for new nodes
    let scatter_radius = 150.0 + (n as f32).sqrt() * 50.0;
    for (i, node) in constellation.nodes.iter().enumerate() {
        if entity_map.map.contains_key(&node.context_id) {
            continue;
        }

        // Initial position: use existing position, or ring scatter for new nodes
        let pos = if node.position != Vec2::ZERO {
            node.position
        } else if Some(node.context_id) == constellation.focus_id {
            Vec2::ZERO
        } else {
            let angle = std::f32::consts::TAU * i as f32 / n.max(1) as f32;
            Vec2::new(angle.cos(), angle.sin()) * scatter_radius
        };

        let entity = commands
            .spawn((
                PhysicsNode {
                    context_id: node.context_id,
                },
                RigidBody::Dynamic,
                Collider::circle(NODE_COLLIDER_RADIUS),
                Transform::from_xyz(pos.x, pos.y, 0.0),
                LinearDamping(NODE_DAMPING),
                LockedAxes::ROTATION_LOCKED,
                ConstantForce(Vec2::ZERO), // Updated by apply_radial_gravity
                CollisionLayers::new(
                    [ConstellationLayer::Node],
                    [ConstellationLayer::Node, ConstellationLayer::CenterZone],
                ),
            ))
            .id();

        entity_map.map.insert(node.context_id, entity);
    }

    // Sync joints for fork edges
    let desired_edges: HashMap<ContextId, ContextId> = constellation
        .nodes
        .iter()
        .filter_map(|node| {
            let parent_id = node.forked_from?;
            if entity_map.map.contains_key(&node.context_id)
                && entity_map.map.contains_key(&parent_id)
            {
                Some((node.context_id, parent_id))
            } else {
                None
            }
        })
        .collect();

    // Remove stale joints
    let stale_joints: Vec<ContextId> = entity_map
        .joints
        .keys()
        .filter(|child_id| !desired_edges.contains_key(child_id))
        .copied()
        .collect();
    for child_id in stale_joints {
        if let Some(joint_entity) = entity_map.joints.remove(&child_id) {
            commands.entity(joint_entity).despawn();
        }
    }

    // Create joints for new edges
    for (child_id, parent_id) in &desired_edges {
        if entity_map.joints.contains_key(child_id) {
            continue;
        }
        let Some(&child_entity) = entity_map.map.get(child_id) else {
            continue;
        };
        let Some(&parent_entity) = entity_map.map.get(parent_id) else {
            continue;
        };

        let joint_entity = commands
            .spawn(
                DistanceJoint::new(parent_entity, child_entity)
                    .with_limits(JOINT_MIN_DIST, JOINT_MAX_DIST)
                    .with_compliance(JOINT_COMPLIANCE),
            )
            .id();

        entity_map.joints.insert(*child_id, joint_entity);
    }

    // Spawn center exclusion body once
    if entity_map.center_body.is_none() {
        let center = commands
            .spawn((
                RigidBody::Static,
                Collider::circle(100.0), // Updated by update_center_exclusion_radius
                Transform::from_xyz(0.0, 0.0, 0.0),
                CollisionLayers::new(
                    [ConstellationLayer::CenterZone],
                    [ConstellationLayer::Node],
                ),
            ))
            .id();
        entity_map.center_body = Some(center);
    }
}

/// Wake all sleeping physics bodies when topology or focus changes.
fn wake_on_topology_change(
    mut commands: Commands,
    constellation: Res<Constellation>,
    entity_map: Res<PhysicsEntityMap>,
    mut last_entity_count: Local<usize>,
    mut last_joint_count: Local<usize>,
    mut last_focus: Local<Option<ContextId>>,
    sleeping_bodies: Query<Entity, (With<PhysicsNode>, With<Sleeping>)>,
) {
    let ec = entity_map.map.len();
    let jc = entity_map.joints.len();
    let focus = constellation.focus_id;

    if ec != *last_entity_count || jc != *last_joint_count || focus != *last_focus {
        for entity in &sleeping_bodies {
            commands.entity(entity).remove::<Sleeping>();
        }
        *last_entity_count = ec;
        *last_joint_count = jc;
        *last_focus = focus;
    }
}

/// Graph metadata update — ring order, BFS distances, depth, camera follow.
///
/// This is the non-physics remainder of the old `run_force_simulation`.
/// Force computation is now handled by Avian2D.
fn update_constellation_graph(
    mut constellation: ResMut<Constellation>,
    mut camera: ResMut<ConstellationCamera>,
    mut cached_ring: Local<Vec<usize>>,
    mut prev_focus: Local<Option<ContextId>>,
    mut prev_node_count: Local<usize>,
) {
    let n = constellation.nodes.len();
    if n == 0 {
        return;
    }

    let focus_changed = *prev_focus != constellation.focus_id;
    let topology_changed = n != *prev_node_count;

    if focus_changed || topology_changed {
        // Rebuild ring order (tree-ordered cycling for keyboard nav)
        if topology_changed || cached_ring.len() != n {
            *cached_ring = build_ring_order(&constellation);
            for (ring_idx, &node_idx) in cached_ring.iter().enumerate() {
                constellation.nodes[node_idx].ring_index = ring_idx;
            }
        }

        // Recompute BFS graph distances
        if let Some(focus_id) = constellation.focus_id {
            let distances = compute_graph_distances(&constellation, focus_id);
            for (i, &dist) in distances.iter().enumerate() {
                constellation.nodes[i].graph_distance = dist;
            }
        }

        // Update depth from graph distances (1.0 = focused, 0.0 = farthest)
        let max_dist = constellation
            .nodes
            .iter()
            .filter(|n| n.graph_distance != u32::MAX)
            .map(|n| n.graph_distance)
            .max()
            .unwrap_or(1)
            .max(1);

        for node in &mut constellation.nodes {
            node.depth = if node.graph_distance == u32::MAX {
                0.0
            } else {
                1.0 - (node.graph_distance as f32 / max_dist as f32).min(1.0)
            };
        }

        *prev_focus = constellation.focus_id;
        *prev_node_count = n;
    }

    // Camera tracks focused node continuously (follows physics motion)
    if let Some(focus_id) = constellation.focus_id {
        if let Some(node) = constellation.node_by_id(focus_id) {
            camera.target_offset = -node.position;
        }
    }
}

/// Apply radial gravity via ConstantForce — pulls all nodes toward origin.
/// Focused node gets stronger pull and passes through the center exclusion zone.
fn apply_radial_gravity(
    constellation: Res<Constellation>,
    mut forces: Query<(
        &PhysicsNode,
        &Transform,
        &mut ConstantForce,
        &mut CollisionLayers,
    )>,
) {
    for (phys_node, transform, mut force, mut layers) in &mut forces {
        let pos = transform.translation.truncate();
        let is_focused = constellation.focus_id == Some(phys_node.context_id);
        let k = if is_focused {
            RADIAL_GRAVITY_K + FOCUS_PULL_K
        } else {
            RADIAL_GRAVITY_K
        };
        force.0 = -pos * k;

        // Focused node ignores center exclusion zone so it can reach origin
        let desired = if is_focused {
            CollisionLayers::new(
                [ConstellationLayer::Node],
                [ConstellationLayer::Node],
            )
        } else {
            CollisionLayers::new(
                [ConstellationLayer::Node],
                [ConstellationLayer::Node, ConstellationLayer::CenterZone],
            )
        };
        if *layers != desired {
            *layers = desired;
        }
    }
}

/// Update center exclusion zone radius based on viewport size.
fn update_center_exclusion_radius(
    mut commands: Commands,
    entity_map: Res<PhysicsEntityMap>,
    container_q: Query<&ComputedNode, With<ConstellationContainer>>,
) {
    let Some(center_entity) = entity_map.center_body else {
        return;
    };
    let Ok(computed) = container_q.single() else {
        return;
    };
    let size = computed.size();
    if size.x < 1.0 || size.y < 1.0 {
        return;
    }
    let radius = size.x.min(size.y) * CENTER_EXCLUSION_FRACTION;
    commands
        .entity(center_entity)
        .insert(Collider::circle(radius));
}

/// Sync physics Transform → ContextNode.position each frame.
fn sync_physics_to_constellation(
    mut constellation: ResMut<Constellation>,
    physics_nodes: Query<(&PhysicsNode, &Transform)>,
) {
    for (phys_node, transform) in &physics_nodes {
        if let Some(node) = constellation
            .nodes
            .iter_mut()
            .find(|n| n.context_id == phys_node.context_id)
        {
            node.position = transform.translation.truncate();
        }
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
