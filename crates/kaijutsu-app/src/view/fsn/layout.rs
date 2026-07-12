//! Pure FSN-world math: VFS-path → world-space placement, the height-channel
//! mapping, wireframe/point mesh vertex builders, the LOD-tier decision, and
//! the camera-fly clamps. No Bevy types (mirrors `time_well::card` /
//! `room::bearing`'s stance) so every rule here is a plain-data unit test —
//! [`super::scene`] is the only place any of this touches an `Entity`.
//!
//! # Coordinate convention
//!
//! The FSN world lives on the room's XZ ground plane, +Y up — same
//! convention as `room::bearing`. [`kaijutsu_viz::fsn::Rect`]'s `(x0,y0,x1,y1)`
//! fields are reused for BOTH roles this module needs:
//! - **unit-face space** — [`kaijutsu_viz::fsn::CellId::quad_rect`]'s native
//!   `[0,1]²`, used only as an intermediate when resolving a path's address.
//! - **world space** — `y0`/`y1` read as world **Z** (never world Y/height);
//!   [`root_world_rect`] and [`map_unit_rect_to_world`] are the one seam that
//!   converts between the two. `layout_field` itself is oblivious to which
//!   space its `Rect` argument lives in — it is a pure function of whatever
//!   rect you hand it — so this module never double-maps: a directory's
//!   children are laid out ONCE, directly in world space.
//!
//! # Height channel (vfs.md open question 1)
//!
//! [`height_channel`] is the ONE first-candidate mapping slice 0 ships,
//! against the REAL tree (not a placeholder): files by `log2(size)`,
//! directories by (capped) child count, symlinks a flat ghost stub. Amy
//! decides against the real data later; this is deliberately a single pure
//! function, not a mapping-selection system (the task's own scope fence).
//!
//! # LOD tier (vfs.md claim 5, the render/enumeration ladder)
//!
//! A directory only ever gets a field of its own once its listing is known
//! (`sync::FsnState`) — an unenumerated directory has NO field to render at
//! all; it appears only as its own cell (a point/prism, whatever tier ITS
//! PARENT's field is drawing at) in its parent's already-known field. So
//! [`lod_tier`]'s `enumerated` argument is `true` at every call site slice 0
//! reaches today (only known fields get tiered) — kept as an explicit,
//! independently-tested input rather than baked into camera distance alone,
//! both because it is what the design doc actually specifies ("decided...
//! from (a) enumeration state and (b) camera distance") and because a field
//! that goes stale (a future re-fetch invalidates it) has a natural `false`
//! path to fall into without restructuring this function.

use kaijutsu_client::VfsFileType;
use kaijutsu_viz::fsn::{
    CellId, ChildSpec, FsnCell, FsnField, NodeKind, Rect, path_to_cell, placeholder_quadrant,
};

// ── World placement (Amy-tunable) ───────────────────────────────────────────

/// Side length (world units) of the root directory's square footprint on the
/// XZ plane, centered at the world origin. **Amy-tunable, first guess.**
pub const ROOT_WORLD_SIZE: f32 = 2000.0;

/// The root directory's world-space rect: a square of [`ROOT_WORLD_SIZE`],
/// centered at the origin (`y` fields read as world Z — see the module doc).
pub fn root_world_rect() -> Rect {
    let half = (ROOT_WORLD_SIZE as f64) * 0.5;
    Rect { x0: -half, y0: -half, x1: half, y1: half }
}

/// Map a unit-face `[0,1]²` rect (e.g. a [`CellId::quad_rect`]) into world
/// space through `root_world`'s own rect — the one seam between the two
/// roles [`Rect`] plays in this module (see the module doc).
pub fn map_unit_rect_to_world(root_world: Rect, unit: Rect) -> Rect {
    let w = root_world.width();
    let h = root_world.height();
    Rect {
        x0: root_world.x0 + unit.x0 * w,
        y0: root_world.y0 + unit.y0 * h,
        x1: root_world.x0 + unit.x1 * w,
        y1: root_world.y0 + unit.y1 * h,
    }
}

/// Split an absolute VFS path into its non-empty components: `"/"` → `[]`,
/// `"/foo/bar"` → `["foo", "bar"]`, and a trailing slash is tolerated
/// (`"/foo/bar/"` → the same as `"/foo/bar"`) since kernel-side paths are
/// usually clean but a defensive split costs nothing.
pub fn path_components(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
}

/// Join a directory's own absolute path with a child's name — `"/"` + `"foo"`
/// → `"/foo"` (no double slash), `"/foo"` + `"bar"` → `"/foo/bar"`.
pub fn join_path(base: &str, name: &str) -> String {
    if base == "/" { format!("/{name}") } else { format!("{base}/{name}") }
}

