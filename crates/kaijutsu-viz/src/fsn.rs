//! FSN landscape: cell addressing + relaxed-Voronoi terrain layout (slice 0).
//!
//! Pure math for the VFS-as-terrain scene (`docs/scenes/vfs.md`, "fsn"). This
//! module renders nothing — it hands a Bevy-side renderer (Lane C) a
//! [`FsnField`] of cell polygons in unit-face `[0, 1]²` space, plus a
//! [`CellId`] address scheme that lets a path be resolved to a cell without
//! walking the whole tree. Three layers, in dependency order:
//!
//! 1. **[`CellId`]** — an S2-style quadtree address: face + quadrant path.
//!    Slice 0 renders one flat face, but the id is cube-sphere-ready (claim 2
//!    of the design doc: *path = cell-ID prefix*).
//! 2. **[`seed_point`]** — a deterministic per-child point inside a cell's
//!    rect, hashed from the child's name (FNV-1a 64, no `std` hasher — see
//!    "Determinism" below).
//! 3. **[`layout_field`]** — the basalt: a Voronoi diagram of sibling seed
//!    points, clipped to the parent rect, relaxed by a **fixed** number of
//!    Lloyd iterations. Fixed-k relaxation is the whole point: it bounds how
//!    far one new sibling's arrival can perturb its neighbors' shapes (the
//!    design doc's "blast radius" promise) — iterating to convergence instead
//!    would let one insertion ripple across the whole field.
//!
//! # Voronoi dependency — bake-off
//!
//! Two candidates, per project convention (bring ≥2 concrete options to a
//! design decision):
//!
//! - **`spade`** — already resolves in `Cargo.lock` (transitively, via
//!   `parry2d`/`bevy_rapier`), robust exact-arithmetic Delaunay
//!   triangulation. But it only gives the triangulation: turning that into
//!   *clipped-to-rect Voronoi cell polygons* means extracting the dual graph
//!   from circumcenters, walking each site's incident triangles in order,
//!   and clipping the result against the rect (Sutherland-Hodgman or
//!   similar) — real engineering surface this slice doesn't need to own.
//! - **`voronator`** (chosen) — a delaunator port whose `VoronoiDiagram`
//!   does exactly what this module needs out of the box:
//!   `VoronoiDiagram::from_tuple(min, max, points)` triangulates, adds four
//!   far-away helper points at the bounding box's extremes (so the hull
//!   sites always have well-formed cells), computes circumcenters, and
//!   Sutherland-Hodgman-clips every cell polygon to the rect. `.cells()`
//!   returns the clipped polygons directly (helper cells stripped); `.neighbors`
//!   gives the Voronoi adjacency graph the blast-radius test walks. Depends
//!   on `delaunator`'s robust-ish (not exact) predicates — acceptable for a
//!   terrain layout, not a CAD kernel.
//!
//! `spade` not already being in the lock file for *this crate* didn't decide
//! it either way; what decided it is that `voronator` hands back the exact
//! shape (clipped polygons + adjacency) this module needs, and `spade` would
//! require hand-rolling that machinery for no accuracy benefit at this
//! scale. Added to `kaijutsu-viz/Cargo.toml` with `default-features = false`
//! (the default `rayon` feature would let cell computation run in parallel,
//! which is fine for throughput but risks floating-point summation order
//! varying run-to-run — determinism over speed for slice 0).
//!
//! # Determinism
//!
//! Every guarantee in this module composes into one property: **the field is
//! a pure function of the (rect, sorted-by-name children) pair.** No
//! `std::collections::HashMap` iteration, no `DefaultHasher` (unstable across
//! Rust releases — this module hand-rolls FNV-1a instead, matching the idiom
//! `kaijutsu-app`'s `accent_color`/`ray_angle` already use for the same
//! reason), no thread parallelism (`voronator`'s `rayon` feature disabled).
//! [`layout_field`] always processes children in name-sorted order, so two
//! callers with the same children in different array order get
//! byte-identical output.
//!
//! Scope that claim honestly: the guarantee is **per compiled binary** —
//! every instance of the same binary produces byte-identical fields — not
//! "every client, on every platform, forever." `voronator`/`delaunator` use
//! plain-`f64` orientation predicates (no Shewchuk-style adaptive exact
//! arithmetic), so compiler FMA contraction or a different
//! platform/compiler can flip a near-degenerate orientation test and yield
//! a combinatorially different (still valid) diagram. If cross-binary
//! bit-stability ever becomes load-bearing (e.g. clients independently
//! recomputing the same field and diffing it), that's a predicate-hardening
//! pass, not a tweak.
//!
//! # Fail-loud stance
//!
//! [`CellId::root`] and [`CellId::child`] return `Err(CellIdError)` rather
//! than panicking on an invalid face or max-depth overflow: a VFS walker
//! that hits `CELL_MAX_DEPTH` on a deeply-nested tree is a normal boundary
//! condition a caller should get to handle (e.g. fall back to flat
//! clustering beyond the max), not a crash. This is still "fail loud, not
//! silent" — `Result` forces the caller to look at it, unlike a wrapping or
//! clamping fallback would.
//!
//! [`layout_field`], by contrast, **panics** on a degenerate rect or a
//! Voronoi construction failure: those are always upstream programming
//! errors in a pure function (bad rect math, NaN seeds), never runtime
//! conditions a renderer should handle per-frame — see the comments at the
//! panic sites.

use std::fmt;

// ─── Vec2 / Rect ────────────────────────────────────────────────────────────

/// A 2D point/vector. `f64` to match this crate's existing float convention
/// (`scales::ScaleLinear` et al. are all `f64`) and `voronator`'s native
/// `delaunator::Point`; the app converts to `f32`/`glam` at the render
/// boundary, same as it already does for scale outputs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vec2 {
    pub x: f64,
    pub y: f64,
}

impl Vec2 {
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

impl From<(f64, f64)> for Vec2 {
    fn from((x, y): (f64, f64)) -> Self {
        Self { x, y }
    }
}

impl From<Vec2> for (f64, f64) {
    fn from(v: Vec2) -> Self {
        (v.x, v.y)
    }
}

/// An axis-aligned rectangle, `[x0, x1] × [y0, y1]`. Used both for a
/// [`CellId`]'s `quad_rect` (unit-face `[0, 1]²` space) and for
/// [`layout_field`]'s clip region (whatever space the caller places the
/// parent cell in).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl Rect {
    pub fn unit() -> Self {
        Self { x0: 0.0, y0: 0.0, x1: 1.0, y1: 1.0 }
    }

    pub fn width(&self) -> f64 {
        self.x1 - self.x0
    }

    pub fn height(&self) -> f64 {
        self.y1 - self.y0
    }

    pub fn area(&self) -> f64 {
        self.width() * self.height()
    }

    /// The four corners, CCW starting bottom-left: `(x0,y0) → (x1,y0) →
    /// (x1,y1) → (x0,y1)`. Not repeated (open), matching `voronator`'s cell
    /// polygon representation — see [`FsnCell::polygon`].
    pub fn corners_ccw(&self) -> Vec<Vec2> {
        vec![
            Vec2::new(self.x0, self.y0),
            Vec2::new(self.x1, self.y0),
            Vec2::new(self.x1, self.y1),
            Vec2::new(self.x0, self.y1),
        ]
    }

    /// Whether `p` lies within the rect, inflated by `eps` on every side —
    /// the clipping-guarantee test's tolerance for float slop.
    pub fn contains_point(&self, p: Vec2, eps: f64) -> bool {
        p.x >= self.x0 - eps && p.x <= self.x1 + eps && p.y >= self.y0 - eps && p.y <= self.y1 + eps
    }
}

// ─── FNV-1a 64 ──────────────────────────────────────────────────────────────

/// FNV-1a, 64-bit. Hand-rolled (not `std::hash::DefaultHasher`, which is
/// explicitly *not* stable across Rust releases — this module's whole
/// contract is "same input, same output, forever, on every client"). Same
/// recipe family as `kaijutsu-app`'s `accent_color`/`ray_angle` (those use
/// the 32-bit variant; this module uses 64-bit so [`seed_point`] can split
/// one hash into two independent-enough halves for x and y without hashing
/// twice).
fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET_BASIS;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(PRIME);
    }
    h
}

