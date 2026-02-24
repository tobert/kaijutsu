//! H3-inspired hyperbolic cone tree layout engine.
//!
//! Positions constellation nodes in hyperbolic 3-space using the algorithm from
//! Munzner's H3 (Stanford 1997), adapted for our fork graph. The layout is
//! computed in two passes:
//!
//! 1. **Bottom-up**: Calculate hemisphere radius for each subtree (how much space
//!    it needs). Leaves get a base radius; internal nodes sum child disc areas.
//!
//! 2. **Top-down**: Place children on the parent's hemisphere surface in concentric
//!    bands, largest subtrees closest to the pole (most visible position).
//!
//! Layout positions are in the hyperboloid model (f64). Rendering projects them
//! to the Poincaré ball via `HyperPoint::to_ball_f32()`.
//!
//! ## Structural stability
//!
//! Adding a fork only recomputes ancestor hemisphere radii (bottom-up) and replaces
//! children of affected ancestors (top-down). Unrelated subtrees don't move.

use bevy::math::DVec3;
use std::collections::HashMap;

use super::hyper::{
    HyperPoint, LorentzTransform,
    hyper_disc_area, hyper_radius_from_area,
};

// ============================================================================
// Types
// ============================================================================

/// Per-node cached layout data.
#[derive(Clone, Debug)]
pub struct NodeLayout {
    /// Position in hyperbolic space (stable across focus changes).
    pub hyper_pos: HyperPoint,
    /// Hemisphere radius from bottom-up sizing.
    pub hemisphere_radius: f64,
}

impl Default for NodeLayout {
    fn default() -> Self {
        Self {
            hyper_pos: HyperPoint::ORIGIN,
            hemisphere_radius: 0.0,
        }
    }
}

/// The H3 layout engine. Stores layout state parallel to `Constellation.nodes`.
#[derive(Clone, Debug)]
pub struct H3Layout {
    /// Per-node layout data, indexed by position in the node list.
    pub nodes: Vec<NodeLayout>,
    /// Base hemisphere radius for leaf nodes.
    pub base_leaf_radius: f64,
    /// Packing factor — multiplier for gap compensation in hemisphere area sums.
    pub packing_factor: f64,
    /// Layout generation — incremented on recompute.
    pub generation: u64,
}

impl Default for H3Layout {
    fn default() -> Self {
        Self {
            nodes: Vec::new(),
            base_leaf_radius: 0.3,
            packing_factor: 1.4,
            generation: 0,
        }
    }
}

/// Maximum tree depth to prevent stack overflow from cyclic parent data.
const MAX_DEPTH: usize = 64;

// ============================================================================
// Tree topology helpers
// ============================================================================

/// Build adjacency from a list of (context_id, parent_id) pairs.
/// Returns (children: Vec<Vec<usize>>, roots: Vec<usize>, id_to_idx: HashMap).
fn build_adjacency<'a>(
    ids: &'a [String],
    parents: &[Option<String>],
) -> (Vec<Vec<usize>>, Vec<usize>, HashMap<&'a str, usize>) {
    let n = ids.len();

    let id_to_idx: HashMap<&str, usize> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| (id.as_str(), i))
        .collect();

    let mut children = vec![Vec::new(); n];
    let mut roots = Vec::new();

    for (i, parent) in parents.iter().enumerate() {
        if let Some(pid) = parent {
            if let Some(&parent_idx) = id_to_idx.get(pid.as_str()) {
                children[parent_idx].push(i);
            } else {
                roots.push(i);
            }
        } else {
            roots.push(i);
        }
    }

    if roots.is_empty() && n > 0 {
        roots = (0..n).collect();
    }

    // Stable sort: children by context_id for deterministic layout
    for ch in &mut children {
        ch.sort_by(|a, b| ids[*a].cmp(&ids[*b]));
    }
    roots.sort_by(|a, b| ids[*a].cmp(&ids[*b]));

    (children, roots, id_to_idx)
}

/// Count descendants (including self), depth-limited.
fn count_descendants(idx: usize, children: &[Vec<usize>], depth: usize) -> usize {
    if depth >= MAX_DEPTH {
        return 1;
    }
    let mut count = 1;
    for &child in &children[idx] {
        count += count_descendants(child, children, depth + 1);
    }
    count
}

// ============================================================================
// H3Layout implementation
// ============================================================================