/// Resolve a VFS path's own world-space rect — where ITS field of children
/// gets laid out — by walking the quadtree address from the root face and
/// mapping the resulting unit `quad_rect` through [`root_world_rect`].
/// [`placeholder_quadrant`] is Lane A's documented Open-Question-2 stand-in
/// (`kaijutsu_viz::fsn`'s own doc) — good enough for slice 0's "which quarter
/// does this subdirectory occupy" placement, not yet the final slack policy.
///
/// Panics only if `components` somehow exceeds [`kaijutsu_viz::fsn::CELL_MAX_DEPTH`]
/// (28) — no real VFS tree gets remotely that deep; a caller that ever did
/// would need a real fallback policy, not a silently wrong rect.
pub fn path_world_rect(root_world: Rect, components: &[&str]) -> Rect {
    let root_cell = CellId::root(0).expect("face 0 is always in range (0..NUM_FACES)");
    let cell = path_to_cell(root_cell, components.iter().copied(), placeholder_quadrant)
        .expect("VFS paths stay far short of CELL_MAX_DEPTH (28)");
    map_unit_rect_to_world(root_world, cell.quad_rect())
}

// ── Height channel (vfs.md open question 1 — first candidate) ──────────────

/// Cap on a directory's height channel (raw child count) before world
/// scaling, so one enormous directory can't dwarf every sibling in its
/// parent's field. **Amy-tunable.**
pub const DIR_HEIGHT_CAP: f32 = 14.0;

/// World-Y units per unit of [`height_channel`] — the one knob between the
/// abstract channel value and a world-space prism height. **Amy-tunable.**
pub const HEIGHT_WORLD_SCALE: f32 = 26.0;

/// vfs.md open question 1's first-candidate height mapping, shipped against
/// the REAL tree at slice 0 (not a placeholder — Amy decides against real
/// data later, per the design doc's own instruction). Files: `1 +
/// log2(size_bytes.max(1))` (a 0-byte file still reads as a short stub, not
/// nothing — `max(1)` keeps `log2` finite). Directories: `1 + child_count`,
/// capped at [`DIR_HEIGHT_CAP`]. Symlinks: a flat `1.0` ghost stub
/// (`docs/scenes/vfs.md`: "ghost columns tethered to their targets by a
/// light thread" — slice 0 doesn't render the thread, just keeps the stub
/// short so it doesn't compete with real content).
pub fn height_channel(kind: NodeKind, size_bytes: u64, child_count: u32) -> f32 {
    match kind {
        NodeKind::File => 1.0 + (size_bytes.max(1) as f64).log2() as f32,
        NodeKind::Dir => (1.0 + child_count as f32).min(DIR_HEIGHT_CAP),
        NodeKind::Symlink => 1.0,
    }
}

/// [`height_channel`]'s abstract value, scaled into world-space prism height
/// by [`HEIGHT_WORLD_SCALE`].
pub fn world_height(channel: f32) -> f32 {
    channel.max(0.0) * HEIGHT_WORLD_SCALE
}

/// [`kaijutsu_client::VfsFileType`] → [`kaijutsu_viz::fsn::NodeKind`] — the
/// one conversion between the wire type and the layout crate's own type,
/// kept in one place rather than duplicated at every call site.
pub fn to_viz_kind(kind: VfsFileType) -> NodeKind {
    match kind {
        VfsFileType::File => NodeKind::File,
        VfsFileType::Directory => NodeKind::Dir,
        VfsFileType::Symlink => NodeKind::Symlink,
    }
}

/// Build one [`ChildSpec`] from a directory listing's raw fields — combines
/// [`to_viz_kind`], [`height_channel`], and [`world_height`] into the single
/// input [`kaijutsu_viz::fsn::layout_field`] wants.
pub fn child_spec(name: &str, kind: VfsFileType, size_bytes: u64, child_count: u32) -> ChildSpec {
    let viz_kind = to_viz_kind(kind);
    let channel = height_channel(viz_kind, size_bytes, child_count);
    ChildSpec { name: name.to_string(), kind: viz_kind, height: world_height(channel) }
}

// ── Wireframe / point mesh vertex builders ──────────────────────────────────

/// One line segment, world-space endpoints (`[x, y, z]`) — the raw unit
/// [`bevy::mesh::PrimitiveTopology::LineList`] wants: every consecutive pair
/// of positions in a flattened buffer is one segment.
pub type Segment = [[f32; 3]; 2];