// ─── seed_point ─────────────────────────────────────────────────────────────

/// Inset a seed into the middle 90% of its rect — the design doc's slack:
/// "sized with slack so growth lands in gaps." A child's seed is never
/// planted flush against a cell edge, leaving room for a sibling to bloom in
/// nearby without immediately butting the boundary.
const SEED_INSET: f64 = 0.05;

/// A deterministic point for `name` inside `rect`, hashed from the name
/// alone (not position in any list, not sibling count) — the "pure function
/// of the namespace" rule. Splits one [`fnv1a64`] hash into low/high 32-bit
/// halves for the x/y fractions; inset into the middle `1 - 2*SEED_INSET`
/// (90%) of the rect per [`SEED_INSET`].
///
/// Two different names can hash to nearly-identical points (a near-collision
/// in the low bits) — `layout_field`'s Voronoi step must not panic on that;
/// see the `near_collision_seeds_do_not_panic` test.
pub fn seed_point(rect: Rect, name: &str) -> Vec2 {
    let h = fnv1a64(name.as_bytes());
    let hx = (h & 0xFFFF_FFFF) as u32;
    let hy = (h >> 32) as u32;
    let tx = hx as f64 / u32::MAX as f64;
    let ty = hy as f64 / u32::MAX as f64;
    let span = 1.0 - 2.0 * SEED_INSET;
    let tx = SEED_INSET + tx * span;
    let ty = SEED_INSET + ty * span;
    Vec2::new(rect.x0 + tx * rect.width(), rect.y0 + ty * rect.height())
}

// ─── CellId ─────────────────────────────────────────────────────────────────

/// Number of cube faces a [`CellId`] can address (`0..NUM_FACES`). Slice 0
/// only ever uses face 0 (a single flat face); the field exists so the
/// encoding is cube-sphere-ready without a breaking change later.
pub const NUM_FACES: u8 = 6;

/// Max quadtree depth a [`CellId`] can reach. Fixed by the bit budget below
/// (3 face bits + 5 level bits + 2 bits/level × 28 levels = 64), not an
/// arbitrary round number — S2 itself uses ~30; 28 is what a clean 64-bit
/// pack yields with an explicit level-count field (see the module doc's
/// encoding note below). No VFS tree is anywhere near 28 directories deep in
/// practice.
pub const CELL_MAX_DEPTH: u8 = 28;

const FACE_BITS: u32 = 3;
const LEVEL_BITS: u32 = 5;
const PATH_BITS: u32 = 2 * CELL_MAX_DEPTH as u32; // 56
const FACE_SHIFT: u32 = 64 - FACE_BITS; // 61
const LEVEL_SHIFT: u32 = FACE_SHIFT - LEVEL_BITS; // 56
const PATH_MASK: u64 = (1u64 << PATH_BITS) - 1;
const LEVEL_MASK: u64 = (1u64 << LEVEL_BITS) - 1;
const FACE_MASK: u64 = (1u64 << FACE_BITS) - 1;

/// An S2-style quadtree cell address: `face` (3 bits) + `level` (5 bits,
/// explicit count) + `path` (2 bits per level, MSB-first, zero-padded on the
/// right below `level`).
///
/// # Encoding choice
///
/// S2 itself packs face + path into a single field with a trailing sentinel
/// `1` bit marking "end of path," so the level is *implicit* (found from the
/// position of the lowest set bit) — that trick is what lets S2 cell ids
/// sort along a contiguous space-filling curve, which this module has no
/// need for yet (slice 0 does no range queries over ids). This encoding uses
/// an **explicit level field** instead: three fixed regions (face, level,
/// path), no bit-scanning to find the level, easier to get right and to
/// verify in tests. The tradeoff is one wasted bit's worth of packing
/// efficiency (28 levels instead of a theoretical 30) and ids that don't
/// sort as a space-filling curve — acceptable for a slice-0 module with no
/// consumer that needs curve locality yet. [`CellId::contains`] (prefix
/// containment) works identically either way: mask both ids down to the
/// ancestor's `level * 2` path bits and compare.
///
/// # Quadrant numbering
///
/// A quadrant is a 2-bit value: bit 0 selects the x-half (`0` = left, `1` =
/// right), bit 1 selects the y-half (`0` = bottom, `1` = top). So `0` =
/// (left, bottom), `1` = (right, bottom), `2` = (left, top), `3` = (right,
/// top). [`CellId::quad_rect`] and [`CellId::child`] agree on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CellId(u64);

/// Error type for [`CellId`] construction/navigation. A pure math crate, so
/// this implements `Error` by hand rather than pulling in `thiserror` for
/// four variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellIdError {
    /// `face >= NUM_FACES`.
    FaceOutOfRange(u8),
    /// `quadrant > 3` (only 2 bits are valid).
    QuadrantOutOfRange(u8),
    /// [`CellId::child`] called at [`CELL_MAX_DEPTH`].
    MaxDepthExceeded,
    /// [`CellId::parent`] called on a root (`level() == 0`).
    RootHasNoParent,
}

impl fmt::Display for CellIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CellIdError::FaceOutOfRange(face) => {
                write!(f, "face {face} is out of range (0..{NUM_FACES})")
            }
            CellIdError::QuadrantOutOfRange(q) => {
                write!(f, "quadrant {q} is out of range (0..=3)")
            }
            CellIdError::MaxDepthExceeded => {
                write!(f, "cell is already at CELL_MAX_DEPTH ({CELL_MAX_DEPTH})")
            }
            CellIdError::RootHasNoParent => write!(f, "a root cell (level 0) has no parent"),
        }
    }
}

impl std::error::Error for CellIdError {}

impl CellId {
    /// The root cell of `face` (level 0, empty path).
    pub fn root(face: u8) -> Result<Self, CellIdError> {
        if face >= NUM_FACES {
            return Err(CellIdError::FaceOutOfRange(face));
        }
        Ok(CellId((face as u64) << FACE_SHIFT))
    }

    fn from_parts(face: u8, level: u8, path: u64) -> Self {
        CellId(((face as u64) << FACE_SHIFT) | ((level as u64) << LEVEL_SHIFT) | (path & PATH_MASK))
    }

    /// The cube face this cell lives on (`0..NUM_FACES`).
    pub fn face(&self) -> u8 {
        ((self.0 >> FACE_SHIFT) & FACE_MASK) as u8
    }

    /// Quadtree depth (root = 0).
    pub fn level(&self) -> u8 {
        ((self.0 >> LEVEL_SHIFT) & LEVEL_MASK) as u8
    }

    fn path(&self) -> u64 {
        self.0 & PATH_MASK
    }

    /// Descend into `quadrant` (`0..=3`, see the type doc's numbering).
    /// `Err(MaxDepthExceeded)` at [`CELL_MAX_DEPTH`]; `Err(QuadrantOutOfRange)`
    /// for `quadrant > 3`.
    pub fn child(&self, quadrant: u8) -> Result<Self, CellIdError> {
        if quadrant > 3 {
            return Err(CellIdError::QuadrantOutOfRange(quadrant));
        }
        let level = self.level();
        if level >= CELL_MAX_DEPTH {
            return Err(CellIdError::MaxDepthExceeded);
        }
        let new_level = level + 1;
        let shift = PATH_BITS - 2 * new_level as u32;
        let new_path = self.path() | ((quadrant as u64) << shift);
        Ok(CellId::from_parts(self.face(), new_level, new_path))
    }

    /// The parent cell. `Err(RootHasNoParent)` at level 0.
    pub fn parent(&self) -> Result<Self, CellIdError> {
        let level = self.level();
        if level == 0 {
            return Err(CellIdError::RootHasNoParent);
        }
        let new_level = level - 1;
        let shift = PATH_BITS - 2 * level as u32;
        let clear_mask = 0b11u64 << shift;
        let new_path = self.path() & !clear_mask;
        Ok(CellId::from_parts(self.face(), new_level, new_path))
    }

