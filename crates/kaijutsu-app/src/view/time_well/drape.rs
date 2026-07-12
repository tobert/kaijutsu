//! Lineage drapes: on-selection curved ribbons hanging down the bowl wall
//! from the selected card to each of its fork-ancestors' cards
//! (`docs/timewell.md`, "The bowl, revisited" — mockup 27's "silk threads
//! from rim cards to their ancestry below," made spatial instead of only the
//! West HUD panel's text list). Slice 1 of the HUD-melt plan; the West panel
//! stays until slice 4 retires it.
//!
//! Two layers, same split as every other module here:
//! - **Pure curve math** ([`drape_points`], [`drape_ribbon_vertices`]) — no
//!   Bevy `World`, no GPU, unit-tested below. Generalizes
//!   `room::bearing::ribbon_vertices`'s cumulative-arclength ribbon idiom
//!   from the room floor's fixed XZ plane to an arbitrary 3D curve (a drape
//!   isn't floor-bound).
//! - **The system** ([`sync_lineage_drapes`]) — spawns/updates/despawns one
//!   ribbon mesh entity per ancestor, reusing [`TraceGlowMaterial`] — the
//!   same faint-glow binding idiom the room's floor traces and console ring
//!   already use, not a new material.

use std::collections::{HashMap, HashSet};

use bevy::prelude::*;
use kaijutsu_types::ContextId;

use super::card;
use super::scene::{Card, TimeWellRoot, TimeWellState, effective_selection, well_zoomed};
use crate::shaders::TraceGlowMaterial;
use crate::view::scene_palette::ScenePalette;

// ============================================================================
// PURE CURVE MATH
// ============================================================================

/// Points sampled along a drape curve, endpoints included ([`drape_points`]
/// returns this many + 1).
const DRAPE_SEGMENTS: usize = 16;

/// How far a drape bulges off the straight chord between two cards, toward
/// the funnel's central axis — as a fraction of the chord length, so a short
/// same-ring hop and a long mouth-to-throat one both read proportionate.
/// **Amy-tunable.**
const DRAPE_SAG_FRACTION: f32 = 0.18;

/// Ribbon width (well-local units) — thread-thin next to a card's
/// [`super::scene::CARD_WIDTH`] (120). **Amy-tunable.**
const DRAPE_WIDTH: f32 = 5.0;

/// Sample a curved path from `start` to `end` (well-local space — the same
/// frame `Card`'s `Transform.translation` lives in, already
/// [`card::well_tilt_quat`]-tilted; see [`card::ring_seat_rotated`]), bulging
/// toward the funnel's central axis (`axis`, a unit vector through the local
/// origin — `well_tilt_quat() * Vec3::Z`) rather than cutting a straight line
/// through open air outside the bowl's interior surface.
///
/// That single "pull toward the axis" does double duty as both "inward" and
/// "downward": an ancestor on a colder ring sits both at a SMALLER radius and
/// FARTHER along the axis than the selection's own ring (`card::band_ring`),
/// so a curve that bows toward the axis reads as draping down the bowl wall,
/// not just arcing sideways through empty space — no separate world-space
/// "down" vector needed (and the well's own local frame isn't level with the
/// room's, so a literal world-down would need the placement rotation threaded
/// in here for no visual gain).
///
/// A sine profile (0 at both ends, peak at the midpoint) stands in for a true
/// catenary's `cosh` — close enough for a subtle selection ornament, and it
/// keeps the endpoints exact without a transcendental solve. Returns
/// [`DRAPE_SEGMENTS`] + 1 points, `start` first and `end` last.
pub fn drape_points(start: Vec3, end: Vec3, axis: Vec3) -> Vec<Vec3> {
    let axis = axis.normalize_or_zero();
    let sag = start.distance(end) * DRAPE_SAG_FRACTION;
    (0..=DRAPE_SEGMENTS)
        .map(|i| {
            let t = i as f32 / DRAPE_SEGMENTS as f32;
            let base = start.lerp(end, t);
            let inward = radial_inward(base, axis);
            base + inward * sag * (std::f32::consts::PI * t).sin()
        })
        .collect()
}