/// Flatten a slice of [`Segment`]s into a `LineList`-ready position buffer
/// (2 positions per segment, in order).
pub fn flatten_segments(segments: &[Segment]) -> Vec<[f32; 3]> {
    segments.iter().flat_map(|s| [s[0], s[1]]).collect()
}

/// The prism wireframe for one laid-out cell: its bottom-ring edges (`y =
/// 0`), its top-ring edges (`y = cell.height`), and one vertical at each
/// vertex. Segment count is exactly `3 × vertex_count`
/// (`cell.edges().count()` bottom + the same top + one vertical per vertex)
/// — `field_wireframe`'s own doc restates this as the field-level invariant.
/// Degenerate cells (< 2 vertices) yield no segments, matching
/// [`FsnCell::edges`]'s own empty-iterator case.
pub fn prism_wireframe(cell: &FsnCell) -> Vec<Segment> {
    let edges: Vec<_> = cell.edges().collect();
    let h = cell.height;
    let mut segments = Vec::with_capacity(edges.len() * 3);
    for (a, b) in &edges {
        segments.push([[a.x as f32, 0.0, a.y as f32], [b.x as f32, 0.0, b.y as f32]]);
    }
    for (a, b) in &edges {
        segments.push([[a.x as f32, h, a.y as f32], [b.x as f32, h, b.y as f32]]);
    }
    for v in &cell.polygon {
        segments.push([[v.x as f32, 0.0, v.y as f32], [v.x as f32, h, v.y as f32]]);
    }
    segments
}

/// The whole field's prism wireframe — every cell's [`prism_wireframe`],
/// concatenated into one buffer (one `Mesh` per directory field, not per
/// cell — the task's own entity-count discipline). Total segment count is
/// `Σ 3 × vertex_count` over every cell, exactly what the task's own test
/// plan calls out.
pub fn field_wireframe(field: &FsnField) -> Vec<Segment> {
    field.cells.iter().flat_map(prism_wireframe).collect()
}

/// World-Y the quad-seam grid sits at — a hair above the floor, matching the
/// room floor traces' own `TRACE_Y` idiom (avoid z-fighting with anything
/// drawn flush at `y = 0`).
pub const SEAM_Y: f32 = 0.8;

/// The faint quad-seam grid for one directory's own rect: its outer boundary
/// (4 edges) plus the internal cross dividing it into the 4 quadrants
/// [`kaijutsu_viz::fsn::placeholder_quadrant`] assigns children into — always
/// exactly 6 segments, independent of how many children the field actually
/// has (frame 45's "faint dotted boundaries" read as architecture, not
/// per-child decoration). **First-candidate simplification, documented**:
/// slice 0 draws the structural quadrant grid rather than only the
/// quadrants some child actually occupies — a future pass could prune empty
/// quadrants once Open Question 2 (`kaijutsu_viz::fsn`'s own doc) settles
/// the real slack/placement policy.
pub fn seam_grid(rect: Rect) -> Vec<Segment> {
    let (x0, x1, z0, z1) =
        (rect.x0 as f32, rect.x1 as f32, rect.y0 as f32, rect.y1 as f32);
    let mx = (x0 + x1) * 0.5;
    let mz = (z0 + z1) * 0.5;
    let y = SEAM_Y;
    vec![
        [[x0, y, z0], [x1, y, z0]],
        [[x1, y, z0], [x1, y, z1]],
        [[x1, y, z1], [x0, y, z1]],
        [[x0, y, z1], [x0, y, z0]],
        [[mx, y, z0], [mx, y, z1]],
        [[x0, y, mz], [x1, y, mz]],
    ]
}

/// Every cell's top-ring vertices, world space (`y = cell.height`) — the
/// vertex-point markers' source positions at the Attended tier.
pub fn field_top_vertices(field: &FsnField) -> Vec<[f32; 3]> {
    field
        .cells
        .iter()
        .flat_map(|c| c.polygon.iter().map(move |v| [v.x as f32, c.height, v.y as f32]))
        .collect()
}

/// Every cell's seed point, world space, held at ground level (`y = 0`) —
/// the sparse-tier "unbuilt Dyson shell" markers (one dot per child,
/// regardless of whether that child's own listing is known yet).
pub fn field_seed_points(field: &FsnField) -> Vec<[f32; 3]> {
    field.cells.iter().map(|c| [c.seed.x as f32, 0.0, c.seed.y as f32]).collect()
}

/// One point marker's raw octahedron (6 vertices, 8 triangles) — the shared
/// template [`point_marker_mesh_data`] instances per position.
const MARKER_VERTS: [[f32; 3]; 6] =
    [[1.0, 0.0, 0.0], [-1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, -1.0, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0, -1.0]];
