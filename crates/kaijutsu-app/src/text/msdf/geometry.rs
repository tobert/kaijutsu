//! Flat-colored triangle geometry for music notation — the vello-free
//! replacement for staff lines, barlines, stems, ledgers, beams, slurs,
//! ties, and repeat dots.
//!
//! Two primitives cover every `EngravingElement` kind that isn't a glyph or
//! text:
//! - [`stroke_line_quad`] turns a stroked `EngravingElement::Line` (staff
//!   line, barline, stem, ledger) into a butt-capped rectangle — exact, no
//!   rasterizer needed (S2).
//! - [`triangulate_polygon`] ear-clips a closed polygon into triangles.
//!   `EngravingElement::Path` (beams, slurs/ties, repeat dots) all arrive as
//!   an SVG path string; the caller flattens curves with `kurbo::flatten`
//!   first (a no-op for the beam case, which has none) so this function
//!   only ever sees straight-edged input. Beams are always convex
//!   (a 4-point parallelogram); slurs/ties are a crescent — two arcs
//!   bulging the *same* direction by different depths — which is genuinely
//!   non-convex, so a convex fan won't do (S3).
//!
//! Both are pure math: no bevy, no wgpu, no kurbo curve types beyond
//! `Point`. The render-world vertex builder (`renderer.rs`) converts the
//! `GeometryVertex` output to physical NDC, mirroring how `PositionedGlyph`
//! is built here and converted to NDC there.

use kurbo::{BezPath, PathEl, Point};

/// A flat-colored triangle-list vertex in block-local LOGICAL pixels (same
/// space as `PositionedGlyph::x/y`). The render-world builder converts to
/// physical NDC — never feed this into layout math directly.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GeometryVertex {
    pub x: f32,
    pub y: f32,
    pub color: [u8; 4],
}

impl GeometryVertex {
    fn new(p: Point, color: [u8; 4]) -> Self {
        Self {
            x: p.x as f32,
            y: p.y as f32,
            color,
        }
    }
}

/// Build a butt-capped stroked-line quad (2 triangles, 6 vertices) — the
/// geometry a vello `Stroke::new(width).with_caps(Cap::Butt)` line produces,
/// without going through vello. Degenerate (zero-length) lines fall back to
/// a small dot (`width` square) rather than vanishing — a zero-length
/// engraving line is unexpected input, not a case worth an empty draw for.
pub fn stroke_line_quad(x1: f64, y1: f64, x2: f64, y2: f64, width: f64, color: [u8; 4]) -> [GeometryVertex; 6] {
    let dx = x2 - x1;
    let dy = y2 - y1;
    let len = (dx * dx + dy * dy).sqrt();
    let half = width / 2.0;
    // The quad is `(sx1,sy1)-(sx2,sy2)` extruded by the half-width normal
    // `(nx,ny)`. A zero-length line has no direction to take a normal from,
    // so it becomes a `width` square centered on the point: extruding the
    // zero-length segment itself would give a finite but zero-AREA quad,
    // which the rasterizer drops — silently swallowing the anomalous input
    // instead of showing it.
    let (sx1, sy1, sx2, sy2, nx, ny) = if len > 1e-9 {
        (x1, y1, x2, y2, -dy / len * half, dx / len * half)
    } else {
        (x1 - half, y1, x1 + half, y1, 0.0, half)
    };

    let a = Point::new(sx1 + nx, sy1 + ny);
    let b = Point::new(sx2 + nx, sy2 + ny);
    let c = Point::new(sx2 - nx, sy2 - ny);
    let d = Point::new(sx1 - nx, sy1 - ny);

    [
        GeometryVertex::new(a, color),
        GeometryVertex::new(b, color),
        GeometryVertex::new(d, color),
        GeometryVertex::new(b, color),
        GeometryVertex::new(c, color),
        GeometryVertex::new(d, color),
    ]
}

/// Signed area (shoelace formula, doubled) of a polygon given in order. Sign
/// indicates winding: positive is one direction, negative the other — which
/// is which doesn't matter here, only that it's consistent so `is_convex`
/// can compare against it.
fn signed_area2(points: &[Point]) -> f64 {
    let n = points.len();
    let mut sum = 0.0;
    for i in 0..n {
        let p = points[i];
        let q = points[(i + 1) % n];
        sum += p.x * q.y - q.x * p.y;
    }
    sum
}