/// Unit vector from `point` toward the nearest point on the axis line through
/// the local origin along `axis` (already unit length; `Vec3::ZERO` axis is
/// treated as degenerate). `Vec3::ZERO` when `point` already sits on the axis
/// — nothing to pull toward.
fn radial_inward(point: Vec3, axis: Vec3) -> Vec3 {
    if axis == Vec3::ZERO {
        return Vec3::ZERO;
    }
    let along = point.dot(axis) * axis;
    (along - point).normalize_or_zero()
}

/// Build a flat ribbon strip (`width` well-local units across) along a
/// drape's sampled `points`, generalizing `room::bearing::ribbon_vertices`'
/// cumulative-arclength idiom (same 2-verts-per-point layout, same `uv.x` =
/// arclength-fraction convention [`TraceGlowMaterial`]'s mode-0 traveling
/// wave rides) from the room floor's fixed XZ plane to a `plane_normal` the
/// caller derives from the curve's own chord + axis. [`drape_points`]'s sag
/// is a single vector times a scalar profile, so the whole curve is planar —
/// one CONSTANT normal is enough, no per-vertex tangent frame needed.
///
/// Emits BOTH triangle windings per segment (double the room ribbon's
/// single-sided count): a floor trace is always viewed from above, so
/// `bearing::ribbon_vertices`'s fixed up-normal winding is always
/// front-facing and single-sided culling is free there; a drape's plane
/// tilts with wherever its ancestor sits, and the well's camera dollies
/// through many framings across rings/hero pose, so a single winding would
/// vanish from some angles. Doubling the triangles (not disabling culling on
/// the shared `TraceGlowMaterial` pipeline other single-sided consumers rely
/// on) keeps this local to the drapes.
///
/// `points.len() < 2` (degenerate/empty curve) returns empty vectors.
pub fn drape_ribbon_vertices(
    points: &[Vec3],
    width: f32,
    plane_normal: Vec3,
) -> (Vec<[f32; 3]>, Vec<[f32; 2]>, Vec<u32>) {
    if points.len() < 2 {
        return (Vec::new(), Vec::new(), Vec::new());
    }
    let half = width * 0.5;
    let n = points.len();
    let normal = plane_normal.normalize_or_zero();

    // Cumulative TRUE 3D arclength up to each point (not the room floor's
    // XZ-only projection — a drape isn't confined to a horizontal plane).
    let mut cum = vec![0.0f32; n];
    for i in 1..n {
        cum[i] = cum[i - 1] + points[i].distance(points[i - 1]);
    }
    let total = cum[n - 1];

    let mut positions = Vec::with_capacity(n * 2);
    let mut uvs = Vec::with_capacity(n * 2);
    for i in 0..n {
        // Tangent via central difference (forward/backward at the ends).
        let prev = points[i.saturating_sub(1)];
        let next = points[(i + 1).min(n - 1)];
        let tangent = (next - prev).normalize_or_zero();
        let side = tangent.cross(normal).normalize_or_zero() * half;
        let u = if total > 1e-6 { cum[i] / total } else { 0.0 };
        positions.push((points[i] + side).to_array());
        positions.push((points[i] - side).to_array());
        uvs.push([u, 0.0]);
        uvs.push([u, 1.0]);
    }
    let mut indices = Vec::with_capacity((n - 1) * 12);
    for i in 0..(n - 1) {
        let a = (i * 2) as u32;
        let (l0, r0, l1, r1) = (a, a + 1, a + 2, a + 3);
        indices.extend_from_slice(&[l0, l1, r0, r0, l1, r1]); // front winding
        indices.extend_from_slice(&[l0, r0, l1, r0, r1, l1]); // mirrored back winding
    }
    (positions, uvs, indices)
}

#[cfg(test)]
mod math_tests {
    use super::*;

    #[test]
    fn drape_points_starts_and_ends_on_the_cards() {
        let start = Vec3::new(300.0, 40.0, -20.0);
        let end = Vec3::new(60.0, -30.0, -180.0);
        let pts = drape_points(start, end, Vec3::Z);
        assert!(pts.first().unwrap().distance(start) < 1e-4, "first point is the selected card");
        assert!(pts.last().unwrap().distance(end) < 1e-4, "last point is the ancestor card");
    }

    #[test]
    fn drape_points_returns_segments_plus_one_points() {
        let pts = drape_points(Vec3::ZERO, Vec3::new(100.0, 0.0, 0.0), Vec3::Z);
        assert_eq!(pts.len(), DRAPE_SEGMENTS + 1);
    }

