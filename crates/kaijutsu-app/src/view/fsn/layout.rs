//! Pure FSN-world math: VFS-path → world-space placement, the height-channel
//! mapping, wireframe/point mesh vertex builders, the LOD-tier decision, and
//! the camera-fly clamps. No Bevy types (mirrors `time_well::card` /
//! `room::bearing`'s stance) so every rule here is a plain-data unit test —
//! [`super::scene`] is the only place any of this touches an `Entity`.
//!
//! # Coordinate convention and field placement
//!
//! The FSN world lives on the room's XZ ground plane, +Y up — same
//! convention as `room::bearing`. [`kaijutsu_viz::fsn::Rect`]'s `y0`/`y1`
//! fields read as world **Z** (never world Y/height); `layout_field` is a
//! pure function of whatever rect you hand it, so a directory's children
//! are laid out ONCE, directly in world space.
//!
//! Where a directory's field rect comes from:
//! - **root** — [`root_world_rect`], a fixed square at the origin.
//! - **subdirectory** — the axis-aligned bounding box of its OWN Voronoi
//!   cell in its parent's already-laid-out field, inset by
//!   [`SUBFIELD_INSET_FRAC`] ([`cell_bbox_inset`]; `super::scene::
//!   sync_fsn_fields` does the parent lookup). An earlier draft descended
//!   the `CellId` quadtree via `placeholder_quadrant` instead — but that
//!   placeholder is `hash(name) % 4`, so colliding sibling directories got
//!   the EXACT same world rect and stacked whole fields on top of each
//!   other (guaranteed with ~10 root children into 4 quadrants). Parent
//!   Voronoi cells are disjoint by construction, so their inset bboxes may
//!   brush at the margins but can never coincide — and Lane A's own
//!   placeholder doc already said this is where subdirs live ("subdirs
//!   bloom as clusters inside the PARENT's Voronoi field").
//!
//! Determinism: a parent field is a pure function of its (rect, listing)
//! pair (`kaijutsu_viz::fsn`'s contract), so the bbox chain — root square →
//! cell bbox → child field → its cells' bboxes → … — is identical on every
//! client, the same layout-is-a-pure-function-of-the-namespace guarantee
//! the quadtree descent had.
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
use kaijutsu_viz::fsn::{ChildSpec, FsnCell, FsnField, NodeKind, Rect, Vec2};

// ── World placement (Amy-tunable) ───────────────────────────────────────────

/// Side length (world units) of the root directory's square footprint on the
/// XZ plane, centered at the world origin. 2000 → 3000 (Amy's 50%,
/// 2026-07-13, single-portal retune), then 3000 → 12000 the same evening
/// (Amy: "what if our fsn was MUCH bigger... maybe even a horizon"): this
/// time the world outgrows the camera ON PURPOSE — the orbit moves out
/// only ~2.5× ([`ORBIT_RADIUS`]/[`ORBIT_HEIGHT`]'s own doc), so instead of
/// the framing holding, the world overflows the frustum: the near districts
/// slide under the camera, the far edge drops toward the skyline, and the
/// glass reads as a horizon over a sprawl rather than a diorama on a
/// table. (A uniform world+orbit scale is a visual no-op — the mismatch IS
/// the effect.) **Amy-tunable.**
pub const ROOT_WORLD_SIZE: f32 = 12_000.0;

/// The root directory's world-space rect: a square of [`ROOT_WORLD_SIZE`],
/// centered at the origin (`y` fields read as world Z — see the module doc).
pub fn root_world_rect() -> Rect {
    let half = (ROOT_WORLD_SIZE as f64) * 0.5;
    Rect { x0: -half, y0: -half, x1: half, y1: half }
}

/// Join a directory's own absolute path with a child's name — `"/"` + `"foo"`
/// → `"/foo"` (no double slash), `"/foo"` + `"bar"` → `"/foo/bar"`.
pub fn join_path(base: &str, name: &str) -> String {
    if base == "/" { format!("/{name}") } else { format!("{base}/{name}") }
}