const MARKER_INDICES: [u32; 24] =
    [2, 0, 4, 2, 4, 1, 2, 1, 5, 2, 5, 0, 3, 4, 0, 3, 1, 4, 3, 5, 1, 3, 0, 5];

/// Build ONE combined triangle mesh's raw buffers for every point in
/// `positions`: a tiny octahedron of `size` at each, so a whole field's
/// vertex/seed points cost one extra `Mesh`, not one entity per point (the
/// task's own "acceptable" fallback for point sprites without a custom
/// pipeline). Returns `(positions, indices)` ready for
/// `Mesh::ATTRIBUTE_POSITION` / `Indices::U32`.
pub fn point_marker_mesh_data(positions: &[[f32; 3]], size: f32) -> (Vec<[f32; 3]>, Vec<u32>) {
    let mut out_positions = Vec::with_capacity(positions.len() * MARKER_VERTS.len());
    let mut out_indices = Vec::with_capacity(positions.len() * MARKER_INDICES.len());
    for (i, p) in positions.iter().enumerate() {
        let base = (i * MARKER_VERTS.len()) as u32;
        for v in &MARKER_VERTS {
            out_positions.push([p[0] + v[0] * size, p[1] + v[1] * size, p[2] + v[2] * size]);
        }
        for idx in &MARKER_INDICES {
            out_indices.push(base + idx);
        }
    }
    (out_positions, out_indices)
}

// ── LOD tier (vfs.md claim 5) ────────────────────────────────────────────────

/// The three render tiers, in the enumeration/render ladder's own order
/// (`docs/scenes/vfs.md`, "LOD ladder").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LodTier {
    /// Unenumerated OR enumerated-but-far: seed points + the seam grid only.
    Sparse,
    /// Listed, mid-distance: the wireframe prisms.
    Wireframe,
    /// Near/attended: prisms + vertex points, full-brightness edges.
    Attended,
}

/// Camera-distance thresholds (world units) — **Amy-tunable, first guess,
/// live-tune over BRP**. `LOD_NEAR` is the Attended boundary, `LOD_FAR` the
/// Wireframe→Sparse boundary; both are read against the field's own rect
/// center (`super::scene`'s call site), not any single cell.
pub const LOD_NEAR: f32 = 700.0;
pub const LOD_FAR: f32 = 2200.0;

/// Decide a directory-field's render tier from whether it's enumerated (see
/// the module doc's note on why every slice-0 call site passes `true`) and
/// the camera's distance to it. Called every frame the field is live
/// (ambient, both zoom directions — the time-well freeze-fix lesson:
/// re-evaluate unconditionally rather than latching on approach only, so
/// retreating degrades the tier back down instead of freezing it at
/// whatever it last was).
pub fn lod_tier(enumerated: bool, camera_dist: f32) -> LodTier {
    if !enumerated || camera_dist > LOD_FAR {
        LodTier::Sparse
    } else if camera_dist <= LOD_NEAR {
        LodTier::Attended
    } else {
        LodTier::Wireframe
    }
}

// ── Camera fly clamps ────────────────────────────────────────────────────────

/// World-Y altitude bounds for the fly camera. **Amy-tunable.**
pub const CAM_ALTITUDE_MIN: f32 = 150.0;
pub const CAM_ALTITUDE_MAX: f32 = 3200.0;

/// How far past the root square's own edge (`ROOT_WORLD_SIZE / 2`) the
/// camera may still fly before clamping — enough slack to look back at the
/// whole world from outside it. **Amy-tunable.**
pub const CAM_BOUNDS_MARGIN: f32 = 1200.0;

/// Clamp a proposed camera XZ position to the world bounds
/// ([`ROOT_WORLD_SIZE`] `/ 2 + `[`CAM_BOUNDS_MARGIN`] on every side).
pub fn clamp_camera_xz(x: f32, z: f32) -> (f32, f32) {
    let half = ROOT_WORLD_SIZE * 0.5 + CAM_BOUNDS_MARGIN;
    (x.clamp(-half, half), z.clamp(-half, half))
}

/// Clamp a proposed camera altitude to [`CAM_ALTITUDE_MIN`]..[`CAM_ALTITUDE_MAX`].
pub fn clamp_altitude(y: f32) -> f32 {
    y.clamp(CAM_ALTITUDE_MIN, CAM_ALTITUDE_MAX)
}

// ── Selection: forward-ray ground intersection + nearest cell ──────────────