    #[test]
    fn drape_points_sag_pulls_toward_the_axis() {
        // Both endpoints at the same radius from the Z axis; the straight
        // chord's own midpoint radius is the baseline any inward bulge must
        // beat — the sag is `drape_points`' doing, not incidental geometry.
        let start = Vec3::new(100.0, 0.0, 0.0);
        let end = Vec3::new(0.0, 100.0, -50.0);
        let pts = drape_points(start, end, Vec3::Z);
        let radius = |p: Vec3| (p.x * p.x + p.y * p.y).sqrt();
        let straight_mid_radius = radius(start.lerp(end, 0.5));
        let mid = pts[DRAPE_SEGMENTS / 2];
        assert!(
            radius(mid) < straight_mid_radius,
            "midpoint ({}) must sit closer to the axis than the straight chord ({straight_mid_radius})",
            radius(mid)
        );
    }

    #[test]
    fn drape_points_on_axis_stays_straight() {
        // Both endpoints already sit ON the axis: no radial direction to pull
        // toward (`radial_inward` degenerates to ZERO) — must not NaN or
        // drift off the chord.
        let start = Vec3::new(0.0, 0.0, 0.0);
        let end = Vec3::new(0.0, 0.0, -200.0);
        let pts = drape_points(start, end, Vec3::Z);
        for p in &pts {
            assert!(p.is_finite(), "on-axis drape must stay finite: {p:?}");
            assert!(p.x.abs() < 1e-4 && p.y.abs() < 1e-4, "on-axis drape must not bow sideways: {p:?}");
        }
    }

    #[test]
    fn drape_points_sag_scales_with_chord_length() {
        let short = drape_points(Vec3::new(50.0, 0.0, 0.0), Vec3::new(0.0, 50.0, 0.0), Vec3::Z);
        let long = drape_points(Vec3::new(500.0, 0.0, 0.0), Vec3::new(0.0, 500.0, 0.0), Vec3::Z);
        let radius = |p: Vec3| (p.x * p.x + p.y * p.y).sqrt();
        let short_dip = 50.0 - radius(short[DRAPE_SEGMENTS / 2]);
        let long_dip = 500.0 - radius(long[DRAPE_SEGMENTS / 2]);
        assert!(long_dip > short_dip, "a longer chord sags a proportionately larger absolute amount");
    }

    #[test]
    fn ribbon_vertices_reject_degenerate_input() {
        let (pos, uvs, idx) = drape_ribbon_vertices(&[Vec3::ZERO], 5.0, Vec3::Y);
        assert!(pos.is_empty() && uvs.is_empty() && idx.is_empty());
    }

    #[test]
    fn ribbon_vertices_are_two_per_point_and_double_wound() {
        let pts = drape_points(Vec3::new(200.0, 0.0, 0.0), Vec3::new(0.0, 200.0, -100.0), Vec3::Z);
        let normal = (*pts.last().unwrap() - pts[0]).cross(Vec3::Z).normalize();
        let (pos, uvs, idx) = drape_ribbon_vertices(&pts, 5.0, normal);
        assert_eq!(pos.len(), pts.len() * 2, "2 verts per sampled point");
        assert_eq!(uvs.len(), pos.len(), "one uv per vertex");
        assert_eq!(idx.len(), (pts.len() - 1) * 12, "6 indices × 2 windings × segment count");
    }

    #[test]
    fn ribbon_vertices_uv_x_is_monotonic_true_3d_arclength() {
        // Real motion on every axis — an XZ-only arclength (the room floor's
        // assumption) would under/over-count this curve's true length.
        let pts = [
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(10.0, 40.0, 5.0),
            Vec3::new(30.0, 10.0, -5.0),
            Vec3::new(30.0, 10.0, 40.0),
        ];
        let (_, uvs, _) = drape_ribbon_vertices(&pts, 4.0, Vec3::Y);
        let u_at = |i: usize| uvs[i * 2][0];
        assert!(u_at(0).abs() < 1e-6, "starts at 0: {}", u_at(0));
        assert!((u_at(pts.len() - 1) - 1.0).abs() < 1e-6, "ends at 1: {}", u_at(pts.len() - 1));
        for i in 0..pts.len() - 1 {
            assert!(u_at(i) <= u_at(i + 1) + 1e-6, "uv.x must be monotonic along the curve");
        }
    }