/// Split an absolute path into `(parent, name)`: `"/foo"` → `("/", "foo")`,
/// `"/foo/bar"` → `("/foo", "bar")`; `None` for the root (no parent) or a
/// degenerate path. A trailing slash is tolerated (`"/foo/"` splits like
/// `"/foo"`). Round-trips with [`join_path`] — the inverse the field-placement
/// chain needs to find a subdirectory's own cell in its parent's field.
pub fn split_parent(path: &str) -> Option<(&str, &str)> {
    if path == "/" {
        return None;
    }
    let trimmed = path.strip_suffix('/').unwrap_or(path);
    let (parent, name) = trimmed.rsplit_once('/')?;
    if name.is_empty() {
        return None;
    }
    Some((if parent.is_empty() { "/" } else { parent }, name))
}

/// Fraction of a cell's bounding box shaved off EACH side when placing a
/// subdirectory's own field inside it — breathing room so a child field's
/// prisms don't butt flush against the parent cell's own edges.
/// **Amy-tunable.**
pub const SUBFIELD_INSET_FRAC: f64 = 0.12;

/// Minimum width/height (world units) an inset cell bbox must keep to host
/// a subdirectory field. Below this the cell is too small for a legible
/// field — and, harder, [`kaijutsu_viz::fsn::layout_field`] PANICS on a
/// non-positive-extent rect, so the floor here is what lets
/// [`cell_bbox_inset`] promise its `Some` is always safe to feed onward.
/// **Amy-tunable.**
pub const SUBFIELD_MIN_EXTENT: f64 = 4.0;