    /// Exact prefix containment: `self` contains `other` iff they share a
    /// face, `self.level() <= other.level()`, and `other`'s path agrees with
    /// `self`'s on `self`'s levels. Pure bit math — mask both paths down to
    /// `self.level() * 2` bits and compare. Every cell contains itself; a
    /// root contains every cell on its face; cross-face never contains.
    pub fn contains(&self, other: &CellId) -> bool {
        if self.face() != other.face() {
            return false;
        }
        if self.level() > other.level() {
            return false;
        }
        let bits = 2 * self.level() as u32;
        if bits == 0 {
            return true; // root: face match already established above
        }
        let mask = PATH_MASK & !((1u64 << (PATH_BITS - bits)) - 1);
        (self.path() & mask) == (other.path() & mask)
    }

    /// This cell's square in the face's unit space (`[0, 1]²`) — what the
    /// Voronoi field is clipped to and what the app places in world space.
    pub fn quad_rect(&self) -> Rect {
        let mut rect = Rect::unit();
        let path = self.path();
        for i in 1..=self.level() {
            let shift = PATH_BITS - 2 * i as u32;
            let quadrant = ((path >> shift) & 0b11) as u8;
            let mx = (rect.x0 + rect.x1) * 0.5;
            let my = (rect.y0 + rect.y1) * 0.5;
            if quadrant & 0b01 == 0 {
                rect.x1 = mx;
            } else {
                rect.x0 = mx;
            }
            if quadrant & 0b10 == 0 {
                rect.y1 = my;
            } else {
                rect.y0 = my;
            }
        }
        rect
    }
}

/// Walk `components` from `root`, resolving each step's quadrant via
/// `child_quadrant(current_cell, name)`. Propagates the first
/// [`CellIdError`] `CellId::child` raises (e.g. hitting [`CELL_MAX_DEPTH`])
/// rather than masking it — a resolver returning an out-of-range quadrant is
/// a caller bug, not something to silently clamp.
pub fn path_to_cell<I, S>(
    root: CellId,
    components: I,
    mut child_quadrant: impl FnMut(CellId, &str) -> u8,
) -> Result<CellId, CellIdError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut cell = root;
    for comp in components {
        let name = comp.as_ref();
        let q = child_quadrant(cell, name);
        cell = cell.child(q)?;
    }
    Ok(cell)
}

/// **Placeholder** quadrant-assignment policy: `hash(name) % 4`. This is
/// Open Question 2 in `docs/scenes/vfs.md` — "the slack math needs its own
/// design pass" — not a real policy. Hash collisions mean two siblings can
/// land in the *same* quadrant; that's acceptable for slice 0 *only*
/// because subdirs bloom as clusters inside their PARENT's Voronoi field
/// (via [`layout_field`]'s own seed hashing) rather than needing a disjoint
/// sub-cell of their own. Do not build anything load-bearing on quadrant
/// uniqueness until Open Question 2 is resolved.
pub fn placeholder_quadrant(_parent: CellId, name: &str) -> u8 {
    (fnv1a64(name.as_bytes()) % 4) as u8
}

// ─── The basalt: relaxed-Voronoi field ─────────────────────────────────────

/// What a child is, for rendering purposes (platforms vs slabs vs ghost
/// slabs — `docs/scenes/vfs.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeKind {
    File,
    Dir,
    Symlink,
}

/// One child of the directory being laid out. `height` is an **input** —
/// mapping real VFS data (block count, recency, ...) to it is Open Question
/// 1 in `docs/scenes/vfs.md`, deliberately unmapped at this layer.
#[derive(Debug, Clone, PartialEq)]
pub struct ChildSpec {
    pub name: String,
    pub kind: NodeKind,
    pub height: f32,
}

/// One laid-out cell in an [`FsnField`].
#[derive(Debug, Clone, PartialEq)]
pub struct FsnCell {
    pub name: String,
    pub kind: NodeKind,
    /// The seed's final position after Lloyd relaxation.
    pub seed: Vec2,
    /// CCW polygon vertices, **not** repeated (open — first vertex isn't
    /// duplicated at the end), in the same unit-face space as the input
    /// `rect`. Matches `voronator`'s own cell representation and
    /// [`Rect::corners_ccw`].
    pub polygon: Vec<Vec2>,
    pub height: f32,
}

impl FsnCell {
    /// Every polygon edge as a `(from, to)` pair, **including the closing
    /// edge** (last vertex → first vertex). The polygon itself is stored
    /// open (no repeated vertex), which makes "forgot the closing segment"
    /// a one-character bug waiting for any line-list mesh builder — iterate
    /// this instead of zipping `polygon.windows(2)` by hand. Yields exactly
    /// `polygon.len()` edges; empty for a degenerate polygon with fewer
    /// than 2 vertices (a 1-vertex "polygon" has no edge, not a
    /// self-loop).
    pub fn edges(&self) -> impl Iterator<Item = (Vec2, Vec2)> + '_ {
        let n = self.polygon.len();
        (0..if n < 2 { 0 } else { n }).map(move |i| (self.polygon[i], self.polygon[(i + 1) % n]))
    }
}

/// The laid-out field for one directory's children. `cells` is always in
/// name-sorted order — part of the determinism contract, not an
/// implementation accident callers should re-sort away.
///
/// Voronoi adjacency (which cells share an edge) is computed internally by
/// `voronator` during layout and then **discarded** — nothing in slice 0
/// consumes it. TODO: a future renderer that wants shared-edge dedup (draw
/// each basalt seam once, not twice) should export adjacency *here*, from
/// the same diagram that produced the polygons, rather than recomputing it
/// from polygon geometry after the fact.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FsnField {
    pub cells: Vec<FsnCell>,
}

/// Fixed Lloyd-relaxation iteration count. **Fixed, not "until converged"**
/// — this bounds the blast radius: with `k` iterations, only cells within
/// `k` Voronoi-adjacency hops of a newly-inserted (or moved) seed can have
/// their centroid, and therefore their polygon, change. Iterating to
/// convergence would let one new sibling ripple across the whole field,
/// exactly the instability `docs/scenes/vfs.md` calls out ("k = 2–3,
/// fixed").
pub const LLOYD_ITERATIONS: usize = 2;