    #[test]
    fn ribbon_vertices_width_is_perpendicular_to_the_tangent() {
        let pts = [Vec3::new(0.0, 0.0, 0.0), Vec3::new(100.0, 0.0, 0.0), Vec3::new(200.0, 0.0, 0.0)];
        let (pos, _, _) = drape_ribbon_vertices(&pts, 10.0, Vec3::Y);
        // Straight line along +X, plane normal +Y → width runs along ±Z
        // (tangent × normal = X × Y = Z).
        let left = Vec3::from_array(pos[0]);
        let right = Vec3::from_array(pos[1]);
        assert!((left.z - 5.0).abs() < 1e-4 || (left.z + 5.0).abs() < 1e-4, "left offset along Z: {left:?}");
        assert!((left.z - right.z).abs() > 9.0, "left/right straddle the centerline: {left:?} {right:?}");
    }
}

// ============================================================================
// COMPONENTS
// ============================================================================

/// One lineage-drape ribbon: the ancestor context id it drapes down to (the
/// join key — [`sync_lineage_drapes`] diffs against this, same as every
/// other keyed reconcile in this crate).
#[derive(Component)]
pub struct LineageDrape(pub ContextId);

/// The `(selected, ancestor)` well-local positions a drape's mesh was last
/// built from — change-guards the mesh rebuild (rewrite only when the
/// geometry actually moved, not every frame, same discipline as every other
/// per-frame system in this module). `pub(crate)`, not private: it appears in
/// [`sync_lineage_drapes`]'s `Query` type, and that system is wired from
/// `TimeWellPlugin` (`pub(crate) fn build`, reachable crate-wide) — a
/// `pub(super)` field here would be narrower than its own public signature.
#[derive(Component)]
pub(crate) struct DrapeEndpoints(Vec3, Vec3);

// ============================================================================
// SYSTEM
// ============================================================================

/// Slow, uniform breathing rate (mode 1 — `params.y` in
/// [`TraceGlowMaterial`]) for the whole lineage set: one calm ensemble on
/// selection, not a per-thread traveling crest — "a lineage indicator, not a
/// fireworks show."
const DRAPE_BREATH_RATE: f32 = 0.35;

/// The one shared drape material — built lazily on first use
/// ([`TimeWellState::lineage_drape_material`]), reused by every drape entity
/// (they're all the same hue/intensity; only their mesh geometry differs).
/// Gold, matching the existing per-card lineage ring's hue
/// (`well_card.wgsl`'s amber `params.y` ring) so the new bowl-wall drapes
/// read as the same concept the card face already taught, rather than
/// introducing a second "lineage color."
fn lineage_drape_material(palette: &ScenePalette) -> TraceGlowMaterial {
    TraceGlowMaterial {
        color: crate::view::room::crest_color(ScenePalette::vec3(palette.gold).to_array(), palette.crest),
        params: Vec4::new(0.0, DRAPE_BREATH_RATE, palette.trough_subtle, 1.0),
    }
}

/// Wrap [`drape_points`] + [`drape_ribbon_vertices`] into a renderable
/// [`Mesh`] — the non-pure half (mirrors `room::ribbon_mesh`'s own wrapper
/// shape: pure vertex math above, a thin Bevy-facing builder here).
fn drape_mesh(start: Vec3, end: Vec3, axis: Vec3) -> Mesh {
    use bevy::asset::RenderAssetUsages;
    use bevy::mesh::{Indices, PrimitiveTopology};

    let points = drape_points(start, end, axis);
    let plane_normal = {
        let n = (end - start).cross(axis);
        if n == Vec3::ZERO { Vec3::Y } else { n.normalize() }
    };
    let (positions, uvs, indices) = drape_ribbon_vertices(&points, DRAPE_WIDTH, plane_normal);
    let normals = vec![plane_normal.to_array(); positions.len()];
    Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default())
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
        .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, uvs)
        .with_inserted_indices(Indices::U32(indices))
}

/// Despawn every currently-spawned drape (shared by every "nothing should be
/// showing" branch of [`sync_lineage_drapes`] below).
fn despawn_all(
    commands: &mut Commands,
    drapes: &Query<(Entity, &LineageDrape, &mut DrapeEndpoints, &Mesh3d)>,
) {
    for (entity, ..) in drapes.iter() {
        commands.entity(entity).despawn();
    }
}