fn cross(o: Point, a: Point, b: Point) -> f64 {
    (a.x - o.x) * (b.y - o.y) - (a.y - o.y) * (b.x - o.x)
}

/// True if `p` lies inside (or on the boundary of, within `EPS`) the
/// triangle `a, b, c`.
fn point_in_triangle(p: Point, a: Point, b: Point, c: Point) -> bool {
    const EPS: f64 = 1e-9;
    let d1 = cross(a, b, p);
    let d2 = cross(b, c, p);
    let d3 = cross(c, a, p);
    let has_neg = d1 < -EPS || d2 < -EPS || d3 < -EPS;
    let has_pos = d1 > EPS || d2 > EPS || d3 > EPS;
    !(has_neg && has_pos)
}

/// Ear-clip a simple (non-self-intersecting) polygon into triangles.
///
/// `points` are the polygon's vertices in order, either winding, with an
/// IMPLICIT closing edge from the last point back to the first (do not
/// repeat the first point at the end — that produces one degenerate
/// zero-length edge, not an error, but callers should just omit it).
///
/// Handles convex input (a beam's parallelogram) and non-convex input (a
/// slur/tie's crescent) identically. Fewer than 3 points returns an empty
/// Vec. A polygon ear-clipping can't fully reduce (self-intersecting or
/// otherwise degenerate input — not expected from the engraver, but ABC
/// source is arbitrary and not vetted before it reaches engraving) falls
/// back to a fan from the first vertex for whatever remains, logged once by
/// the caller: a cosmetically-imperfect curve beats an unrendered one or a
/// panicked render path.
pub fn triangulate_polygon(points: &[Point]) -> Vec<[Point; 3]> {
    let n = points.len();
    if n < 3 {
        return Vec::new();
    }
    if n == 3 {
        return vec![[points[0], points[1], points[2]]];
    }

    let area2 = signed_area2(points);
    if area2.abs() < 1e-9 {
        // Degenerate (collinear/zero-area) polygon — nothing to fill.
        return Vec::new();
    }
    let positive_winding = area2 > 0.0;

    let mut idx: Vec<usize> = (0..n).collect();
    let mut triangles = Vec::with_capacity(n - 2);

    while idx.len() > 3 {
        let m = idx.len();
        let mut clipped = false;

        for i in 0..m {
            let i_prev = idx[(i + m - 1) % m];
            let i_cur = idx[i];
            let i_next = idx[(i + 1) % m];
            let (p_prev, p_cur, p_next) = (points[i_prev], points[i_cur], points[i_next]);

            let cr = cross(p_prev, p_cur, p_next);
            let is_convex = if positive_winding { cr > 0.0 } else { cr < 0.0 };
            if !is_convex {
                continue;
            }

            // An ear if no other polygon vertex sits inside prev-cur-next.
            let mut any_inside = false;
            for &j in &idx {
                if j == i_prev || j == i_cur || j == i_next {
                    continue;
                }
                if point_in_triangle(points[j], p_prev, p_cur, p_next) {
                    any_inside = true;
                    break;
                }
            }
            if any_inside {
                continue;
            }

            triangles.push([p_prev, p_cur, p_next]);
            idx.remove(i);
            clipped = true;
            break;
        }

        if !clipped {
            // Couldn't find a valid ear — fall back to a fan from the first
            // remaining vertex rather than spinning forever.
            bevy::log::warn_once!(
                "triangulate_polygon: no ear found with {} vertices remaining — \
                 falling back to a fan triangulation (degenerate or \
                 self-intersecting input)",
                idx.len(),
            );
            let anchor = idx[0];
            for w in 1..idx.len() - 1 {
                triangles.push([points[anchor], points[idx[w]], points[idx[w + 1]]]);
            }
            return triangles;
        }
    }

    triangles.push([points[idx[0]], points[idx[1]], points[idx[2]]]);
    triangles
}