/// Lay out `children` inside `rect`: seed each from its name
/// ([`seed_point`]), build a Voronoi diagram of the seeds clipped to `rect`,
/// then relax it for [`LLOYD_ITERATIONS`] fixed iterations (each iteration:
/// move every seed to its clipped cell's area centroid, recompute the
/// diagram). Children are always processed in name-sorted order, so the
/// result does not depend on `children`'s input order.
///
/// Degenerate **child counts** never panic: 0 children → an empty field; 1
/// child → the whole rect as its cell (no Voronoi machinery needed — a
/// single site's clipped cell *is* the rect); 2+ children (including
/// collinear seeds) go through the normal path — `voronator` appends four
/// helper points at the rect's extremes before triangulating, which keeps
/// the point set non-degenerate even when every real seed is collinear.
///
/// A degenerate **rect** (zero/negative width or height, or NaN extents),
/// by contrast, panics loudly at entry — see the assert below. An earlier
/// draft fell back to handing every child the full rect when `voronator`
/// returned `None`: overlapping polygons, a disjointness lie the renderer
/// would draw as garbage. Silent fallbacks are a mistake; crashing beats
/// corruption.
///
/// # Separation of concerns: no [`CellId`]s here
///
/// `layout_field` deliberately neither takes nor returns [`CellId`]s: cell
/// *addressing* (where a directory's rect sits on the face — claim 2's
/// path-is-prefix math) and field *layout* (how that directory's children
/// partition the rect) are separate concerns joined only by the rect. A
/// renderer that needs both resolves the address via [`path_to_cell`] +
/// [`CellId::quad_rect`], then feeds the resulting rect here.
pub fn layout_field(rect: Rect, children: &[ChildSpec]) -> FsnField {
    // Fail loud, not fallback: a degenerate rect can only be an upstream
    // programming error in a pure function like this — never a runtime
    // condition the renderer should be handling per-frame.
    assert!(
        rect.width() > 0.0 && rect.height() > 0.0,
        "layout_field requires a rect with strictly positive width and height, got {rect:?}"
    );

    let mut sorted: Vec<&ChildSpec> = children.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));

    match sorted.len() {
        0 => return FsnField::default(),
        1 => {
            let c = sorted[0];
            return FsnField {
                cells: vec![FsnCell {
                    name: c.name.clone(),
                    kind: c.kind,
                    seed: seed_point(rect, &c.name),
                    polygon: rect.corners_ccw(),
                    height: c.height,
                }],
            };
        }
        _ => {}
    }

    let initial_points: Vec<(f64, f64)> = sorted.iter().map(|c| seed_point(rect, &c.name).into()).collect();
    let points = lloyd_relax(rect, &initial_points);

    // Panic over Result: this is a pure function whose failure here is
    // always an upstream programming error (degenerate rect — caught by the
    // entry assert — or NaN seed coordinates), never a runtime condition
    // Lane C should handle per-frame. The rect assert above makes this
    // effectively unreachable; if it fires anyway, something new is wrong
    // and we want the name of the culprit, not a garbage field.
    let diagram = build_voronoi(rect, &points).unwrap_or_else(|| {
        panic!(
            "voronator failed to build a Voronoi diagram of {} relaxed seeds in {rect:?} — \
             likely cause: NaN/non-finite seed coordinates (a degenerate rect is caught at entry)",
            points.len()
        )
    });
    let diagram_cells = diagram.cells();
    debug_assert_eq!(diagram_cells.len(), sorted.len(), "voronator cell count must match site count");
    let cells = sorted
        .iter()
        .zip(points.iter())
        .zip(diagram_cells.iter())
        .map(|((c, p), poly)| FsnCell {
            name: c.name.clone(),
            kind: c.kind,
            seed: Vec2::from(*p),
            polygon: poly.points().iter().map(|pt| Vec2::new(pt.x, pt.y)).collect(),
            height: c.height,
        })
        .collect();

    FsnField { cells }
}

/// Run [`LLOYD_ITERATIONS`] fixed Lloyd-relaxation steps starting from
/// `initial_points` (in the caller's chosen order — the caller, i.e.
/// [`layout_field`], is responsible for that order being name-sorted), each
/// step moving every point to its clipped Voronoi cell's area centroid.
/// Returns only the final positions; `layout_field` doesn't need the
/// intermediate steps, but the blast-radius test does (via
/// [`lloyd_relax_trajectory`]) — kept as a separate, smaller function so the
/// common case stays a plain `Vec` return, not a `Vec<Vec<_>>` nobody but
/// the test wants.
fn lloyd_relax(rect: Rect, initial_points: &[(f64, f64)]) -> Vec<(f64, f64)> {
    lloyd_relax_trajectory(rect, initial_points).pop().expect("trajectory always has at least the initial point set")
}

/// Same relaxation as [`lloyd_relax`], but returns every intermediate point
/// set: index `0` is `initial_points` unchanged, index `t` is the result
/// after `t` Lloyd iterations, index [`LLOYD_ITERATIONS`] is what
/// [`lloyd_relax`] returns. Private: only the blast-radius test (via
/// `use super::*`) needs the intermediate steps, to compare two runs'
/// trajectories exactly rather than reconstructing adjacency after the
/// fact.
fn lloyd_relax_trajectory(rect: Rect, initial_points: &[(f64, f64)]) -> Vec<Vec<(f64, f64)>> {
    let mut trajectory = vec![initial_points.to_vec()];
    for iteration in 0..LLOYD_ITERATIONS {
        let mut points = trajectory.last().expect("trajectory is never empty").clone();
        // Same fail-loud rationale as layout_field's final build: an earlier
        // draft silently `break`-ed here, which handed layout_field
        // *unrelaxed* points and compounded into its (also silent, also
        // deleted) full-rect fallback. A failure here is an upstream
        // programming error (degenerate rect, NaN seeds), not a condition
        // to relax around.
        let diagram = build_voronoi(rect, &points).unwrap_or_else(|| {
            panic!(
                "voronator failed to build a Voronoi diagram of {} seeds in {rect:?} at Lloyd \
                 iteration {iteration} — likely causes: degenerate rect (zero/negative extent) \
                 or NaN/non-finite seed coordinates",
                points.len()
            )
        });
        let cells = diagram.cells();
        debug_assert_eq!(cells.len(), points.len(), "voronator cell count must match site count");
        for (p, cell) in points.iter_mut().zip(cells.iter()) {
            if let Some(centroid) = polygon_centroid(cell.points()) {
                *p = centroid;
            }
        }
        trajectory.push(points);
    }
    trajectory
}

/// Build (or rebuild) the Voronoi diagram of `points`, clipped to `rect`.
/// Private: the test module reaches it via `use super::*` to independently
/// reconstruct adjacency for the blast-radius guarantee.
fn build_voronoi(rect: Rect, points: &[(f64, f64)]) -> Option<voronator::VoronoiDiagram<voronator::delaunator::Point>> {
    voronator::VoronoiDiagram::from_tuple(&(rect.x0, rect.y0), &(rect.x1, rect.y1), points)
}

/// Area centroid of a (possibly clipped) convex polygon via the standard
/// shoelace-weighted formula. Falls back to a plain vertex average for
/// degenerate inputs (fewer than 3 points, or ~zero signed area) — those
/// only arise from extreme clipping at a rect's corner, and a vertex average
/// is a reasonable centroid proxy there.
fn polygon_centroid(points: &[voronator::delaunator::Point]) -> Option<(f64, f64)> {
    if points.is_empty() {
        return None;
    }
    if points.len() < 3 {
        return Some(vertex_average(points));
    }
    let mut area2 = 0.0;
    let mut cx = 0.0;
    let mut cy = 0.0;
    for i in 0..points.len() {
        let p0 = &points[i];
        let p1 = &points[(i + 1) % points.len()];
        let cross = p0.x * p1.y - p1.x * p0.y;
        area2 += cross;
        cx += (p0.x + p1.x) * cross;
        cy += (p0.y + p1.y) * cross;
    }
    if area2.abs() < 1e-12 {
        return Some(vertex_average(points));
    }
    let area6 = area2 * 3.0;
    Some((cx / area6, cy / area6))
}