/// Build/update/despawn one ribbon mesh per fork-ancestor of the *effective*
/// selection (see [`effective_selection`] — folds the well's zoom gate in, so
/// a room-scale or no-selection frame clears every drape).
///
/// Ambient, not dived-only, for the same reason
/// [`super::scene::highlight_lineage`] is: it must react to BOTH zoom
/// directions — clearing on zoom-OUT, not just building on zoom-in — or a
/// drape set left over from the last dived frame stays frozen at room scale.
/// Mesh geometry is only rewritten when an endpoint's `Transform.translation`
/// actually changed since the last write ([`DrapeEndpoints`]) — while a card
/// eases toward its `CardTarget` the drape re-drapes every frame with it, and
/// once the card snaps to rest (`super::scene::move_cards_toward_target`'s
/// own snap-and-hold) the drape stops touching its mesh too.
///
/// An ancestor with no live `Card` entity — past the event horizon
/// (`docs/timewell.md`'s "no card entity, ever" past a ring's 10 seats) or
/// archived — has nowhere to draw a drape TO and is silently skipped; the
/// fork-lineage chain can run deeper than what's currently seated.
pub fn sync_lineage_drapes(
    mut commands: Commands,
    room: Res<crate::view::room::RoomState>,
    mut state: ResMut<TimeWellState>,
    palette: Res<ScenePalette>,
    mut materials: ResMut<Assets<TraceGlowMaterial>>,
    mut meshes: ResMut<Assets<Mesh>>,
    card_transforms: Query<&Transform, With<Card>>,
    roots: Query<Entity, With<TimeWellRoot>>,
    mut drapes: Query<(Entity, &LineageDrape, &mut DrapeEndpoints, &Mesh3d)>,
) {
    let Some(sel_id) = effective_selection(well_zoomed(&room), state.selected) else {
        despawn_all(&mut commands, &drapes);
        return;
    };
    let Some(&sel_entity) = state.entities.get(&sel_id) else {
        despawn_all(&mut commands, &drapes);
        return;
    };
    let Ok(sel_tf) = card_transforms.get(sel_entity) else {
        return; // just despawned / not spawned yet this frame
    };
    let sel_pos = sel_tf.translation;

    let lineage = card::ancestors(sel_id, |id| state.join.get(&id).and_then(|c| c.forked_from));
    let mut wanted: HashMap<ContextId, Vec3> = HashMap::with_capacity(lineage.len());
    for id in lineage {
        if let Some(&e) = state.entities.get(&id)
            && let Ok(tf) = card_transforms.get(e)
        {
            wanted.insert(id, tf.translation);
        }
    }

    // Despawn drapes for ancestors no longer in the lineage set (deselect,
    // selection change, or the ancestor's own card left the join) — runs
    // even when `wanted` is empty, which is exactly "despawn everything."
    for (entity, drape, ..) in drapes.iter() {
        if !wanted.contains_key(&drape.0) {
            commands.entity(entity).despawn();
        }
    }
    if wanted.is_empty() {
        return;
    }

    let axis = card::well_tilt_quat() * Vec3::Z;
    let material = state
        .lineage_drape_material
        .get_or_insert_with(|| materials.add(lineage_drape_material(&palette)))
        .clone();

    let mut present: HashSet<ContextId> = HashSet::with_capacity(wanted.len());
    for (_, drape, mut cached, mesh3d) in drapes.iter_mut() {
        let Some(&end) = wanted.get(&drape.0) else { continue };
        present.insert(drape.0);
        if cached.0 != sel_pos || cached.1 != end {
            cached.0 = sel_pos;
            cached.1 = end;
            if let Some(m) = meshes.get_mut(&mesh3d.0) {
                *m = drape_mesh(sel_pos, end, axis);
            }
        }
    }

    let Ok(root) = roots.single() else { return };
    for (id, end) in wanted {
        if present.contains(&id) {
            continue;
        }
        let mesh = meshes.add(drape_mesh(sel_pos, end, axis));
        commands.spawn((
            LineageDrape(id),
            DrapeEndpoints(sel_pos, end),
            Mesh3d(mesh),
            MeshMaterial3d(material.clone()),
            Transform::IDENTITY,
            Visibility::Inherited,
            Name::new(format!("LineageDrape({})", id.short())),
            ChildOf(root),
        ));
    }
}