/// Flatten an already-transformed `BezPath` (curves resolved via
/// `kurbo::flatten` at `tolerance`, in whatever units the caller wants
/// error measured — callers should transform to final pixel space first so
/// a single pixel-space tolerance is meaningful) into one polygon per
/// closed subpath.
///
/// A closed subpath's `Z` sometimes re-visits the start point as a literal
/// coincident vertex; that's dropped so `triangulate_polygon`'s "implicit
/// closing edge" contract holds (a repeated vertex would otherwise become a
/// zero-area sliver triangle at the seam).
pub fn flatten_to_polygons(path: &BezPath, tolerance: f64) -> Vec<Vec<Point>> {
    let mut polygons = Vec::new();
    let mut current: Vec<Point> = Vec::new();

    kurbo::flatten(path.iter(), tolerance, |el| match el {
        PathEl::MoveTo(p) => {
            if current.len() >= 3 {
                polygons.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
            current.push(p);
        }
        PathEl::LineTo(p) => current.push(p),
        PathEl::ClosePath => {
            if current.len() > 1 {
                let first = current[0];
                let last = *current.last().unwrap();
                if first.distance_squared(last) < 1e-6 {
                    current.pop();
                }
            }
        }
        PathEl::QuadTo(..) | PathEl::CurveTo(..) => {
            unreachable!("kurbo::flatten only ever emits MoveTo/LineTo/ClosePath")
        }
    });

    if current.len() >= 3 {
        polygons.push(current);
    }

    polygons
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    const WHITE: [u8; 4] = [255, 255, 255, 255];

    // -- stroke_line_quad ----------------------------------------------

    #[test]
    fn horizontal_line_quad_has_corners_offset_vertically_by_half_width() {
        let quad = stroke_line_quad(0.0, 0.0, 10.0, 0.0, 2.0, WHITE);
        // Horizontal line: normal is vertical, so corners sit at y = ±1.
        for v in &quad {
            assert!((v.y.abs() - 1.0).abs() < 1e-5, "y={}", v.y);
        }
        let xs: Vec<f32> = quad.iter().map(|v| v.x).collect();
        assert!(xs.iter().any(|&x| (x - 0.0).abs() < 1e-5));
        assert!(xs.iter().any(|&x| (x - 10.0).abs() < 1e-5));
    }

    #[test]
    fn vertical_line_quad_has_corners_offset_horizontally_by_half_width() {
        let quad = stroke_line_quad(5.0, 0.0, 5.0, 20.0, 4.0, WHITE);
        for v in &quad {
            assert!((v.x - 5.0).abs() - 2.0 < 1e-5, "x={}", v.x);
        }
        let ys: Vec<f32> = quad.iter().map(|v| v.y).collect();
        assert!(ys.iter().any(|&y| (y - 0.0).abs() < 1e-5));
        assert!(ys.iter().any(|&y| (y - 20.0).abs() < 1e-5));
    }

    #[test]
    fn line_quad_covers_exactly_length_times_width_area() {
        // Shoelace area of the 6 emitted vertices (as two triangles) must
        // equal length * width regardless of line direction.
        let quad = stroke_line_quad(3.0, 4.0, 13.0, 4.0, 3.0, WHITE);
        let tri_area = |a: GeometryVertex, b: GeometryVertex, c: GeometryVertex| {
            0.5 * ((b.x - a.x) as f64 * (c.y - a.y) as f64 - (c.x - a.x) as f64 * (b.y - a.y) as f64).abs()
        };
        let total = tri_area(quad[0], quad[1], quad[2]) + tri_area(quad[3], quad[4], quad[5]);
        assert!((total - 30.0).abs() < 1e-3, "area = {total}");
    }

    #[test]
    fn zero_length_line_still_produces_a_finite_quad() {
        // Guards against NaN/div-by-zero — a zero-length engraving line is
        // unexpected input, but it must not poison the vertex stream.
        let quad = stroke_line_quad(5.0, 5.0, 5.0, 5.0, 2.0, WHITE);
        for v in &quad {
            assert!(v.x.is_finite() && v.y.is_finite());
        }
    }

    #[test]
    fn zero_length_line_draws_a_visible_dot_not_an_empty_quad() {
        // Extruding a zero-length segment along one normal yields a quad with
        // zero HEIGHT — finite, but zero-area, so the rasterizer drops it and
        // the anomalous input disappears silently. A `width` square makes it
        // visible instead, which is what the fallback is for.
        let quad = stroke_line_quad(5.0, 5.0, 5.0, 5.0, 2.0, WHITE);
        let tri_area = |a: GeometryVertex, b: GeometryVertex, c: GeometryVertex| {
            0.5 * ((b.x - a.x) as f64 * (c.y - a.y) as f64 - (c.x - a.x) as f64 * (b.y - a.y) as f64).abs()
        };
        let total = tri_area(quad[0], quad[1], quad[2]) + tri_area(quad[3], quad[4], quad[5]);
        assert!((total - 4.0).abs() < 1e-6, "expected a 2x2 dot, got area {total}");
        // …and it must be centered on the point, not offset to one side.
        for v in &quad {
            assert!((v.x - 5.0).abs() <= 1.0 + 1e-6 && (v.y - 5.0).abs() <= 1.0 + 1e-6);
        }
    }

    // -- triangulate_polygon ---------------------------------------------

    fn polygon_area(points: &[Point]) -> f64 {
        signed_area2(points).abs() / 2.0
    }

    fn triangles_area(tris: &[[Point; 3]]) -> f64 {
        tris.iter()
            .map(|[a, b, c]| ((b.x - a.x) * (c.y - a.y) - (c.x - a.x) * (b.y - a.y)).abs() / 2.0)
            .sum()
    }

    #[test]
    fn fewer_than_three_points_triangulates_to_nothing() {
        assert!(triangulate_polygon(&[]).is_empty());
        assert!(triangulate_polygon(&[Point::new(0.0, 0.0)]).is_empty());
        assert!(triangulate_polygon(&[Point::new(0.0, 0.0), Point::new(1.0, 1.0)]).is_empty());
    }

    #[test]
    fn convex_quad_triangulates_to_two_triangles_covering_the_same_area() {
        // A beam-shaped parallelogram (matches emit_beam's "M L L L Z").
        let quad = [
            Point::new(0.0, 0.0),
            Point::new(10.0, -1.0),
            Point::new(10.0, 1.0),
            Point::new(0.0, 2.0),
        ];
        let tris = triangulate_polygon(&quad);
        assert_eq!(tris.len(), 2, "n vertices should yield n-2 triangles");
        assert!((triangles_area(&tris) - polygon_area(&quad)).abs() < 1e-6);
    }

    #[test]
    fn regular_polygon_approximating_a_circle_triangulates_to_n_minus_2_triangles() {
        // Stand-in for a flattened repeat-dot circle: N points on a unit
        // circle. Convex, so this also exercises the "every ear is
        // findable" path for a larger N.
        let n = 24;
        let points: Vec<Point> = (0..n)
            .map(|i| {
                let t = 2.0 * PI * (i as f64) / (n as f64);
                Point::new(t.cos(), t.sin())
            })
            .collect();
        let tris = triangulate_polygon(&points);
        assert_eq!(tris.len(), n - 2);
        let expected_area = polygon_area(&points);
        assert!((triangles_area(&tris) - expected_area).abs() < 1e-6);
        // Sanity: a fine circle polygon's area should be close to pi*r^2 (r=1).
        assert!((expected_area - PI).abs() < 0.05);
    }

    #[test]
    fn crescent_shape_non_convex_triangulates_to_the_same_total_area() {
        // A hand-built crescent: outer arc bulges up by 4, inner arc bulges
        // up by 1 (same direction, shallower) — the actual topology
        // `emit_tie_or_slur` produces (both control points on the same
        // side), which is genuinely non-convex, unlike a lens/eye shape
        // (arcs bulging in opposite directions). Approximated here with a
        // handful of straight segments per arc, as if already flattened.
        let mut points = Vec::new();
        // Outer arc from (0,0) to (20,0), bulging up (positive y here
        // means "up" in a plain math sense for this test).
        for i in 0..=8 {
            let t = i as f64 / 8.0;
            let x = 20.0 * t;
            let y = 4.0 * 4.0 * t * (1.0 - t); // parabola peaking at 4
            points.push(Point::new(x, y));
        }
        // Inner arc back from (20,0) to (0,0), bulging up by only 1 (still
        // "up", i.e. into the outer arc's region — this is what makes the
        // shape a crescent, not a lens).
        for i in 1..8 {
            let t = 1.0 - (i as f64 / 8.0);
            let x = 20.0 * t;
            let y = 4.0 * 1.0 * t * (1.0 - t);
            points.push(Point::new(x, y));
        }

        let tris = triangulate_polygon(&points);
        assert_eq!(tris.len(), points.len() - 2);
        assert!((triangles_area(&tris) - polygon_area(&points)).abs() < 1e-6);
    }

    #[test]
    fn triangle_input_returns_itself_unchanged() {
        let tri = [Point::new(0.0, 0.0), Point::new(4.0, 0.0), Point::new(0.0, 4.0)];
        let tris = triangulate_polygon(&tri);
        assert_eq!(tris.len(), 1);
        assert_eq!(tris[0], tri);
    }

    #[test]
    fn collinear_points_triangulate_to_nothing() {
        let points = [
            Point::new(0.0, 0.0),
            Point::new(1.0, 0.0),
            Point::new(2.0, 0.0),
            Point::new(3.0, 0.0),
        ];
        assert!(triangulate_polygon(&points).is_empty());
    }

    // -- flatten_to_polygons + curve flattening tolerance -----------------

    /// A unit circle built from the standard 4-cubic-bezier approximation
    /// (kappa = 0.5522847498), closed with `Z`.
    fn unit_circle_bezpath() -> BezPath {
        const K: f64 = 0.5522847498;
        let mut p = BezPath::new();
        p.move_to((1.0, 0.0));
        p.curve_to((1.0, K), (K, 1.0), (0.0, 1.0));
        p.curve_to((-K, 1.0), (-1.0, K), (-1.0, 0.0));
        p.curve_to((-1.0, -K), (-K, -1.0), (0.0, -1.0));
        p.curve_to((K, -1.0), (1.0, -K), (1.0, 0.0));
        p.close_path();
        p
    }

    #[test]
    fn flatten_to_polygons_drops_the_closepath_duplicate_vertex() {
        // A closed square with an explicit final LineTo back to the start
        // (as ClosePath would imply) must not carry that point twice.
        let mut p = BezPath::new();
        p.move_to((0.0, 0.0));
        p.line_to((1.0, 0.0));
        p.line_to((1.0, 1.0));
        p.line_to((0.0, 1.0));
        p.close_path();

        let polys = flatten_to_polygons(&p, 0.25);
        assert_eq!(polys.len(), 1);
        assert_eq!(polys[0].len(), 4, "no duplicated closing vertex");
    }

    /// The core "curve flattening tolerance" claim: a tighter tolerance
    /// must flatten to more segments AND land closer to the true circle
    /// area (pi * r^2), not just "look different". This exercises the real
    /// `kurbo::flatten` + `triangulate_polygon` pipeline end to end, not a
    /// hand-built polygon standing in for one.
    #[test]
    fn tighter_tolerance_flattens_a_circle_closer_to_its_true_area() {
        let circle = unit_circle_bezpath();

        let loose = flatten_to_polygons(&circle, 1.0);
        let tight = flatten_to_polygons(&circle, 0.001);

        assert_eq!(loose.len(), 1);
        assert_eq!(tight.len(), 1);
        assert!(
            tight[0].len() > loose[0].len(),
            "tighter tolerance should need more segments: loose={} tight={}",
            loose[0].len(),
            tight[0].len(),
        );

        let true_area = PI; // unit circle, r = 1
        let loose_area = triangles_area(&triangulate_polygon(&loose[0]));
        let tight_area = triangulate_polygon(&tight[0]);
        let tight_area = triangles_area(&tight_area);

        let loose_err = (loose_area - true_area).abs();
        let tight_err = (tight_area - true_area).abs();

        assert!(
            tight_err < loose_err,
            "tight tolerance (err={tight_err}) should approximate the circle \
             at least as well as loose tolerance (err={loose_err})",
        );
        // The tight approximation should be visually indistinguishable from
        // the true circle for on-screen notation (repeat dots render at a
        // few px radius).
        assert!(tight_err < 0.02, "tight_area={tight_area} true_area={true_area}");
    }

    #[test]
    fn flatten_then_triangulate_a_circle_covers_the_right_area() {
        let circle = unit_circle_bezpath();
        let polys = flatten_to_polygons(&circle, 0.001);
        assert_eq!(polys.len(), 1);
        let tris = triangulate_polygon(&polys[0]);
        assert_eq!(tris.len(), polys[0].len() - 2);
        let area = triangles_area(&tris);
        assert!((area - PI).abs() < 0.02, "area={area}");
    }
}