fn vertex_average(points: &[voronator::delaunator::Point]) -> (f64, f64) {
    let n = points.len() as f64;
    let sx: f64 = points.iter().map(|p| p.x).sum();
    let sy: f64 = points.iter().map(|p| p.y).sum();
    (sx / n, sy / n)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── CellId: root / child / parent ───────────────────────────────────────

    #[test]
    fn root_rejects_out_of_range_face() {
        assert_eq!(CellId::root(NUM_FACES), Err(CellIdError::FaceOutOfRange(NUM_FACES)));
        assert!(CellId::root(NUM_FACES - 1).is_ok());
    }

    #[test]
    fn root_is_level_zero() {
        let r = CellId::root(2).unwrap();
        assert_eq!(r.level(), 0);
        assert_eq!(r.face(), 2);
    }

    #[test]
    fn child_rejects_out_of_range_quadrant() {
        let r = CellId::root(0).unwrap();
        assert_eq!(r.child(4), Err(CellIdError::QuadrantOutOfRange(4)));
        assert!(r.child(3).is_ok());
    }

    #[test]
    fn child_increments_level_and_preserves_face() {
        let r = CellId::root(1).unwrap();
        let c = r.child(2).unwrap();
        assert_eq!(c.level(), 1);
        assert_eq!(c.face(), 1);
        let gc = c.child(0).unwrap();
        assert_eq!(gc.level(), 2);
        assert_eq!(gc.face(), 1);
    }

    #[test]
    fn child_errors_past_max_depth() {
        let mut cell = CellId::root(0).unwrap();
        for _ in 0..CELL_MAX_DEPTH {
            cell = cell.child(0).unwrap();
        }
        assert_eq!(cell.level(), CELL_MAX_DEPTH);
        assert_eq!(cell.child(0), Err(CellIdError::MaxDepthExceeded));
    }

    #[test]
    fn parent_of_root_errs() {
        let r = CellId::root(0).unwrap();
        assert_eq!(r.parent(), Err(CellIdError::RootHasNoParent));
    }

    /// Full-depth round trip: descend to level 28 (the last valid level),
    /// then walk parent() all the way back and land exactly on the root —
    /// the path bits must be bit-perfect at the very bottom of the u64
    /// budget, where an off-by-one shift would corrupt first.
    #[test]
    fn max_depth_descend_and_ascend_round_trips_to_root() {
        let root = CellId::root(0).unwrap();
        let mut cell = root;
        // Vary the quadrant so every 2-bit slot carries a nonzero pattern
        // somewhere (all-zeros would mask a failure to clear bits).
        for i in 0..CELL_MAX_DEPTH {
            cell = cell.child(i % 4).unwrap();
        }
        assert_eq!(cell.level(), CELL_MAX_DEPTH);
        for _ in 0..CELL_MAX_DEPTH {
            cell = cell.parent().unwrap();
        }
        assert_eq!(cell, root, "28 children then 28 parents must land exactly on the root");
    }

    /// Containment at the deepest level: a level-27 parent contains its
    /// level-28 child, and two level-28 cousins sharing that parent do NOT
    /// contain each other — the prefix mask must be exact even when the
    /// diverging bits are the lowest 2 in the path field.
    #[test]
    fn contains_holds_at_the_level_28_boundary() {
        let mut parent = CellId::root(0).unwrap();
        for i in 0..(CELL_MAX_DEPTH - 1) {
            parent = parent.child(i % 4).unwrap();
        }
        assert_eq!(parent.level(), CELL_MAX_DEPTH - 1);

        let a = parent.child(0).unwrap();
        let b = parent.child(3).unwrap();
        assert_eq!(a.level(), CELL_MAX_DEPTH);

        assert!(parent.contains(&a));
        assert!(parent.contains(&b));
        assert!(!a.contains(&parent), "a level-28 cell cannot contain its level-27 parent");
        assert!(!a.contains(&b), "level-28 cousins diverging in the last 2 path bits must not contain each other");
        assert!(!b.contains(&a));
    }

    #[test]
    fn parent_child_round_trip() {
        let r = CellId::root(3).unwrap();
        for q in 0u8..=3 {
            let c = r.child(q).unwrap();
            assert_eq!(c.parent().unwrap(), r, "quadrant {q} round-trips to its parent");
        }
    }

    #[test]
    fn deep_parent_child_round_trip() {
        let mut cell = CellId::root(0).unwrap();
        let path = [1u8, 3, 0, 2, 1, 1, 3];
        for &q in &path {
            cell = cell.child(q).unwrap();
        }
        let mut walked_back = cell;
        for _ in 0..path.len() {
            walked_back = walked_back.parent().unwrap();
        }
        assert_eq!(walked_back, CellId::root(0).unwrap());
    }

    // ── CellId: contains (guarantee 3) ──────────────────────────────────────

    #[test]
    fn contains_self() {
        let r = CellId::root(0).unwrap();
        assert!(r.contains(&r));
        let c = r.child(1).unwrap().child(2).unwrap();
        assert!(c.contains(&c));
    }

    #[test]
    fn root_contains_every_descendant_on_its_face() {
        let r = CellId::root(4).unwrap();
        let a = r.child(0).unwrap().child(3).unwrap().child(1).unwrap();
        let b = r.child(2).unwrap();
        assert!(r.contains(&a));
        assert!(r.contains(&b));
    }

    #[test]
    fn parent_contains_child_but_not_reverse() {
        let r = CellId::root(0).unwrap();
        let c = r.child(1).unwrap();
        let gc = c.child(2).unwrap();
        assert!(c.contains(&gc));
        assert!(!gc.contains(&c), "a deeper cell cannot contain its shallower ancestor");
    }

    #[test]
    fn siblings_do_not_contain_each_other() {
        let r = CellId::root(0).unwrap();
        let a = r.child(0).unwrap();
        let b = r.child(1).unwrap();
        assert!(!a.contains(&b));
        assert!(!b.contains(&a));
    }

    #[test]
    fn cousins_do_not_contain_each_other() {
        // Same parent-level ancestor path length, diverging at the last step.
        let r = CellId::root(0).unwrap();
        let a = r.child(0).unwrap().child(1).unwrap();
        let b = r.child(0).unwrap().child(2).unwrap();
        assert!(!a.contains(&b));
        assert!(!b.contains(&a));
    }

    #[test]
    fn contains_is_a_prefix_relation_on_the_path() {
        // a.contains(b) must agree with "b's ancestor chain includes a".
        let r = CellId::root(5).unwrap();
        let mid = r.child(3).unwrap().child(0).unwrap();
        let deep = mid.child(2).unwrap().child(1).unwrap();
        assert!(r.contains(&mid));
        assert!(r.contains(&deep));
        assert!(mid.contains(&deep));
        // Walking deep's ancestors back up must hit mid, then r.
        let mut walker = deep;
        let mut hit_mid = false;
        let hit_root;
        loop {
            if walker == mid {
                hit_mid = true;
            }
            if walker == r {
                hit_root = true;
                break;
            }
            walker = walker.parent().unwrap();
        }
        assert!(hit_mid && hit_root);
    }

    #[test]
    fn cross_face_never_contains() {
        let a = CellId::root(0).unwrap();
        let b = CellId::root(1).unwrap();
        assert!(!a.contains(&b));
        assert!(!b.contains(&a));
        let ac = a.child(0).unwrap();
        let bc = b.child(0).unwrap();
        assert!(!ac.contains(&bc));
    }

    // ── CellId: quad_rect ────────────────────────────────────────────────────

    #[test]
    fn root_quad_rect_is_unit_square() {
        let r = CellId::root(0).unwrap();
        assert_eq!(r.quad_rect(), Rect::unit());
    }

    #[test]
    fn quad_rect_quarters_are_disjoint_and_cover_the_parent() {
        let r = CellId::root(0).unwrap();
        let rects: Vec<Rect> = (0u8..=3).map(|q| r.child(q).unwrap().quad_rect()).collect();
        let total: f64 = rects.iter().map(|rc| rc.area()).sum();
        assert!((total - r.quad_rect().area()).abs() < 1e-12);
        for rc in &rects {
            assert!((rc.area() - 0.25).abs() < 1e-12);
        }
    }

    #[test]
    fn quad_rect_matches_documented_quadrant_numbering() {
        let r = CellId::root(0).unwrap();
        // 0 = left/bottom
        let q0 = r.child(0).unwrap().quad_rect();
        assert_eq!(q0, Rect { x0: 0.0, y0: 0.0, x1: 0.5, y1: 0.5 });
        // 1 = right/bottom
        let q1 = r.child(1).unwrap().quad_rect();
        assert_eq!(q1, Rect { x0: 0.5, y0: 0.0, x1: 1.0, y1: 0.5 });
        // 2 = left/top
        let q2 = r.child(2).unwrap().quad_rect();
        assert_eq!(q2, Rect { x0: 0.0, y0: 0.5, x1: 0.5, y1: 1.0 });
        // 3 = right/top
        let q3 = r.child(3).unwrap().quad_rect();
        assert_eq!(q3, Rect { x0: 0.5, y0: 0.5, x1: 1.0, y1: 1.0 });
    }

    #[test]
    fn quad_rect_shrinks_geometrically_with_depth() {
        let mut cell = CellId::root(0).unwrap();
        for depth in 1..=8u32 {
            cell = cell.child(3).unwrap();
            let expected = 1.0 / (4f64.powi(depth as i32));
            assert!((cell.quad_rect().area() - expected).abs() < 1e-12);
        }
    }

    // ── path_to_cell / placeholder_quadrant ─────────────────────────────────

    #[test]
    fn path_to_cell_walks_components_in_order() {
        let root = CellId::root(0).unwrap();
        let cell = path_to_cell(root, ["etc", "rc"], placeholder_quadrant).unwrap();
        let expected = root
            .child(placeholder_quadrant(root, "etc"))
            .unwrap()
            .child(placeholder_quadrant(root.child(placeholder_quadrant(root, "etc")).unwrap(), "rc"))
            .unwrap();
        assert_eq!(cell, expected);
        assert_eq!(cell.level(), 2);
    }

    #[test]
    fn path_to_cell_empty_path_is_root() {
        let root = CellId::root(0).unwrap();
        let cell = path_to_cell(root, Vec::<&str>::new(), placeholder_quadrant).unwrap();
        assert_eq!(cell, root);
    }

    #[test]
    fn path_to_cell_propagates_child_errors() {
        let mut deep = CellId::root(0).unwrap();
        for _ in 0..CELL_MAX_DEPTH {
            deep = deep.child(0).unwrap();
        }
        let err = path_to_cell(deep, ["one_too_many"], placeholder_quadrant);
        assert_eq!(err, Err(CellIdError::MaxDepthExceeded));
    }

    #[test]
    fn placeholder_quadrant_is_deterministic_and_in_range() {
        let root = CellId::root(0).unwrap();
        let a = placeholder_quadrant(root, "signoff.md");
        let b = placeholder_quadrant(root, "signoff.md");
        assert_eq!(a, b);
        assert!(a <= 3);
    }

    // ── seed_point ───────────────────────────────────────────────────────────

    #[test]
    fn seed_point_is_deterministic() {
        let rect = Rect { x0: 10.0, y0: -5.0, x1: 40.0, y1: 25.0 };
        assert_eq!(seed_point(rect, "foo.txt"), seed_point(rect, "foo.txt"));
    }

    #[test]
    fn seed_point_stays_within_the_middle_90_percent() {
        let rect = Rect { x0: 0.0, y0: 0.0, x1: 100.0, y1: 100.0 };
        for name in ["a", "bb", "ccc", "docs", "src", "Cargo.toml", ".gitignore", "target"] {
            let p = seed_point(rect, name);
            assert!(p.x >= 5.0 && p.x <= 95.0, "{name}: x={} out of inset bounds", p.x);
            assert!(p.y >= 5.0 && p.y <= 95.0, "{name}: y={} out of inset bounds", p.y);
        }
    }

    #[test]
    fn seed_point_scales_with_rect_offset_and_size() {
        let unit = seed_point(Rect::unit(), "child");
        let scaled = Rect { x0: 100.0, y0: 200.0, x1: 300.0, y1: 250.0 };
        let p = seed_point(scaled, "child");
        // Same relative position within the (differently offset/sized) rect.
        assert!((p.x - (scaled.x0 + unit.x * scaled.width())).abs() < 1e-9);
        assert!((p.y - (scaled.y0 + unit.y * scaled.height())).abs() < 1e-9);
    }

    #[test]
    fn near_collision_seeds_do_not_panic() {
        // Hunt a pair of short names whose seed points land within a tiny
        // epsilon of each other — the birthday paradox makes this common
        // enough over a few thousand candidates that we don't need to
        // hand-craft an exact FNV collision.
        let rect = Rect { x0: 0.0, y0: 0.0, x1: 1.0, y1: 1.0 };
        let mut candidates: Vec<(String, Vec2)> = (0..5000).map(|i| {
            let name = format!("f{i}");
            let p = seed_point(rect, &name);
            (name, p)
        }).collect();
        candidates.sort_by(|a, b| a.1.x.partial_cmp(&b.1.x).unwrap());

        let mut best: Option<(String, String, f64)> = None;
        for w in candidates.windows(2) {
            let d = ((w[0].1.x - w[1].1.x).powi(2) + (w[0].1.y - w[1].1.y).powi(2)).sqrt();
            if best.as_ref().map(|(_, _, bd)| d < *bd).unwrap_or(true) {
                best = Some((w[0].0.clone(), w[1].0.clone(), d));
            }
        }
        let (name_a, name_b, dist) = best.expect("at least one pair among 5000 candidates");
        assert!(dist < 0.01, "expected a near-collision under 0.01, got {dist}");

        let children = vec![
            ChildSpec { name: name_a, kind: NodeKind::File, height: 1.0 },
            ChildSpec { name: name_b, kind: NodeKind::File, height: 1.0 },
            ChildSpec { name: "other.txt".into(), kind: NodeKind::File, height: 1.0 },
        ];
        let field = layout_field(rect, &children);
        assert_eq!(field.cells.len(), 3, "near-collision seeds must not drop a cell");
        for cell in &field.cells {
            assert!(!cell.polygon.is_empty(), "{} got an empty polygon", cell.name);
            assert!(
                polygon_area(&cell.polygon) > 0.0,
                "{} got a zero-area polygon from near-colliding seeds: {:?}",
                cell.name,
                cell.polygon
            );
        }
    }

    // ── layout_field: degenerate cases (guarantee 6) ────────────────────────

    #[test]
    fn zero_children_yields_empty_field() {
        let field = layout_field(Rect::unit(), &[]);
        assert!(field.cells.is_empty());
    }

    #[test]
    #[should_panic(expected = "strictly positive width and height")]
    fn degenerate_rect_panics_loudly() {
        // Zero width: the old silent fallback would have produced
        // overlapping full-rect polygons; now it must refuse at entry.
        let rect = Rect { x0: 1.0, y0: 0.0, x1: 1.0, y1: 1.0 };
        layout_field(rect, &[ChildSpec { name: "a".into(), kind: NodeKind::File, height: 1.0 }]);
    }

    // ── FsnCell::edges ───────────────────────────────────────────────────────

    #[test]
    fn edges_includes_the_closing_edge() {
        let rect = Rect::unit();
        let children: Vec<ChildSpec> = (0..7)
            .map(|i| ChildSpec { name: format!("e{i}"), kind: NodeKind::File, height: 1.0 })
            .collect();
        let field = layout_field(rect, &children);
        for cell in &field.cells {
            let edges: Vec<(Vec2, Vec2)> = cell.edges().collect();
            assert_eq!(
                edges.len(),
                cell.polygon.len(),
                "{}: edge count must equal vertex count (closed loop over an open polygon)",
                cell.name
            );
            let last = edges.last().unwrap();
            assert_eq!(last.0, *cell.polygon.last().unwrap());
            assert_eq!(last.1, cell.polygon[0], "{}: the final edge must close last→first", cell.name);
            // Consecutive edges chain: each edge starts where the previous ended.
            for w in edges.windows(2) {
                assert_eq!(w[0].1, w[1].0, "{}: edges must chain tip-to-tail", cell.name);
            }
        }
    }

    #[test]
    fn edges_of_degenerate_polygons_are_empty() {
        let mk = |polygon: Vec<Vec2>| FsnCell {
            name: "d".into(),
            kind: NodeKind::File,
            seed: Vec2::new(0.0, 0.0),
            polygon,
            height: 0.0,
        };
        assert_eq!(mk(vec![]).edges().count(), 0);
        assert_eq!(mk(vec![Vec2::new(0.5, 0.5)]).edges().count(), 0, "a single vertex has no self-loop edge");
    }

    #[test]
    fn one_child_gets_the_whole_rect() {
        let rect = Rect { x0: 0.0, y0: 0.0, x1: 10.0, y1: 10.0 };
        let children = vec![ChildSpec { name: "only.txt".into(), kind: NodeKind::File, height: 2.0 }];
        let field = layout_field(rect, &children);
        assert_eq!(field.cells.len(), 1);
        let cell = &field.cells[0];
        assert_eq!(cell.name, "only.txt");
        assert_eq!(cell.height, 2.0);
        let area = polygon_area(&cell.polygon);
        assert!((area - rect.area()).abs() < 1e-9);
    }

    #[test]
    fn two_children_do_not_panic_and_partition_the_rect() {
        let rect = Rect::unit();
        let children = vec![
            ChildSpec { name: "a".into(), kind: NodeKind::Dir, height: 1.0 },
            ChildSpec { name: "b".into(), kind: NodeKind::File, height: 1.0 },
        ];
        let field = layout_field(rect, &children);
        assert_eq!(field.cells.len(), 2);
        let total_area: f64 = field.cells.iter().map(|c| polygon_area(&c.polygon)).sum();
        assert!((total_area - rect.area()).abs() < 1e-6);
    }

    #[test]
    fn collinear_seeds_do_not_panic() {
        // Force every seed onto a shared horizontal band by using a rect
        // that's razor-thin in y — seed_point's y fraction still varies, but
        // the resulting spread is tiny relative to x, close to collinear.
        let rect = Rect { x0: 0.0, y0: 0.0, x1: 1000.0, y1: 0.001 };
        let children: Vec<ChildSpec> = (0..12)
            .map(|i| ChildSpec { name: format!("n{i:02}"), kind: NodeKind::File, height: 1.0 })
            .collect();
        let field = layout_field(rect, &children);
        assert_eq!(field.cells.len(), 12);
        for cell in &field.cells {
            assert!(!cell.polygon.is_empty());
        }
    }

    // ── layout_field: coverage + disjointness (guarantee 4) ─────────────────

    fn polygon_area(points: &[Vec2]) -> f64 {
        if points.len() < 3 {
            return 0.0;
        }
        let mut sum = 0.0;
        for i in 0..points.len() {
            let a = points[i];
            let b = points[(i + 1) % points.len()];
            sum += a.x * b.y - b.x * a.y;
        }
        sum.abs() / 2.0
    }

    /// Ray-casting point-in-polygon (even-odd rule), with a tiny epsilon
    /// treatment for points that land exactly on an edge (a seed can sit on
    /// a shared Voronoi boundary after relaxation in rare symmetric cases).
    fn point_in_polygon(p: Vec2, poly: &[Vec2]) -> bool {
        if poly.len() < 3 {
            return false;
        }
        // Edge/vertex proximity check first, so boundary-sitting points
        // (symmetric layouts) count as "in".
        for i in 0..poly.len() {
            let a = poly[i];
            let b = poly[(i + 1) % poly.len()];
            if point_near_segment(p, a, b, 1e-7) {
                return true;
            }
        }
        let mut inside = false;
        let mut j = poly.len() - 1;
        for i in 0..poly.len() {
            let pi = poly[i];
            let pj = poly[j];
            if ((pi.y > p.y) != (pj.y > p.y))
                && (p.x < (pj.x - pi.x) * (p.y - pi.y) / (pj.y - pi.y) + pi.x)
            {
                inside = !inside;
            }
            j = i;
        }
        inside
    }

    fn point_near_segment(p: Vec2, a: Vec2, b: Vec2, eps: f64) -> bool {
        let abx = b.x - a.x;
        let aby = b.y - a.y;
        let len2 = abx * abx + aby * aby;
        if len2 < 1e-18 {
            return ((p.x - a.x).powi(2) + (p.y - a.y).powi(2)).sqrt() < eps;
        }
        let t = (((p.x - a.x) * abx + (p.y - a.y) * aby) / len2).clamp(0.0, 1.0);
        let proj = Vec2::new(a.x + t * abx, a.y + t * aby);
        ((p.x - proj.x).powi(2) + (p.y - proj.y).powi(2)).sqrt() < eps
    }

    #[test]
    fn cell_areas_sum_to_rect_area() {
        let rect = Rect { x0: 0.0, y0: 0.0, x1: 1.0, y1: 1.0 };
        let children: Vec<ChildSpec> = (0..40)
            .map(|i| ChildSpec { name: format!("child-{i:03}"), kind: NodeKind::File, height: 1.0 })
            .collect();
        let field = layout_field(rect, &children);
        let total: f64 = field.cells.iter().map(|c| polygon_area(&c.polygon)).sum();
        assert!((total - rect.area()).abs() < 1e-6, "total {total} vs rect area {}", rect.area());
    }

    #[test]
    fn every_seed_lies_inside_its_own_polygon() {
        let rect = Rect { x0: 0.0, y0: 0.0, x1: 1.0, y1: 1.0 };
        let children: Vec<ChildSpec> = (0..25)
            .map(|i| ChildSpec { name: format!("item-{i:02}"), kind: NodeKind::Dir, height: 1.0 })
            .collect();
        let field = layout_field(rect, &children);
        for cell in &field.cells {
            assert!(
                point_in_polygon(cell.seed, &cell.polygon),
                "{}'s final seed {:?} is outside its own polygon {:?}",
                cell.name,
                cell.seed,
                cell.polygon
            );
        }
    }

    // ── layout_field: clipping (guarantee 5) ─────────────────────────────────

    #[test]
    fn no_polygon_vertex_escapes_the_rect() {
        let rect = Rect { x0: -3.0, y0: 7.0, x1: 12.0, y1: 20.0 };
        let children: Vec<ChildSpec> = (0..30)
            .map(|i| ChildSpec { name: format!("x{i}"), kind: NodeKind::File, height: 1.0 })
            .collect();
        let field = layout_field(rect, &children);
        for cell in &field.cells {
            for v in &cell.polygon {
                assert!(
                    rect.contains_point(*v, 1e-6),
                    "{} has a vertex {:?} outside rect {:?}",
                    cell.name,
                    v,
                    rect
                );
            }
        }
    }

    // ── layout_field: determinism (guarantee 1) ──────────────────────────────

    #[test]
    fn determinism_is_independent_of_input_order() {
        let rect = Rect { x0: 0.0, y0: 0.0, x1: 1.0, y1: 1.0 };
        let names = ["zeta", "alpha", "mu", "beta", "gamma", "eta", "delta", "iota"];
        let make = |order: &[&str]| -> Vec<ChildSpec> {
            order
                .iter()
                .map(|n| ChildSpec { name: (*n).to_string(), kind: NodeKind::File, height: 1.0 })
                .collect()
        };

        let forward = layout_field(rect, &make(&names));

        let mut reversed_names = names.to_vec();
        reversed_names.reverse();
        let reversed = layout_field(rect, &make(&reversed_names));

        let mut shuffled_names = names.to_vec();
        shuffled_names.swap(0, 5);
        shuffled_names.swap(1, 6);
        shuffled_names.swap(2, 7);
        let shuffled = layout_field(rect, &make(&shuffled_names));

        assert_eq!(forward, reversed, "reversing the input order must not change the field");
        assert_eq!(forward, shuffled, "shuffling the input order must not change the field");

        // Also confirm the "name-sorted output order" half of the contract.
        let mut sorted_names: Vec<&str> = names.to_vec();
        sorted_names.sort();
        let got_names: Vec<&str> = forward.cells.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(got_names, sorted_names);
    }

    // ── layout_field: blast radius (guarantee 2) ─────────────────────────────

    /// Rotation-invariant polygon comparison: `voronator` re-triangulates
    /// from scratch on every call, and while an *unaffected* site's cell is
    /// combinatorially identical whether or not an unrelated site exists
    /// elsewhere in the point set, its polygon's vertex list can start at a
    /// different rotation depending on internal traversal order. Same
    /// winding direction is guaranteed (both come from the same
    /// deterministic algorithm); only the starting vertex can differ.
    fn polygons_equal_up_to_rotation(a: &[Vec2], b: &[Vec2], eps: f64) -> bool {
        if a.len() != b.len() {
            return false;
        }
        if a.is_empty() {
            return true;
        }
        let close = |p: Vec2, q: Vec2| (p.x - q.x).abs() < eps && (p.y - q.y).abs() < eps;
        for start in 0..b.len() {
            if (0..a.len()).all(|i| close(a[i], b[(start + i) % b.len()])) {
                return true;
            }
        }
        false
    }

    /// The neighbor set of `name` in the Voronoi diagram built from `points`
    /// (name-sorted, matching `layout_field`'s own convention), excluding
    /// helper-point indices `voronator` appends past the real sites.
    fn neighbors_of(sorted_names: &[String], points: &[(f64, f64)], rect: Rect, name: &str) -> Vec<String> {
        let diagram = build_voronoi(rect, points).expect("test point sets always triangulate");
        let i = sorted_names.iter().position(|n| n == name).expect("name must be in sorted_names");
        diagram.neighbors[i]
            .iter()
            .filter(|&&j| j < sorted_names.len())
            .map(|&j| sorted_names[j].clone())
            .collect()
    }

    /// The blast-radius guarantee `docs/scenes/vfs.md` calls out: inserting
    /// one new child must only perturb cells near the insertion, not the
    /// whole field. `k`-fixed Lloyd relaxation is what bounds this: each
    /// relaxation step can only move a site whose Voronoi cell shape
    /// differs from what it would've been without the new site, and that
    /// can only be true of sites within a shrinking neighborhood of the
    /// insertion at each step.
    ///
    /// Rather than *predicting* a hop count and asserting against it (a
    /// single before/after Voronoi-adjacency snapshot is only an
    /// approximation of the graph that actually drives each relaxation
    /// step, and was flaky here in practice), this test computes the
    /// **exact** trajectories for the 40-child and 41-child cases via
    /// [`lloyd_relax_trajectory`] and compares them position-by-position, so
    /// there's no guessing about which cells "should" be affected:
    ///
    /// 1. At `t = 0` (raw, pre-relaxation seeds), every original name's
    ///    point must be *exactly* identical between the two runs —
    ///    [`seed_point`] depends only on `(rect, name)`, never on siblings.
    /// 2. The set of original names whose point ever differs across the
    ///    trajectory (`t = 0..=LLOYD_ITERATIONS`) must be a small minority
    ///    of the field, not the whole thing — this is what "fixed-k, not
    ///    iterate-to-convergence" buys: an unbounded ripple (e.g. a bug
    ///    that iterates until stable) would make most or all of the 40
    ///    sites move by the final step.
    /// 3. Every cell whose **final polygon** differs must be explained by
    ///    that moved set: either its own seed moved, or one of its
    ///    neighbors *in the actual final diagram* (checked in both the
    ///    40-site and 41-site final diagrams, since a cell's neighbor set
    ///    can itself differ by one insertion) has a seed that moved. A
    ///    polygon change with no such explanation would mean influence
    ///    leaked further than the relaxation graph allows — a real bug.
    #[test]
    fn inserting_one_child_only_perturbs_a_bounded_neighborhood() {
        let rect = Rect { x0: 0.0, y0: 0.0, x1: 1.0, y1: 1.0 };
        let names: Vec<String> = (0..40).map(|i| format!("child-{i:03}")).collect();
        let new_name = "child-020b".to_string(); // sorts between child-020 and child-021

        let mut sorted_b = names.clone();
        sorted_b.push(new_name.clone());
        sorted_b.sort();

        let points_a0: Vec<(f64, f64)> = names.iter().map(|n| seed_point(rect, n).into()).collect();
        let points_b0: Vec<(f64, f64)> = sorted_b.iter().map(|n| seed_point(rect, n).into()).collect();

        let trajectory_a = lloyd_relax_trajectory(rect, &points_a0);
        let trajectory_b = lloyd_relax_trajectory(rect, &points_b0);
        assert_eq!(trajectory_a.len(), LLOYD_ITERATIONS + 1);
        assert_eq!(trajectory_b.len(), LLOYD_ITERATIONS + 1);

        let idx_a = |name: &str| names.iter().position(|n| n == name).unwrap();
        let idx_b = |name: &str| sorted_b.iter().position(|n| n == name).unwrap();
        let close = |p: (f64, f64), q: (f64, f64)| (p.0 - q.0).abs() < 1e-9 && (p.1 - q.1).abs() < 1e-9;

        // 1. Raw seeds must be pointwise identical for every original name.
        for name in &names {
            assert!(
                close(trajectory_a[0][idx_a(name)], trajectory_b[0][idx_b(name)]),
                "{name}'s raw seed differs between the 40- and 41-child runs — seed_point must not depend on siblings"
            );
        }

        // 2. The moved set (any point in the trajectory that ever diverges)
        // must be small — the actual, exact blast radius.
        let moved_anytime: std::collections::HashSet<&str> = names
            .iter()
            .filter(|name| {
                (0..=LLOYD_ITERATIONS).any(|t| !close(trajectory_a[t][idx_a(name)], trajectory_b[t][idx_b(name)]))
            })
            .map(|s| s.as_str())
            .collect();

        assert!(!moved_anytime.is_empty(), "sanity: a nearby insertion should nudge at least one original seed");
        // NB: with only 40 sites total, a 2-hop Voronoi neighborhood is
        // geometrically NOT a small sliver — average Delaunay degree is ~6,
        // and 2D planar-graph "ball growth" (area, not exponential fan-out)
        // means a genuinely-correct 2-hop bound routinely covers 40-60% of
        // a field this small (observed: ~21/40 in this exact scenario).
        // This bound exists to catch the *actual* regression this test is
        // for — a fixed-k loop turning into an iterate-to-convergence loop,
        // which (as a real CVT relaxation) shifts nearly *every* site, not
        // just those near one insertion — not to claim "2 hops = a tiny
        // sliver" for a small field. See check 3 below for the precise,
        // non-heuristic locality proof. The 32-of-40 bound is calibrated
        // for LLOYD_ITERATIONS = 2: if that constant changes, this bound
        // must be recalibrated — a correct k bump legitimately grows the
        // moved set and would break this assert without any bug existing.
        assert!(
            moved_anytime.len() <= 32,
            "blast radius touched {} of 40 original seeds — looks like unbounded ripple, not k-fixed relaxation: {:?}",
            moved_anytime.len(),
            moved_anytime
        );

        // 3. Build the real fields and check every polygon change is
        // explained by the moved set (own seed, or a real final-diagram
        // neighbor's seed).
        let original: Vec<ChildSpec> =
            names.iter().map(|n| ChildSpec { name: n.clone(), kind: NodeKind::File, height: 1.0 }).collect();
        let field_a = layout_field(rect, &original);
        let mut with_insert = original.clone();
        with_insert.push(ChildSpec { name: new_name.clone(), kind: NodeKind::Dir, height: 1.0 });
        let field_b = layout_field(rect, &with_insert);
        assert_eq!(field_a.cells.len(), 40);
        assert_eq!(field_b.cells.len(), 41);

        let final_points_a = &trajectory_a[LLOYD_ITERATIONS];
        let final_points_b = &trajectory_b[LLOYD_ITERATIONS];

        // "explained" = moved itself, or adjacent (in EITHER final diagram)
        // to something that moved, or adjacent to the brand-new child.
        let mut explained: std::collections::HashSet<&str> = moved_anytime.clone();
        for name in &moved_anytime {
            explained.extend(neighbors_of(&names, final_points_a, rect, name).into_iter().filter_map(|n| {
                names.iter().find(|orig| orig.as_str() == n).map(|s| s.as_str())
            }));
            explained.extend(
                neighbors_of(&sorted_b, final_points_b, rect, name)
                    .into_iter()
                    .filter_map(|n| names.iter().find(|orig| orig.as_str() == n).map(|s| s.as_str())),
            );
        }
        // The new child itself is adjacent to some original sites in
        // field_b's final diagram — those are explained too (their polygon
        // now borders a site that didn't exist before).
        explained.extend(
            neighbors_of(&sorted_b, final_points_b, rect, &new_name)
                .into_iter()
                .filter_map(|n| names.iter().find(|orig| orig.as_str() == n).map(|s| s.as_str())),
        );

        let mut far_checked = 0;
        for name in &names {
            let cell_a = field_a.cells.iter().find(|c| &c.name == name).unwrap();
            let cell_b = field_b.cells.iter().find(|c| &c.name == name).unwrap();
            let polygon_changed = !polygons_equal_up_to_rotation(&cell_a.polygon, &cell_b.polygon, 1e-6);
            if polygon_changed {
                assert!(
                    explained.contains(name.as_str()),
                    "{name}'s polygon changed but it's outside the moved-set-plus-neighbors explanation — \
                     influence leaked further than the relaxation graph allows:\n  before: {:?}\n  after:  {:?}",
                    cell_a.polygon,
                    cell_b.polygon
                );
            } else if !explained.contains(name.as_str()) {
                far_checked += 1;
            }
        }
        assert!(far_checked > 0, "sanity: at least one clearly-unaffected far cell must exist in a 41-site field");
    }
}