impl H3Layout {
    /// Full recompute: bottom-up hemisphere sizing, then top-down placement.
    pub fn full_layout(
        &mut self,
        ids: &[String],
        parents: &[Option<String>],
    ) {
        let n = ids.len();
        if n == 0 {
            self.nodes.clear();
            self.generation += 1;
            return;
        }

        self.nodes.resize_with(n, NodeLayout::default);

        let (children, roots, _id_to_idx) = build_adjacency(ids, parents);

        // Bottom-up: compute hemisphere radii
        self.compute_hemisphere_radii(&children, &roots);

        // Top-down: place nodes in hyperbolic space
        self.place_all(&children, &roots);

        self.generation += 1;
    }

    /// Bottom-up pass: compute hemisphere radius for each node.
    ///
    /// Post-order traversal: leaf = base_leaf_radius, internal = area sum.
    fn compute_hemisphere_radii(
        &mut self,
        children: &[Vec<usize>],
        roots: &[usize],
    ) {
        for &root in roots {
            self.compute_radius_recursive(root, children, 0);
        }
    }

    fn compute_radius_recursive(
        &mut self,
        idx: usize,
        children: &[Vec<usize>],
        depth: usize,
    ) -> f64 {
        if depth >= MAX_DEPTH {
            let r = self.base_leaf_radius;
            self.nodes[idx].hemisphere_radius = r;
            return r;
        }

        let child_indices = &children[idx];

        if child_indices.is_empty() {
            let r = self.base_leaf_radius;
            self.nodes[idx].hemisphere_radius = r;
            return r;
        }

        // Recurse into children first (post-order)
        let mut area_sum = 0.0;
        for &child in child_indices {
            let child_r = self.compute_radius_recursive(child, children, depth + 1);
            area_sum += hyper_disc_area(child_r);
        }

        // Parent radius accommodates all children with packing factor
        let r = hyper_radius_from_area(area_sum * self.packing_factor);
        self.nodes[idx].hemisphere_radius = r;
        r
    }

    /// Top-down pass: place all nodes in hyperbolic space.
    fn place_all(
        &mut self,
        children: &[Vec<usize>],
        roots: &[usize],
    ) {
        if roots.len() == 1 {
            // Single root at origin
            self.nodes[roots[0]].hyper_pos = HyperPoint::ORIGIN;
            self.place_children(roots[0], children, &HyperPoint::ORIGIN, 0);
        } else {
            // Multiple roots: distribute around origin at angular positions
            // proportional to subtree size
            let total_desc: usize = roots
                .iter()
                .map(|&r| count_descendants(r, children, 0))
                .sum();

            let mut angle = 0.0_f64;

            for &root_idx in roots {
                let desc = count_descendants(root_idx, children, 0);
                let sector = std::f64::consts::TAU * (desc as f64 / total_desc.max(1) as f64);
                let mid_angle = angle + sector / 2.0;

                // Place root at a small offset from origin
                let root_dist = if roots.len() > 1 {
                    self.nodes[root_idx].hemisphere_radius * 0.3
                } else {
                    0.0
                };

                let direction = DVec3::new(mid_angle.cos(), mid_angle.sin(), 0.0);
                let root_pos = HyperPoint::from_direction_and_distance(direction, root_dist);
                self.nodes[root_idx].hyper_pos = root_pos;

                self.place_children(root_idx, children, &root_pos, 0);
                angle += sector;
            }
        }
    }