/// The world rect a subdirectory's own field occupies: the axis-aligned
/// bounding box of its Voronoi-cell `polygon` in the PARENT's laid-out
/// field, inset by `inset_frac` of the bbox's own width/height on each side
/// (see the module doc for why this replaced quadtree-quadrant descent).
///
/// `None` when the cell can't host a field: fewer than 3 vertices (a
/// degenerate clip), non-finite coordinates, or an inset bbox thinner than
/// [`SUBFIELD_MIN_EXTENT`] in either axis — the caller skips building that
/// subfield rather than feeding `layout_field` a rect it would panic on.
/// A returned rect always has both extents `>= SUBFIELD_MIN_EXTENT`.
pub fn cell_bbox_inset(polygon: &[Vec2], inset_frac: f64) -> Option<Rect> {
    if polygon.len() < 3 {
        return None;
    }
    let (mut x0, mut y0) = (f64::INFINITY, f64::INFINITY);
    let (mut x1, mut y1) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    for v in polygon {
        if !v.x.is_finite() || !v.y.is_finite() {
            return None;
        }
        x0 = x0.min(v.x);
        y0 = y0.min(v.y);
        x1 = x1.max(v.x);
        y1 = y1.max(v.y);
    }
    let dx = (x1 - x0) * inset_frac;
    let dy = (y1 - y0) * inset_frac;
    let rect = Rect { x0: x0 + dx, y0: y0 + dy, x1: x1 - dx, y1: y1 - dy };
    if rect.width() < SUBFIELD_MIN_EXTENT || rect.height() < SUBFIELD_MIN_EXTENT {
        return None;
    }
    Some(rect)
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
/// (4 edges) plus an internal quartering cross — always exactly 6 segments,
/// independent of how many children the field actually has (frame 45's
/// "faint dotted boundaries" read as architecture, not per-child
/// decoration). **Purely cosmetic**: since field placement moved to
/// parent-cell bboxes ([`cell_bbox_inset`] — see the module doc), the cross
/// no longer corresponds to any addressing quadrant; it stays as the quad
/// seam *look* the concept frames show, pending Open Question 2
/// (`kaijutsu_viz::fsn`'s own doc) settling the real address/seam grammar.
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

/// Vertices contributed per point by [`point_marker_mesh_data`] — the marker
/// template's own vertex count, exposed so callers can build a matching
/// per-vertex-color buffer (e.g. [`super::scene`]'s recency tint, expanded
/// via [`per_cell_colors`] once per marker) without duplicating the literal
/// `6` [`MARKER_VERTS`] already encodes.
pub const MARKER_VERT_COUNT: usize = MARKER_VERTS.len();

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

// ── Recency glow (A1 — vertex-color RELATIVE tint, baked at mesh build) ─────
//
// The color-composition law this module's half of: vertex colors carry
// RECENCY as a per-channel tint toward gold, `super::scene_palette::
// warmth_tint` bakes `tint = lerp(1.0, gold_c/base_c, w)` so `tint × base_color
// == lerp(base_color, gold, w)` exactly once the material's own base_color is
// multiplied in at render time. This half only computes `w` (0..1, how gold)
// from an mtime age — the tint math itself lives in `scene_palette` (it needs
// the palette's hues, which this Bevy-free module doesn't touch).

/// Age (seconds) at or under which a cell reads as maximally fresh — full
/// gold tint weight. An hour. **Amy-tunable.**
pub const RECENCY_FRESH_SECS: f64 = 3_600.0;
/// Age (seconds) at or over which a cell has faded to no tint at all — 90
/// days. **Amy-tunable.**
pub const RECENCY_OLD_SECS: f64 = 90.0 * 86_400.0;

/// Recency tint weight (0..1) for a cell whose mtime is `age_secs` old: 1.0
/// at/under [`RECENCY_FRESH_SECS`], 0.0 at/over [`RECENCY_OLD_SECS`], a log
/// ramp between — `1 - ln(age/fresh) / ln(old/fresh)`, so the falloff eases
/// rather than dropping linearly (a file just past "fresh" still reads
/// mostly warm). A negative age (clock skew, a future mtime) clamps to 1.0
/// — the freshest reading; a NaN age clamps to 1.0 too (the only way to
/// dodge `ln`'s NaN-in-NaN-out without a fresh/old special case); `+inf`
/// falls out through the ordinary `>= RECENCY_OLD_SECS` check to 0.0, which
/// already reads correctly (infinitely old).
pub fn recency_weight(age_secs: f64) -> f32 {
    if age_secs.is_nan() || age_secs <= RECENCY_FRESH_SECS {
        return 1.0;
    }
    if age_secs >= RECENCY_OLD_SECS {
        return 0.0;
    }
    let ratio = age_secs / RECENCY_FRESH_SECS;
    let span = RECENCY_OLD_SECS / RECENCY_FRESH_SECS;
    let w = 1.0 - ratio.ln() / span.ln();
    w.clamp(0.0, 1.0) as f32
}

/// Expand one color per cell into one color per vertex, given each cell's
/// own vertex count (e.g. [`prism_wireframe`]'s `3 × vertex_count` — this
/// function doesn't care WHICH per-cell layout produced the count, only that
/// `vert_counts` and `cell_colors` are the same two zipped sequences the
/// caller already built cell-by-cell). Debug-asserts the lengths match
/// (a caller bug, not a runtime condition this pure fn should paper over
/// with a silent truncation — crashing in debug/test builds beats a subtly
/// wrong paint job shipping to release).
pub fn per_cell_colors(vert_counts: &[usize], cell_colors: &[[f32; 4]]) -> Vec<[f32; 4]> {
    debug_assert_eq!(
        vert_counts.len(),
        cell_colors.len(),
        "per_cell_colors: exactly one color per cell"
    );
    vert_counts
        .iter()
        .zip(cell_colors)
        .flat_map(|(&n, &c)| std::iter::repeat_n(c, n))
        .collect()
}

// ── The vessel (A3, reframed 2026-07-13: the ship IS the octagon room) ──────

/// Vessel-silhouette ring radius (world units, upper ring). Bumped 220 →
/// 440 when the vessel moved from hovering over the world center to riding
/// [`orbit_pose`]'s own path (Amy, 2026-07-13: "from a view on/in the fsn
/// you'd see an octagon orbiting") — at orbit distance the old radius read
/// as a speck. **Amy-tunable.**
pub const SHIP_RADIUS: f32 = 440.0;
/// The lower ring's radius as a fraction of [`SHIP_RADIUS`] — a taper toward
/// the belly. **Amy-tunable.**
pub const SHIP_LOWER_RADIUS_FRAC: f32 = 0.6;
/// How far below the upper ring the lower ring sits. **Amy-tunable.**
pub const SHIP_LOWER_DROP: f32 = 60.0;
/// How far the nose spine projects (local −Z — the vessel's own N window
/// bearing; `super::scene`'s orbit system yaws it to face the world center)
/// beyond the upper ring's own radius. **Amy-tunable.**
pub const SHIP_NOSE_LENGTH: f32 = 140.0;

/// Ring tessellation (octagon, matching the room's own wall-shell
/// convention) for both silhouette rings.
const SHIP_RING_SIDES: usize = 8;

/// One ring's vertices, evenly spaced in the XZ plane at height `y`.
fn ring_points(radius: f32, y: f32) -> Vec<[f32; 3]> {
    (0..SHIP_RING_SIDES)
        .map(|i| {
            let theta = (i as f32 / SHIP_RING_SIDES as f32) * std::f32::consts::TAU;
            [radius * theta.cos(), y, radius * theta.sin()]
        })
        .collect()
}

/// The vessel's wireframe silhouette, ORIGIN-CENTERED (upper ring at local
/// `y = 0` — the entity `Transform` places it on the orbit; before
/// 2026-07-13 the world position was baked into the vertices at a fixed
/// overhead `SHIP_Y`, which pinned it over the world center): two
/// concentric octagon rings ([`SHIP_RADIUS`] at `0`,
/// [`SHIP_LOWER_RADIUS_FRAC`] × radius at `-`[`SHIP_LOWER_DROP`]), one
/// vertical at each of the upper ring's vertices connecting down to the
/// matching lower vertex, and a short nose spine off the ring's local-−Z
/// point — the vessel's own N window bearing, which the orbit system keeps
/// yawed at the world center. Segment count is always
/// `3 × SHIP_RING_SIDES + 1` (upper ring edges + lower ring edges +
/// verticals + one nose) — pure, no Bevy types, so [`super::scene`] just
/// flattens the result via [`flatten_segments`].
pub fn ship_silhouette_segments() -> Vec<Segment> {
    let upper = ring_points(SHIP_RADIUS, 0.0);
    let lower = ring_points(SHIP_RADIUS * SHIP_LOWER_RADIUS_FRAC, -SHIP_LOWER_DROP);
    let n = SHIP_RING_SIDES;
    let mut segs = Vec::with_capacity(3 * n + 1);
    for i in 0..n {
        segs.push([upper[i], upper[(i + 1) % n]]);
    }
    for i in 0..n {
        segs.push([lower[i], lower[(i + 1) % n]]);
    }
    for i in 0..n {
        segs.push([upper[i], lower[i]]);
    }
    let nose_base = [0.0, 0.0, -SHIP_RADIUS];
    let nose_tip = [0.0, 0.0, -(SHIP_RADIUS + SHIP_NOSE_LENGTH)];
    segs.push([nose_base, nose_tip]);
    segs
}

// ── The vessel's orbit (A4; shared, 2026-07-13) ──────────────────────────────
//
// ONE orbit, two riders: the backdrop RTT camera (the view out the room's N
// portal) and the dived world's visible vessel silhouette both take their
// pose from `orbit_pose` — the octagon room circles the fsn space with its
// window always facing the world, and what you'd see from the ground IS the
// vantage the window renders from, by construction.

/// The vessel's orbit radius/height (world units) and angular rate
/// (rad/s) — a slow drift so the portal reads as "something out there is
/// churning," not a spinning toy. Height dropped 1300 → 900 with the single
/// panel-spanning portal (2026-07-13): the elevation angle falls from ~34°
/// to ~25°, trading the map-from-above read for a horizon — the world seen
/// out a window, which is now the primary FSN surface (diving is
/// de-emphasized; the world rotates past the glass instead). Retuned with
/// [`ROOT_WORLD_SIZE`]'s 4× horizon bump (2026-07-13 eve) — and this time
/// deliberately NOT in proportion (2850/1350 → 7200/1900, ~2.5×/1.4×): the
/// camera sits 1.2× the world's half-extent out at a shallow ~15°
/// elevation, so the nearest districts pass beneath the frame's bottom
/// edge, the far edge (~13000 out) settles just under the skyline with sky
/// above it, and the world reads as terrain to the horizon instead of a
/// model on a table. **Amy-tunable.**
pub const ORBIT_RADIUS: f32 = 7200.0;
pub const ORBIT_HEIGHT: f32 = 1900.0;

/// The horizon's gear ratio against the room's master spin — the time
/// well's TOP terrace ring (`time_well::scene::TERRACE_RING_SPIN_BASE`).
/// Amy, 2026-07-13: the well's rings and the world past the glass read
/// gear-like from the console, so they should BE geared — the FSN orbit is
/// the big outer wheel of the ensemble. Started at 1/3 (her "maybe 2/3 or
/// 1/3"), dropped to 1/5 the same evening when the well's base rose 0.10 →
/// 0.13 (Amy: "well rings could be a touch faster and the fsn a bit
/// slower" — the gearing means a faster base speeds the horizon too, so
/// the ratio absorbs both moves: net horizon 0.033 → 0.026 rad/s, ~4 min
/// per revolution). Retuning the well's spin turns the whole train
/// together; retune the RATIO here to change only the horizon.
/// **Amy-tunable.**
pub const ORBIT_GEAR_RATIO: f32 = 1.0 / 5.0;

/// Angular rate (rad/s) of the vessel's orbit — derived, never set
/// directly: the master spin × [`ORBIT_GEAR_RATIO`].
pub const ORBIT_RATE: f32 =
    crate::view::time_well::scene::TERRACE_RING_SPIN_BASE * ORBIT_GEAR_RATIO;

/// The backdrop camera's pose at time `t` (seconds): a slow circular orbit
/// at [`ORBIT_RADIUS`]/[`ORBIT_HEIGHT`] around the world origin, always
/// looking back at the origin. Returns `(position, look_at)` as plain arrays
/// — this module's own no-Bevy-types stance — `super::backdrop` turns the
/// pair into a `Transform::from_translation(..).looking_at(..)`.
pub fn orbit_pose(t: f32) -> ([f32; 3], [f32; 3]) {
    let theta = t * ORBIT_RATE;
    ([ORBIT_RADIUS * theta.cos(), ORBIT_HEIGHT, ORBIT_RADIUS * theta.sin()], [0.0, 0.0, 0.0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_viz::fsn::layout_field;

    // ── join_path / split_parent ──

    #[test]
    fn join_path_avoids_double_slash_at_root() {
        assert_eq!(join_path("/", "foo"), "/foo");
        assert_eq!(join_path("/foo", "bar"), "/foo/bar");
    }

    #[test]
    fn split_parent_handles_root_children_and_nesting() {
        assert_eq!(split_parent("/foo"), Some(("/", "foo")));
        assert_eq!(split_parent("/foo/bar"), Some(("/foo", "bar")));
        assert_eq!(split_parent("/a/b/c"), Some(("/a/b", "c")));
    }

    #[test]
    fn split_parent_of_the_root_is_none() {
        assert_eq!(split_parent("/"), None);
        assert_eq!(split_parent(""), None);
    }

    #[test]
    fn split_parent_tolerates_a_trailing_slash() {
        assert_eq!(split_parent("/foo/"), Some(("/", "foo")));
        assert_eq!(split_parent("/foo/bar/"), Some(("/foo", "bar")));
    }

    #[test]
    fn split_parent_round_trips_with_join_path() {
        for path in ["/etc", "/etc/rc", "/a/b/c/d"] {
            let (parent, name) = split_parent(path).unwrap();
            assert_eq!(join_path(parent, name), path, "{path} must round-trip");
        }
    }

    // ── root_world_rect ──

    #[test]
    fn root_world_rect_is_centered_with_the_configured_side_length() {
        let r = root_world_rect();
        assert_eq!(r.width(), ROOT_WORLD_SIZE as f64);
        assert_eq!(r.height(), ROOT_WORLD_SIZE as f64);
        assert_eq!(r.x0, -(ROOT_WORLD_SIZE as f64) / 2.0);
        assert_eq!(r.x1, (ROOT_WORLD_SIZE as f64) / 2.0);
    }

    // ── cell_bbox_inset ──

    #[test]
    fn cell_bbox_inset_shrinks_a_square_by_the_fraction_on_each_side() {
        let square = vec![
            Vec2::new(0.0, 0.0),
            Vec2::new(100.0, 0.0),
            Vec2::new(100.0, 100.0),
            Vec2::new(0.0, 100.0),
        ];
        let rect = cell_bbox_inset(&square, 0.1).unwrap();
        assert_eq!(rect, Rect { x0: 10.0, y0: 10.0, x1: 90.0, y1: 90.0 });
    }

    #[test]
    fn cell_bbox_inset_lies_inside_the_parent_rect_for_every_laid_out_cell() {
        // The real production shape: lay out a world-scale field, then every
        // cell's inset bbox must sit inside the parent rect (a cell polygon
        // is clipped to the rect, its bbox can't exceed it, and the inset
        // only shrinks).
        let parent = root_world_rect();
        let children: Vec<ChildSpec> = (0..12)
            .map(|i| ChildSpec { name: format!("d{i}"), kind: NodeKind::Dir, height: 30.0 })
            .collect();
        let field = layout_field(parent, &children);
        let mut some_hosted = false;
        for cell in &field.cells {
            let Some(bbox) = cell_bbox_inset(&cell.polygon, SUBFIELD_INSET_FRAC) else {
                continue;
            };
            some_hosted = true;
            assert!(parent.x0 <= bbox.x0 && bbox.x1 <= parent.x1, "{}: {bbox:?}", cell.name);
            assert!(parent.y0 <= bbox.y0 && bbox.y1 <= parent.y1, "{}: {bbox:?}", cell.name);
        }
        assert!(some_hosted, "world-scale cells must be big enough to host subfields");
    }

    #[test]
    fn cell_bbox_inset_guarantees_layout_field_safe_extents() {
        // The contract layout_field's panic-on-degenerate-rect leans on: a
        // Some() bbox always has BOTH extents >= SUBFIELD_MIN_EXTENT.
        let parent = root_world_rect();
        let children: Vec<ChildSpec> = (0..30)
            .map(|i| ChildSpec { name: format!("n{i}"), kind: NodeKind::File, height: 5.0 })
            .collect();
        let field = layout_field(parent, &children);
        for cell in &field.cells {
            if let Some(bbox) = cell_bbox_inset(&cell.polygon, SUBFIELD_INSET_FRAC) {
                assert!(bbox.width() >= SUBFIELD_MIN_EXTENT, "{}: {bbox:?}", cell.name);
                assert!(bbox.height() >= SUBFIELD_MIN_EXTENT, "{}: {bbox:?}", cell.name);
                // And layout_field must actually accept it without panicking.
                let sub = layout_field(
                    bbox,
                    &[ChildSpec { name: "probe".into(), kind: NodeKind::File, height: 1.0 }],
                );
                assert_eq!(sub.cells.len(), 1);
            }
        }
    }

    #[test]
    fn cell_bbox_inset_rejects_degenerate_polygons() {
        // Too few vertices.
        assert_eq!(cell_bbox_inset(&[], SUBFIELD_INSET_FRAC), None);
        assert_eq!(cell_bbox_inset(&[Vec2::new(0.0, 0.0)], SUBFIELD_INSET_FRAC), None);
        assert_eq!(
            cell_bbox_inset(&[Vec2::new(0.0, 0.0), Vec2::new(1.0, 1.0)], SUBFIELD_INSET_FRAC),
            None
        );
        // Too small to host a field after the inset (extent < SUBFIELD_MIN_EXTENT).
        let tiny = vec![Vec2::new(0.0, 0.0), Vec2::new(1.0, 0.0), Vec2::new(0.5, 1.0)];
        assert_eq!(cell_bbox_inset(&tiny, SUBFIELD_INSET_FRAC), None);
        // Non-finite coordinates.
        let nan = vec![
            Vec2::new(0.0, 0.0),
            Vec2::new(f64::NAN, 0.0),
            Vec2::new(100.0, 100.0),
            Vec2::new(0.0, 100.0),
        ];
        assert_eq!(cell_bbox_inset(&nan, SUBFIELD_INSET_FRAC), None);
    }

    #[test]
    fn cell_bbox_inset_is_deterministic() {
        let poly = vec![
            Vec2::new(3.0, -7.0),
            Vec2::new(90.0, 4.0),
            Vec2::new(70.0, 88.0),
            Vec2::new(-10.0, 60.0),
        ];
        assert_eq!(
            cell_bbox_inset(&poly, SUBFIELD_INSET_FRAC),
            cell_bbox_inset(&poly, SUBFIELD_INSET_FRAC)
        );
    }

    #[test]
    fn colliding_name_hashes_no_longer_stack_sibling_subfields() {
        // The bug this placement replaced: placeholder_quadrant is
        // hash(name) % 4, so two sibling directories hashing to the same
        // quadrant used to get the IDENTICAL world rect. Voronoi cells are
        // disjoint by construction, so any two cells' inset bboxes must
        // never coincide — whatever their names hash to.
        let parent = root_world_rect();
        let children: Vec<ChildSpec> = (0..10)
            .map(|i| ChildSpec { name: format!("dir{i}"), kind: NodeKind::Dir, height: 30.0 })
            .collect();
        let field = layout_field(parent, &children);
        let bboxes: Vec<Rect> = field
            .cells
            .iter()
            .filter_map(|c| cell_bbox_inset(&c.polygon, SUBFIELD_INSET_FRAC))
            .collect();
        assert!(bboxes.len() >= 2, "need at least two hosted cells to compare");
        for i in 0..bboxes.len() {
            for j in (i + 1)..bboxes.len() {
                assert_ne!(bboxes[i], bboxes[j], "sibling subfield rects must never coincide");
            }
        }
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

    // ── recency_weight ──

    #[test]
    fn recency_weight_is_one_at_and_under_fresh() {
        assert_eq!(recency_weight(0.0), 1.0);
        assert_eq!(recency_weight(RECENCY_FRESH_SECS), 1.0);
        assert_eq!(recency_weight(RECENCY_FRESH_SECS * 0.5), 1.0);
    }

    #[test]
    fn recency_weight_is_zero_at_and_over_old() {
        assert_eq!(recency_weight(RECENCY_OLD_SECS), 0.0);
        assert_eq!(recency_weight(RECENCY_OLD_SECS * 10.0), 0.0);
    }

    #[test]
    fn recency_weight_ramps_monotonically_between_fresh_and_old() {
        let samples: Vec<f32> = [0.25, 0.5, 0.75]
            .iter()
            .map(|frac| {
                let age = RECENCY_FRESH_SECS + frac * (RECENCY_OLD_SECS - RECENCY_FRESH_SECS);
                recency_weight(age)
            })
            .collect();
        assert!(samples[0] > samples[1] && samples[1] > samples[2], "{samples:?} must decrease");
        for w in samples {
            assert!((0.0..=1.0).contains(&w), "{w} out of range");
        }
    }

    #[test]
    fn recency_weight_treats_a_future_mtime_as_fresh() {
        assert_eq!(recency_weight(-100.0), 1.0, "clock skew must not go negative/NaN");
    }

    #[test]
    fn recency_weight_treats_nan_as_fresh_and_infinity_as_infinitely_old() {
        assert_eq!(recency_weight(f64::NAN), 1.0, "NaN must not propagate into the tint math");
        assert_eq!(recency_weight(f64::INFINITY), 0.0, "an infinite age reads as maximally stale");
    }

    // ── per_cell_colors ──

    #[test]
    fn per_cell_colors_expands_each_cell_to_its_own_vertex_count() {
        let counts = [3usize, 0, 2];
        let colors = [[1.0, 0.0, 0.0, 1.0], [0.0, 1.0, 0.0, 1.0], [0.0, 0.0, 1.0, 1.0]];
        let out = per_cell_colors(&counts, &colors);
        assert_eq!(out.len(), 5);
        assert_eq!(&out[0..3], &[colors[0]; 3]);
        assert_eq!(&out[3..5], &[colors[2]; 2]);
    }

    #[test]
    fn per_cell_colors_of_empty_input_is_empty() {
        assert!(per_cell_colors(&[], &[]).is_empty());
    }

    // ── ship_silhouette_segments ──

    #[test]
    fn ship_silhouette_segment_count_matches_the_3x_plus_nose_invariant() {
        let segs = ship_silhouette_segments();
        assert_eq!(segs.len(), 3 * 8 + 1);
    }

    #[test]
    fn ship_silhouette_is_origin_centered_with_rings_at_local_y_levels() {
        // Origin-centered since 2026-07-13 (the vessel rides the orbit via
        // its entity Transform; a baked-in world Y would double-place it).
        let segs = ship_silhouette_segments();
        // First 8: upper ring, both endpoints at local y = 0.
        for s in &segs[0..8] {
            assert_eq!(s[0][1], 0.0);
            assert_eq!(s[1][1], 0.0);
        }
        // Next 8: lower ring, both endpoints at -SHIP_LOWER_DROP.
        for s in &segs[8..16] {
            assert_eq!(s[0][1], -SHIP_LOWER_DROP);
            assert_eq!(s[1][1], -SHIP_LOWER_DROP);
        }
        // Next 8: verticals, one endpoint at each level.
        for s in &segs[16..24] {
            let ys = [s[0][1], s[1][1]];
            assert!(ys.contains(&0.0) && ys.contains(&(-SHIP_LOWER_DROP)));
        }
        // Final: the nose spine, both ends at local y = 0, projecting past
        // -radius (local -Z = the vessel's own N window bearing).
        let nose = segs[24];
        assert_eq!(nose[0][1], 0.0);
        assert_eq!(nose[1][1], 0.0);
        assert_eq!(nose[0][2], -SHIP_RADIUS);
        assert_eq!(nose[1][2], -(SHIP_RADIUS + SHIP_NOSE_LENGTH));
    }

    // ── orbit_pose ──

    #[test]
    fn orbit_pose_holds_the_configured_radius_and_height_over_time() {
        for t in [0.0, 3.7, 12.0, 100.0] {
            let (pos, look_at) = orbit_pose(t);
            let xz_dist = (pos[0] * pos[0] + pos[2] * pos[2]).sqrt();
            assert!((xz_dist - ORBIT_RADIUS).abs() < 1e-2, "t={t}: {xz_dist} vs {ORBIT_RADIUS}");
            assert_eq!(pos[1], ORBIT_HEIGHT);
            assert_eq!(look_at, [0.0, 0.0, 0.0]);
        }
    }

    #[test]
    fn orbit_pose_advances_with_time() {
        let (p0, _) = orbit_pose(0.0);
        let (p1, _) = orbit_pose(10.0);
        assert_ne!(p0, p1, "the orbit must actually move");
    }
}