/// Where a camera's forward ray crosses the world's `y = 0` ground plane, or
/// `None` if the ray never reaches it (looking level or up — `forward.y >=
/// 0` — or the intersection point is behind the camera).
pub fn ray_ground_intersection(cam_pos: [f32; 3], cam_forward: [f32; 3]) -> Option<[f32; 2]> {
    if cam_forward[1] >= -1e-6 {
        return None;
    }
    let t = -cam_pos[1] / cam_forward[1];
    if t <= 0.0 {
        return None;
    }
    Some([cam_pos[0] + cam_forward[0] * t, cam_pos[2] + cam_forward[2] * t])
}

/// The index of `candidates`' closest point to `point` (squared XZ
/// distance), or `None` for an empty slice.
pub fn nearest_index(point: [f32; 2], candidates: &[[f32; 2]]) -> Option<usize> {
    candidates
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let dx = c[0] - point[0];
            let dz = c[1] - point[1];
            (i, dx * dx + dz * dz)
        })
        .min_by(|a, b| a.1.partial_cmp(&b.1).expect("distances are always finite"))
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_viz::fsn::layout_field;

    // ── path_components / join_path ──

    #[test]
    fn path_components_splits_and_ignores_empties() {
        assert_eq!(path_components("/"), Vec::<&str>::new());
        assert_eq!(path_components("/foo/bar"), vec!["foo", "bar"]);
        assert_eq!(path_components("/foo/bar/"), vec!["foo", "bar"]);
        assert_eq!(path_components(""), Vec::<&str>::new());
    }

    #[test]
    fn join_path_avoids_double_slash_at_root() {
        assert_eq!(join_path("/", "foo"), "/foo");
        assert_eq!(join_path("/foo", "bar"), "/foo/bar");
    }

    // ── root_world_rect / map_unit_rect_to_world / path_world_rect ──

    #[test]
    fn root_world_rect_is_centered_with_the_configured_side_length() {
        let r = root_world_rect();
        assert_eq!(r.width(), ROOT_WORLD_SIZE as f64);
        assert_eq!(r.height(), ROOT_WORLD_SIZE as f64);
        assert_eq!(r.x0, -(ROOT_WORLD_SIZE as f64) / 2.0);
        assert_eq!(r.x1, (ROOT_WORLD_SIZE as f64) / 2.0);
    }

    #[test]
    fn map_unit_rect_identity_returns_the_root_rect_unchanged() {
        let root = root_world_rect();
        let mapped = map_unit_rect_to_world(root, Rect::unit());
        assert_eq!(mapped, root);
    }

    #[test]
    fn map_unit_rect_quarter_lands_in_the_expected_world_quadrant() {
        let root = root_world_rect();
        // Unit rect [0.5,1]x[0.5,1] (top-right quadrant) should map to the
        // root's own top-right quadrant.
        let unit = Rect { x0: 0.5, y0: 0.5, x1: 1.0, y1: 1.0 };
        let mapped = map_unit_rect_to_world(root, unit);
        assert_eq!(mapped.x0, 0.0);
        assert_eq!(mapped.y0, 0.0);
        assert_eq!(mapped.x1, root.x1);
        assert_eq!(mapped.y1, root.y1);
    }

    #[test]
    fn path_world_rect_of_the_root_is_the_whole_root_rect() {
        let root = root_world_rect();
        let rect = path_world_rect(root, &[]);
        assert_eq!(rect, root);
    }

    #[test]
    fn path_world_rect_of_a_subdirectory_is_a_quarter_of_its_parent() {
        let root = root_world_rect();
        let child_rect = path_world_rect(root, &["some_dir"]);
        // Whatever quadrant it landed in, its area must be exactly 1/4 the
        // parent's (CellId::quad_rect's own guarantee, composed through the
        // world mapping — quad_rect_quarters_are_disjoint_and_cover_the_parent
        // in kaijutsu-viz already locks the unit-space half of this).
        assert!((child_rect.area() - root.area() / 4.0).abs() < 1e-6);
        assert!(root.x0 <= child_rect.x0 && child_rect.x1 <= root.x1);
        assert!(root.y0 <= child_rect.y0 && child_rect.y1 <= root.y1);
    }

    #[test]
    fn path_world_rect_of_a_grandchild_is_a_sixteenth() {
        let root = root_world_rect();
        let rect = path_world_rect(root, &["a", "b"]);
        assert!((rect.area() - root.area() / 16.0).abs() < 1e-6);
    }

    // ── height_channel / world_height ──

    #[test]
    fn file_height_channel_treats_zero_bytes_as_one() {
        assert_eq!(height_channel(NodeKind::File, 0, 0), 1.0);
        assert_eq!(height_channel(NodeKind::File, 1, 0), 1.0);
    }

    #[test]
    fn file_height_channel_is_log2_of_size_plus_one() {
        let h = height_channel(NodeKind::File, 1024, 0);
        assert!((h - 11.0).abs() < 1e-4, "1 + log2(1024) = 11, got {h}");
    }

    #[test]
    fn dir_height_channel_grows_with_child_count_and_caps() {
        assert_eq!(height_channel(NodeKind::Dir, 0, 0), 1.0);
        assert_eq!(height_channel(NodeKind::Dir, 0, 5), 6.0);
        assert_eq!(height_channel(NodeKind::Dir, 0, 10_000), DIR_HEIGHT_CAP, "must cap");
    }

    #[test]
    fn symlink_height_channel_is_a_flat_stub() {
        assert_eq!(height_channel(NodeKind::Symlink, 999_999, 50), 1.0);
    }

    #[test]
    fn world_height_scales_the_channel_and_never_goes_negative() {
        assert_eq!(world_height(1.0), HEIGHT_WORLD_SCALE);
        assert_eq!(world_height(0.0), 0.0);
        assert_eq!(world_height(-5.0), 0.0, "a negative channel must not carve a pit");
    }

    #[test]
    fn to_viz_kind_maps_every_wire_variant() {
        assert_eq!(to_viz_kind(VfsFileType::File), NodeKind::File);
        assert_eq!(to_viz_kind(VfsFileType::Directory), NodeKind::Dir);
        assert_eq!(to_viz_kind(VfsFileType::Symlink), NodeKind::Symlink);
    }

    #[test]
    fn child_spec_combines_kind_and_scaled_height() {
        let c = child_spec("big.log", VfsFileType::File, 1024, 0);
        assert_eq!(c.name, "big.log");
        assert_eq!(c.kind, NodeKind::File);
        assert!((c.height - world_height(11.0)).abs() < 1e-3);
    }

    // ── prism_wireframe / field_wireframe (segment-count invariant) ──

    #[test]
    fn prism_wireframe_segment_count_is_3x_vertex_count() {
        let rect = Rect::unit();
        let children: Vec<ChildSpec> = (0..5)
            .map(|i| ChildSpec { name: format!("f{i}"), kind: NodeKind::File, height: 12.0 })
            .collect();
        let field = layout_field(rect, &children);
        for cell in &field.cells {
            let segs = prism_wireframe(cell);
            assert_eq!(segs.len(), 3 * cell.polygon.len(), "{}: 3x vertex count", cell.name);
        }
    }

    #[test]
    fn prism_wireframe_bottom_edges_sit_at_y_zero_and_top_at_cell_height() {
        let rect = Rect { x0: 0.0, y0: 0.0, x1: 10.0, y1: 10.0 };
        let children = vec![ChildSpec { name: "solo".into(), kind: NodeKind::Dir, height: 42.0 }];
        let field = layout_field(rect, &children);
        let cell = &field.cells[0];
        let segs = prism_wireframe(cell);
        let n = cell.polygon.len();
        // First n segments are the bottom ring.
        for s in &segs[0..n] {
            assert_eq!(s[0][1], 0.0);
            assert_eq!(s[1][1], 0.0);
        }
        // Next n segments are the top ring.
        for s in &segs[n..2 * n] {
            assert_eq!(s[0][1], 42.0);
            assert_eq!(s[1][1], 42.0);
        }
        // Final n segments are the verticals: one endpoint at 0, one at height.
        for s in &segs[2 * n..3 * n] {
            let ys = [s[0][1], s[1][1]];
            assert!(ys.contains(&0.0) && ys.contains(&42.0));
        }
    }

    #[test]
    fn field_wireframe_totals_the_3x_invariant_across_every_cell() {
        let rect = Rect::unit();
        let children: Vec<ChildSpec> = (0..9)
            .map(|i| ChildSpec { name: format!("n{i}"), kind: NodeKind::File, height: 3.0 })
            .collect();
        let field = layout_field(rect, &children);
        let expected: usize = field.cells.iter().map(|c| 3 * c.polygon.len()).sum();
        assert_eq!(field_wireframe(&field).len(), expected);
    }

    #[test]
    fn field_wireframe_of_an_empty_field_is_empty() {
        let field = layout_field(Rect::unit(), &[]);
        assert!(field_wireframe(&field).is_empty());
    }

    #[test]
    fn flatten_segments_yields_two_positions_per_segment_in_order() {
        let segs: Vec<Segment> = vec![
            [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0]],
            [[1.0, 0.0, 0.0], [1.0, 1.0, 0.0]],
        ];
        let flat = flatten_segments(&segs);
        assert_eq!(flat.len(), 4);
        assert_eq!(flat[0], [0.0, 0.0, 0.0]);
        assert_eq!(flat[1], [1.0, 0.0, 0.0]);
        assert_eq!(flat[2], [1.0, 0.0, 0.0]);
        assert_eq!(flat[3], [1.0, 1.0, 0.0]);
    }

    // ── seam_grid ──

    #[test]
    fn seam_grid_is_always_six_segments() {
        let rect = Rect { x0: -5.0, y0: -5.0, x1: 5.0, y1: 5.0 };
        assert_eq!(seam_grid(rect).len(), 6);
    }

    #[test]
    fn seam_grid_segments_stay_within_the_rect_bounds() {
        let rect = Rect { x0: -5.0, y0: -3.0, x1: 5.0, y1: 3.0 };
        for seg in seam_grid(rect) {
            for p in seg {
                assert!(p[0] >= -5.0 - 1e-4 && p[0] <= 5.0 + 1e-4, "x out of bounds: {p:?}");
                assert!(p[2] >= -3.0 - 1e-4 && p[2] <= 3.0 + 1e-4, "z out of bounds: {p:?}");
            }
        }
    }

    #[test]
    fn seam_grid_cross_lines_pass_through_the_rect_center() {
        let rect = Rect { x0: 0.0, y0: 0.0, x1: 10.0, y1: 20.0 };
        let segs = seam_grid(rect);
        // Segments 4 and 5 are the internal cross (vertical then horizontal).
        let vertical = segs[4];
        let horizontal = segs[5];
        assert_eq!(vertical[0][0], 5.0, "vertical cross at the x midpoint");
        assert_eq!(horizontal[0][2], 10.0, "horizontal cross at the z midpoint");
    }

    // ── field_top_vertices / field_seed_points ──

    #[test]
    fn field_top_vertices_count_matches_total_polygon_vertices() {
        let rect = Rect::unit();
        let children: Vec<ChildSpec> = (0..6)
            .map(|i| ChildSpec { name: format!("v{i}"), kind: NodeKind::File, height: 7.0 })
            .collect();
        let field = layout_field(rect, &children);
        let expected: usize = field.cells.iter().map(|c| c.polygon.len()).sum();
        assert_eq!(field_top_vertices(&field).len(), expected);
    }

    #[test]
    fn field_top_vertices_sit_at_each_cells_own_height() {
        let rect = Rect { x0: 0.0, y0: 0.0, x1: 10.0, y1: 10.0 };
        let children = vec![ChildSpec { name: "solo".into(), kind: NodeKind::Dir, height: 9.0 }];
        let field = layout_field(rect, &children);
        for v in field_top_vertices(&field) {
            assert_eq!(v[1], 9.0);
        }
    }

    #[test]
    fn field_seed_points_one_per_cell_at_ground_level() {
        let rect = Rect::unit();
        let children: Vec<ChildSpec> = (0..4)
            .map(|i| ChildSpec { name: format!("s{i}"), kind: NodeKind::File, height: 5.0 })
            .collect();
        let field = layout_field(rect, &children);
        let points = field_seed_points(&field);
        assert_eq!(points.len(), field.cells.len());
        for p in points {
            assert_eq!(p[1], 0.0);
        }
    }

    // ── point_marker_mesh_data ──

    #[test]
    fn point_marker_mesh_data_sizes_scale_with_input_count() {
        let positions = vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [20.0, 5.0, 0.0]];
        let (verts, indices) = point_marker_mesh_data(&positions, 2.0);
        assert_eq!(verts.len(), positions.len() * 6);
        assert_eq!(indices.len(), positions.len() * 24);
        assert!(indices.iter().all(|&i| (i as usize) < verts.len()));
    }

    #[test]
    fn point_marker_mesh_data_empty_input_is_empty() {
        let (verts, indices) = point_marker_mesh_data(&[], 1.0);
        assert!(verts.is_empty() && indices.is_empty());
    }

    #[test]
    fn point_marker_octahedron_is_centered_on_its_position() {
        let (verts, _) = point_marker_mesh_data(&[[3.0, 4.0, 5.0]], 1.0);
        let cx: f32 = verts.iter().map(|v| v[0]).sum::<f32>() / verts.len() as f32;
        let cy: f32 = verts.iter().map(|v| v[1]).sum::<f32>() / verts.len() as f32;
        let cz: f32 = verts.iter().map(|v| v[2]).sum::<f32>() / verts.len() as f32;
        assert!((cx - 3.0).abs() < 1e-5);
        assert!((cy - 4.0).abs() < 1e-5);
        assert!((cz - 5.0).abs() < 1e-5);
    }

    // ── lod_tier ──

    #[test]
    fn unenumerated_is_always_sparse_regardless_of_distance() {
        assert_eq!(lod_tier(false, 0.0), LodTier::Sparse);
        assert_eq!(lod_tier(false, 100_000.0), LodTier::Sparse);
    }

    #[test]
    fn enumerated_near_is_attended() {
        assert_eq!(lod_tier(true, 0.0), LodTier::Attended);
        assert_eq!(lod_tier(true, LOD_NEAR), LodTier::Attended);
    }

    #[test]
    fn enumerated_mid_is_wireframe() {
        assert_eq!(lod_tier(true, LOD_NEAR + 1.0), LodTier::Wireframe);
        assert_eq!(lod_tier(true, LOD_FAR), LodTier::Wireframe);
    }

    #[test]
    fn enumerated_far_is_sparse() {
        assert_eq!(lod_tier(true, LOD_FAR + 1.0), LodTier::Sparse);
    }

    #[test]
    fn lod_tier_reacts_to_both_directions_symmetrically() {
        // Same distance, computed fresh each time (the freeze-fix contract:
        // no hidden latch — going near-far-near must retrace the same tiers).
        let seq = [100.0, 3000.0, 100.0];
        let tiers: Vec<LodTier> = seq.iter().map(|&d| lod_tier(true, d)).collect();
        assert_eq!(tiers[0], LodTier::Attended);
        assert_eq!(tiers[1], LodTier::Sparse);
        assert_eq!(tiers[2], LodTier::Attended);
    }

    // ── camera clamps ──

    #[test]
    fn clamp_camera_xz_passes_through_in_bounds_values() {
        assert_eq!(clamp_camera_xz(0.0, 0.0), (0.0, 0.0));
    }

    #[test]
    fn clamp_camera_xz_clamps_out_of_bounds_values() {
        let half = ROOT_WORLD_SIZE * 0.5 + CAM_BOUNDS_MARGIN;
        let (x, z) = clamp_camera_xz(half * 10.0, -half * 10.0);
        assert_eq!(x, half);
        assert_eq!(z, -half);
    }

    #[test]
    fn clamp_altitude_clamps_both_ends() {
        assert_eq!(clamp_altitude(CAM_ALTITUDE_MIN - 100.0), CAM_ALTITUDE_MIN);
        assert_eq!(clamp_altitude(CAM_ALTITUDE_MAX + 100.0), CAM_ALTITUDE_MAX);
        let mid = (CAM_ALTITUDE_MIN + CAM_ALTITUDE_MAX) * 0.5;
        assert_eq!(clamp_altitude(mid), mid);
    }

    // ── ray_ground_intersection / nearest_index ──

    #[test]
    fn ray_ground_intersection_hits_the_expected_point() {
        // Camera at (0, 100, 0) looking straight down: hits (0, 0).
        let hit = ray_ground_intersection([0.0, 100.0, 0.0], [0.0, -1.0, 0.0]);
        assert_eq!(hit, Some([0.0, 0.0]));
    }

    #[test]
    fn ray_ground_intersection_none_when_looking_level_or_up() {
        assert_eq!(ray_ground_intersection([0.0, 100.0, 0.0], [1.0, 0.0, 0.0]), None);
        assert_eq!(ray_ground_intersection([0.0, 100.0, 0.0], [0.0, 1.0, 0.0]), None);
    }

    #[test]
    fn ray_ground_intersection_angled_ray_lands_on_the_correct_side() {
        // Looking down and forward along -Z from height 10 at a 45-degree
        // pitch: should land 10 units out along -Z.
        let dir = [0.0, -std::f32::consts::FRAC_1_SQRT_2, -std::f32::consts::FRAC_1_SQRT_2];
        let hit = ray_ground_intersection([0.0, 10.0, 0.0], dir).unwrap();
        assert!((hit[0]).abs() < 1e-3);
        assert!((hit[1] - (-10.0)).abs() < 1e-2);
    }

    #[test]
    fn nearest_index_finds_the_closest_candidate() {
        let candidates = [[0.0, 0.0], [10.0, 0.0], [-3.0, 4.0]];
        assert_eq!(nearest_index([-2.5, 4.5], &candidates), Some(2));
        assert_eq!(nearest_index([9.0, 1.0], &candidates), Some(1));
    }

    #[test]
    fn nearest_index_empty_candidates_is_none() {
        assert_eq!(nearest_index([0.0, 0.0], &[]), None);
    }
}