    /// Place children of a node on its hemisphere surface.
    ///
    /// Children are sorted by subtree size (largest first → closest to pole,
    /// the most visible position). Concentric bands fill the hemisphere from
    /// pole outward.
    fn place_children(
        &mut self,
        parent_idx: usize,
        children: &[Vec<usize>],
        parent_pos: &HyperPoint,
        depth: usize,
    ) {
        if depth >= MAX_DEPTH {
            return;
        }

        let child_indices = &children[parent_idx];
        if child_indices.is_empty() {
            return;
        }

        let parent_r = self.nodes[parent_idx].hemisphere_radius;

        // Sort children by subtree size (largest first)
        let mut sorted_children: Vec<(usize, f64)> = child_indices
            .iter()
            .map(|&c| (c, self.nodes[c].hemisphere_radius))
            .collect();
        sorted_children.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // Place children in concentric bands on the hemisphere
        // We use a simplified band-filling approach:
        // Each child is placed at a polar angle that ensures its hemisphere
        // fits within its angular allocation.

        let n = sorted_children.len();

        if n == 1 {
            // Single child: place along the "forward" direction at parent_r distance
            let (child_idx, _) = sorted_children[0];
            let child_pos = self.place_child_at_angle(
                parent_pos,
                parent_r,
                0.0, // theta = 0 → pole direction
                0.0, // phi = 0
                depth,
            );
            self.nodes[child_idx].hyper_pos = child_pos;
            self.place_children(child_idx, children, &child_pos, depth + 1);
            return;
        }

        // Multiple children: distribute around the hemisphere
        // Use a golden-angle spiral for even distribution that degrades gracefully
        let golden_angle = std::f64::consts::PI * (3.0 - 5.0_f64.sqrt()); // ~2.399 rad

        for (i, &(child_idx, _child_r)) in sorted_children.iter().enumerate() {
            // Polar angle: children placed further from pole as index increases
            // Scale by subtree size ratio so larger children get tighter (more central) placement
            let t = (i as f64 + 0.5) / n as f64; // 0..1

            // Map t to polar angle: closer to pole for earlier (larger) children
            // Use a concave function so most area goes to first few children
            let theta = (std::f64::consts::PI / 2.0) * t.sqrt();

            // Azimuthal angle: golden spiral for even angular distribution
            let phi = golden_angle * i as f64;

            // Distance from parent: proportional to parent hemisphere radius
            // Larger subtrees are closer to parent (more central in Poincaré projection)
            let distance = parent_r * (0.8 + 0.4 * t);

            // Direction in parent's local frame
            let dx = theta.sin() * phi.cos();
            let dy = theta.sin() * phi.sin();
            let dz = theta.cos();

            let direction = DVec3::new(dx, dy, dz);

            // Place child in global frame relative to parent
            let child_pos = if parent_pos.distance(&HyperPoint::ORIGIN) < 1e-10 {
                // Parent is at origin — place directly
                HyperPoint::from_direction_and_distance(direction, distance)
            } else {
                // Parent is not at origin — compute in parent's local frame
                // then transform to global frame
                let local_child = HyperPoint::from_direction_and_distance(direction, distance);

                // Use the inverse of boost_to_origin(parent) to transport from
                // origin frame to parent's frame
                let boost_to_parent = LorentzTransform::boost_to_origin(parent_pos).inverse();
                boost_to_parent.apply(&local_child)
            };

            self.nodes[child_idx].hyper_pos = child_pos;

            // Recurse
            self.place_children(child_idx, children, &child_pos, depth + 1);
        }
    }

    /// Helper: place a child at specific spherical angles from parent.
    fn place_child_at_angle(
        &self,
        parent_pos: &HyperPoint,
        parent_r: f64,
        theta: f64,
        phi: f64,
        _depth: usize,
    ) -> HyperPoint {
        let dx = theta.sin() * phi.cos();
        let dy = theta.sin() * phi.sin();
        let dz = theta.cos();

        let direction = if dx.abs() < 1e-14 && dy.abs() < 1e-14 && dz.abs() < 1e-14 {
            DVec3::Z // Default forward direction
        } else {
            DVec3::new(dx, dy, dz)
        };

        let distance = parent_r;

        if parent_pos.distance(&HyperPoint::ORIGIN) < 1e-10 {
            HyperPoint::from_direction_and_distance(direction, distance)
        } else {
            let local_child = HyperPoint::from_direction_and_distance(direction, distance);
            let boost_to_parent = LorentzTransform::boost_to_origin(parent_pos).inverse();
            boost_to_parent.apply(&local_child)
        }
    }

    /// Project all nodes to Poincaré ball positions using a focus transform.
    ///
    /// Returns `Vec<bevy::math::Vec3>` — the f32 ball positions for rendering.
    pub fn project_all(&self, focus_transform: &LorentzTransform) -> Vec<bevy::math::Vec3> {
        self.nodes
            .iter()
            .map(|node| {
                let transformed = focus_transform.apply(&node.hyper_pos);
                transformed.to_ball_f32()
            })
            .collect()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::hyper::HyperPoint;

    #[test]
    fn single_root_at_origin() {
        let mut layout = H3Layout::default();
        let ids = vec!["root".to_string()];
        let parents = vec![None];

        layout.full_layout(&ids, &parents);

        assert_eq!(layout.nodes.len(), 1);
        assert!(layout.nodes[0].hyper_pos.distance(&HyperPoint::ORIGIN) < 1e-10);
    }

    #[test]
    fn binary_tree_depth_5_unique_positions() {
        // Build a binary tree of depth 5 (31 nodes)
        let mut ids = Vec::new();
        let mut parents = Vec::new();

        ids.push("root".to_string());
        parents.push(None);

        fn add_children(
            parent: &str,
            depth: usize,
            max_depth: usize,
            ids: &mut Vec<String>,
            parents: &mut Vec<Option<String>>,
        ) {
            if depth >= max_depth {
                return;
            }
            let left = format!("{parent}_L");
            let right = format!("{parent}_R");
            ids.push(left.clone());
            parents.push(Some(parent.to_string()));
            ids.push(right.clone());
            parents.push(Some(parent.to_string()));
            add_children(&left, depth + 1, max_depth, ids, parents);
            add_children(&right, depth + 1, max_depth, ids, parents);
        }

        add_children("root", 0, 4, &mut ids, &mut parents);
        assert_eq!(ids.len(), 31);

        let mut layout = H3Layout::default();
        layout.full_layout(&ids, &parents);

        assert_eq!(layout.nodes.len(), 31);

        // All positions should be unique
        for i in 0..layout.nodes.len() {
            for j in (i + 1)..layout.nodes.len() {
                let d = layout.nodes[i]
                    .hyper_pos
                    .distance(&layout.nodes[j].hyper_pos);
                assert!(
                    d > 1e-6,
                    "nodes {i} and {j} overlap: distance = {d}"
                );
            }
        }
    }

    #[test]
    fn deep_tree_no_overflow() {
        // Depth 20: linear chain
        let n = 20;
        let ids: Vec<String> = (0..n).map(|i| format!("node_{i}")).collect();
        let mut parents: Vec<Option<String>> = vec![None];
        for i in 1..n {
            parents.push(Some(format!("node_{}", i - 1)));
        }

        let mut layout = H3Layout::default();
        layout.full_layout(&ids, &parents);

        assert_eq!(layout.nodes.len(), n);
        // All should have valid (finite) positions
        for (i, node) in layout.nodes.iter().enumerate() {
            assert!(
                node.hyper_pos.t.is_finite(),
                "node {i} has non-finite position"
            );
        }
    }

    #[test]
    fn multi_root_spacing() {
        let ids = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let parents = vec![None, None, None];

        let mut layout = H3Layout::default();
        layout.full_layout(&ids, &parents);

        // All roots should be at non-zero distance from each other
        for i in 0..3 {
            for j in (i + 1)..3 {
                let d = layout.nodes[i]
                    .hyper_pos
                    .distance(&layout.nodes[j].hyper_pos);
                assert!(d > 0.01, "roots {i} and {j} too close: distance = {d}");
            }
        }
    }

    #[test]
    fn deterministic_layout() {
        let ids = vec![
            "root".to_string(),
            "child_a".to_string(),
            "child_b".to_string(),
            "grandchild".to_string(),
        ];
        let parents = vec![
            None,
            Some("root".to_string()),
            Some("root".to_string()),
            Some("child_a".to_string()),
        ];

        let mut layout1 = H3Layout::default();
        layout1.full_layout(&ids, &parents);

        let mut layout2 = H3Layout::default();
        layout2.full_layout(&ids, &parents);

        for i in 0..ids.len() {
            let d = layout1.nodes[i]
                .hyper_pos
                .distance(&layout2.nodes[i].hyper_pos);
            assert!(
                d < 1e-10,
                "layout not deterministic at node {i}: distance = {d}"
            );
        }
    }

    #[test]
    fn project_all_in_unit_ball() {
        let ids = vec![
            "root".to_string(),
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
        ];
        let parents = vec![
            None,
            Some("root".to_string()),
            Some("root".to_string()),
            Some("a".to_string()),
        ];

        let mut layout = H3Layout::default();
        layout.full_layout(&ids, &parents);

        let ball_positions = layout.project_all(&LorentzTransform::IDENTITY);

        for (i, pos) in ball_positions.iter().enumerate() {
            let r = pos.length();
            assert!(
                r < 1.0,
                "node {i} projects outside unit ball: r = {r}, pos = {pos:?}"
            );
        }
    }

    #[test]
    fn hemisphere_radius_monotonic() {
        // Internal nodes should have >= radius of their largest child
        let ids = vec![
            "root".to_string(),
            "child".to_string(),
            "grandchild".to_string(),
        ];
        let parents = vec![
            None,
            Some("root".to_string()),
            Some("child".to_string()),
        ];

        let mut layout = H3Layout::default();
        layout.full_layout(&ids, &parents);

        // root >= child >= grandchild (leaf)
        assert!(layout.nodes[0].hemisphere_radius >= layout.nodes[1].hemisphere_radius);
        assert!(layout.nodes[1].hemisphere_radius >= layout.nodes[2].hemisphere_radius);
    }

    #[test]
    fn empty_layout() {
        let mut layout = H3Layout::default();
        layout.full_layout(&[], &[]);
        assert!(layout.nodes.is_empty());
        assert_eq!(layout.generation, 1);
    }
}
