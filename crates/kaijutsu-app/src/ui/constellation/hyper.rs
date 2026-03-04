//! Hyperbolic geometry math library for Poincaré ball constellation layout.
// Math library: not all functions are called by current rendering code.
#![allow(dead_code)]
//!
//! All internal computation uses f64 for numerical stability. The f64→f32
//! boundary is crossed only at `HyperPoint::to_ball_f32()` for rendering.
//!
//! ## Coordinate system
//!
//! Points live on the upper sheet of the hyperboloid in 4D Minkowski space:
//!
//! ```text
//! p = (t, x, y, z)  where  -t² + x² + y² + z² = -1,  t > 0
//! ```
//!
//! The Minkowski inner product has signature (-,+,+,+).
//!
//! ## Key operations
//!
//! - **Distance**: `acosh(-⟨p, q⟩_M)`
//! - **Projection**: hyperboloid → Poincaré ball via `(x,y,z) / (1+t)`
//! - **Focus change**: Lorentz boost maps any point to origin (rigid transform)
//! - **Animation**: geodesic lerp along the unique geodesic between two points

use bevy::math::{DVec3, Vec3};
use std::f64::consts::PI;

// ============================================================================
// HyperPoint
// ============================================================================

/// A point on the hyperboloid in 4D Minkowski space.
///
/// Invariant: `-t² + x² + y² + z² = -1, t > 0`
#[derive(Clone, Copy, Debug)]
pub struct HyperPoint {
    pub t: f64,
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl HyperPoint {
    /// The origin of hyperbolic space: the "north pole" of the hyperboloid.
    pub const ORIGIN: Self = Self {
        t: 1.0,
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };

    /// Construct a point and assert the hyperboloid constraint in debug builds.
    pub fn new(t: f64, x: f64, y: f64, z: f64) -> Self {
        let p = Self { t, x, y, z };
        debug_assert!(
            (p.minkowski_norm() + 1.0).abs() < 1e-6,
            "HyperPoint violates hyperboloid constraint: norm = {} (expected -1)",
            p.minkowski_norm()
        );
        debug_assert!(t > 0.0, "HyperPoint must be on upper sheet (t > 0)");
        p
    }

    /// Place a point at a given hyperbolic distance from the origin along a
    /// direction in the spatial (x,y,z) subspace.
    ///
    /// `direction` is normalized internally; only its direction matters.
    pub fn from_direction_and_distance(direction: DVec3, distance: f64) -> Self {
        if distance.abs() < 1e-15 {
            return Self::ORIGIN;
        }
        let dir = direction.normalize();
        let t = distance.cosh();
        let s = distance.sinh();
        Self::new(t, dir.x * s, dir.y * s, dir.z * s)
    }

    /// Minkowski inner product: ⟨p, q⟩_M = -t·t + x·x + y·y + z·z
    pub fn minkowski_dot(&self, other: &Self) -> f64 {
        -self.t * other.t + self.x * other.x + self.y * other.y + self.z * other.z
    }

    /// Minkowski norm: ⟨p, p⟩_M (should be -1 for valid hyperboloid points).
    fn minkowski_norm(&self) -> f64 {
        self.minkowski_dot(self)
    }

    /// Hyperbolic distance to another point.
    pub fn distance(&self, other: &Self) -> f64 {
        let dot = -self.minkowski_dot(other);
        // Clamp for numerical safety — dot should be >= 1.0 on the hyperboloid
        dot.max(1.0).acosh()
    }

    /// Fix floating-point drift: recompute `t` from the spatial components
    /// so the hyperboloid constraint is restored.
    pub fn renormalize(&mut self) {
        let spatial_sq = self.x * self.x + self.y * self.y + self.z * self.z;
        self.t = (1.0 + spatial_sq).sqrt();
    }

    /// Project to the Poincaré ball (f64).
    ///
    /// `project(t, x, y, z) = (x, y, z) / (1 + t)`
    pub fn to_ball(&self) -> DVec3 {
        let denom = 1.0 + self.t;
        DVec3::new(self.x / denom, self.y / denom, self.z / denom)
    }

    /// Project to the Poincaré ball and cast to f32 for rendering.
    ///
    /// This is the f64→f32 boundary.
    pub fn to_ball_f32(&self) -> Vec3 {
        let b = self.to_ball();
        Vec3::new(b.x as f32, b.y as f32, b.z as f32)
    }

    /// Inverse projection: Poincaré ball → hyperboloid.
    pub fn from_ball(b: DVec3) -> Self {
        let r_sq = b.length_squared();
        if r_sq >= 1.0 {
            // On or outside the ball boundary — clamp to a very large distance
            let dir = b.normalize_or_zero();
            return Self::from_direction_and_distance(dir, 10.0);
        }
        let denom = 1.0 - r_sq;
        let t = (1.0 + r_sq) / denom;
        let scale = 2.0 / denom;
        Self {
            t,
            x: b.x * scale,
            y: b.y * scale,
            z: b.z * scale,
        }
    }

    /// The spatial part as a DVec3.
    pub fn spatial(&self) -> DVec3 {
        DVec3::new(self.x, self.y, self.z)
    }
}

impl Default for HyperPoint {
    fn default() -> Self {
        Self::ORIGIN
    }
}

impl PartialEq for HyperPoint {
    fn eq(&self, other: &Self) -> bool {
        (self.t - other.t).abs() < 1e-10
            && (self.x - other.x).abs() < 1e-10
            && (self.y - other.y).abs() < 1e-10
            && (self.z - other.z).abs() < 1e-10
    }
}

// ============================================================================
// LorentzTransform
// ============================================================================

/// A Lorentz transformation stored as a column-major 4×4 matrix of f64.
///
/// Newtype prevents accidental use of Euclidean `Mat4` operations
/// (e.g. `transform_point3` does perspective division, wrong for Minkowski space).
///
/// Strategy: always recompute from scratch (no composition) to avoid
/// Thomas rotation drift.
#[derive(Clone, Debug)]
pub struct LorentzTransform {
    /// Column-major 4×4 matrix: `m[col * 4 + row]`
    m: [f64; 16],
}

impl LorentzTransform {
    /// The identity transform.
    pub const IDENTITY: Self = Self {
        m: [
            1.0, 0.0, 0.0, 0.0, // col 0
            0.0, 1.0, 0.0, 0.0, // col 1
            0.0, 0.0, 1.0, 0.0, // col 2
            0.0, 0.0, 0.0, 1.0, // col 3
        ],
    };

    /// Access element at (row, col).
    #[inline]
    fn get(&self, row: usize, col: usize) -> f64 {
        self.m[col * 4 + row]
    }

    /// Set element at (row, col).
    #[inline]
    fn set(&mut self, row: usize, col: usize, val: f64) {
        self.m[col * 4 + row] = val;
    }

    /// Construct the Lorentz boost that maps `point` to the origin.
    ///
    /// This is the focus operation: after applying this transform, the given
    /// point will be at `HyperPoint::ORIGIN`.
    ///
    /// The construction rotates the spatial direction of `point` to align with
    /// the x-axis, applies a 1D boost, then rotates back. But the composition
    /// is done analytically to produce a single 4×4 matrix.
    ///
    /// For the origin, returns identity.
    pub fn boost_to_origin(point: &HyperPoint) -> Self {
        let spatial = point.spatial();
        let spatial_len = spatial.length();

        if spatial_len < 1e-14 {
            // Point is (effectively) at the origin already
            return Self::IDENTITY;
        }

        // Hyperbolic distance from origin to point
        let dist = point.distance(&HyperPoint::ORIGIN);
        let c = dist.cosh(); // = point.t (ideally)
        let s = dist.sinh(); // = spatial_len (ideally)

        // Unit direction in spatial subspace
        let d = spatial / spatial_len;

        // The boost that maps `point` → origin is along `-d` by `dist`.
        // General formula for a Lorentz boost along unit direction (dx, dy, dz):
        //
        //   B = I + (c-1)*d⊗d_spatial + terms involving sinh/cosh
        //
        // Specifically for mapping p → origin:
        //
        //   B[0,0] = cosh(dist)         (= point.t)
        //   B[0,i] = -sinh(dist) * d[i] (= -spatial[i])
        //   B[i,0] = -sinh(dist) * d[i]
        //   B[i,j] = δ[i,j] + (cosh(dist) - 1) * d[i] * d[j]
        //
        // To map p → origin we boost in the -d direction.
        // Check: B·p should give (_, 0, 0, 0) with _ > 0.
        //
        // Row 0 of B·p: c*t - s*(dx*x + dy*y + dz*z) = c*t - s*|spatial| = c*t - s²
        //   Since t = cosh(dist) = c and s = sinh(dist):  c² - s² = 1  ✓

        let mut b = Self::IDENTITY;

        // Row 0, Col 0: t component
        b.set(0, 0, c);

        // Row 0, Cols 1-3 and Rows 1-3, Col 0: mixed temporal-spatial
        // To map p → origin, we need the signs such that B·p = (1,0,0,0).
        // B[0,j] = -s * d[j-1] for j=1,2,3
        // B[i,0] = -s * d[i-1] for i=1,2,3
        let dirs = [d.x, d.y, d.z];
        for i in 0..3 {
            b.set(0, i + 1, -s * dirs[i]);
            b.set(i + 1, 0, -s * dirs[i]);
        }

        // Rows/Cols 1-3: spatial block
        // B[i,j] = δ[i,j] + (c - 1) * d[i] * d[j]
        for i in 0..3 {
            for j in 0..3 {
                let delta = if i == j { 1.0 } else { 0.0 };
                b.set(i + 1, j + 1, delta + (c - 1.0) * dirs[i] * dirs[j]);
            }
        }

        b
    }

    /// Apply this transform to a hyperboloid point and renormalize.
    pub fn apply(&self, point: &HyperPoint) -> HyperPoint {
        let v = [point.t, point.x, point.y, point.z];
        let mut result = [0.0f64; 4];

        for row in 0..4 {
            for col in 0..4 {
                result[row] += self.get(row, col) * v[col];
            }
        }

        let mut p = HyperPoint {
            t: result[0],
            x: result[1],
            y: result[2],
            z: result[3],
        };
        p.renormalize();
        p
    }

    /// Apply this transform to every point in a slice.
    pub fn apply_batch(&self, points: &mut [HyperPoint]) {
        for p in points.iter_mut() {
            *p = self.apply(p);
        }
    }

    /// Compute the inverse transform.
    ///
    /// For a Lorentz boost, the inverse negates the rapidity (flip the sign of
    /// the temporal-spatial cross terms). Equivalently, transpose the matrix
    /// with Minkowski metric adjustment: `B_inv[i,j] = η·B^T·η` where
    /// `η = diag(-1,1,1,1)`.
    pub fn inverse(&self) -> Self {
        let mut inv = Self::IDENTITY;
        // For a proper Lorentz transform L, the Minkowski-transpose inverse is:
        // L^{-1}[i,j] = η[i,i] * L[j,i] * η[j,j]
        // where η = diag(-1, 1, 1, 1)
        let eta = [-1.0, 1.0, 1.0, 1.0];
        for i in 0..4 {
            for j in 0..4 {
                inv.set(i, j, eta[i] * self.get(j, i) * eta[j]);
            }
        }
        inv
    }
}

impl Default for LorentzTransform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

// ============================================================================
// Standalone functions
// ============================================================================

/// Area of a hyperbolic disc of radius r (curvature K = -1).
///
/// `A = 2π(cosh(r) - 1)`
pub fn hyper_disc_area(r: f64) -> f64 {
    2.0 * PI * (r.cosh() - 1.0)
}

/// Inverse: radius of a hyperbolic disc with area A.
///
/// `r = acosh(1 + A/(2π))`
pub fn hyper_radius_from_area(area: f64) -> f64 {
    (1.0 + area / (2.0 * PI)).acosh()
}

/// Circumference of a band at polar angle θ on a hemisphere of radius R.
///
/// `C = 2π · sinh(R) · sin(θ)`
pub fn hyper_band_circumference(r: f64, theta: f64) -> f64 {
    2.0 * PI * r.sinh() * theta.sin()
}

/// Compute the Lorentz transform that interpolates between identity and
/// `boost_to_origin(target)` at parameter `t ∈ [0, 1]`.
///
/// At t=0: identity (no movement).
/// At t=1: full boost to target.
///
/// Interpolation is along the geodesic: we compute the boost for a point
/// that is fraction `t` of the distance along the geodesic from origin to target.
pub fn geodesic_lerp(target: &HyperPoint, t: f64) -> LorentzTransform {
    let t = t.clamp(0.0, 1.0);
    if t < 1e-12 {
        return LorentzTransform::IDENTITY;
    }

    let dist = HyperPoint::ORIGIN.distance(target);
    if dist < 1e-14 {
        return LorentzTransform::IDENTITY;
    }

    // Intermediate point at fraction t along the geodesic from origin to target
    let partial_dist = dist * t;
    let direction = target.spatial();
    let dir_len = direction.length();
    if dir_len < 1e-14 {
        return LorentzTransform::IDENTITY;
    }

    let intermediate = HyperPoint::from_direction_and_distance(
        direction / dir_len,
        partial_dist,
    );

    LorentzTransform::boost_to_origin(&intermediate)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    const EPSILON: f64 = 1e-10;

    fn approx_eq(a: f64, b: f64) -> bool {
        (a - b).abs() < EPSILON
    }

    fn approx_eq_point(a: &HyperPoint, b: &HyperPoint) -> bool {
        approx_eq(a.t, b.t)
            && approx_eq(a.x, b.x)
            && approx_eq(a.y, b.y)
            && approx_eq(a.z, b.z)
    }

    fn approx_eq_dvec3(a: DVec3, b: DVec3) -> bool {
        (a - b).length() < EPSILON
    }

    #[test]
    fn origin_is_valid() {
        let o = HyperPoint::ORIGIN;
        assert!(approx_eq(o.minkowski_dot(&o), -1.0));
    }

    #[test]
    fn ball_projection_round_trip() {
        // Test several points at various distances
        for dist in [0.1, 0.5, 1.0, 2.0, 5.0] {
            for dir in [
                DVec3::X,
                DVec3::Y,
                DVec3::Z,
                DVec3::new(1.0, 1.0, 0.0).normalize(),
                DVec3::new(1.0, 1.0, 1.0).normalize(),
            ] {
                let p = HyperPoint::from_direction_and_distance(dir, dist);
                let ball = p.to_ball();
                let recovered = HyperPoint::from_ball(ball);
                assert!(
                    approx_eq_point(&p, &recovered),
                    "Round-trip failed for dist={dist}, dir={dir:?}: \
                     original=({:.6},{:.6},{:.6},{:.6}), \
                     recovered=({:.6},{:.6},{:.6},{:.6})",
                    p.t, p.x, p.y, p.z,
                    recovered.t, recovered.x, recovered.y, recovered.z,
                );
            }
        }
    }

    #[test]
    fn origin_projects_to_ball_center() {
        let b = HyperPoint::ORIGIN.to_ball();
        assert!(approx_eq_dvec3(b, DVec3::ZERO));
    }

    #[test]
    fn distance_known_value() {
        // Point at distance 1.0 along x-axis
        let p = HyperPoint::from_direction_and_distance(DVec3::X, 1.0);
        assert!(
            approx_eq(p.t, 1.0_f64.cosh()),
            "t={}, expected cosh(1)={}",
            p.t,
            1.0_f64.cosh()
        );
        let d = HyperPoint::ORIGIN.distance(&p);
        assert!(
            approx_eq(d, 1.0),
            "distance={d}, expected 1.0"
        );
    }

    #[test]
    fn distance_symmetry() {
        let p = HyperPoint::from_direction_and_distance(DVec3::new(1.0, 2.0, 3.0).normalize(), 2.5);
        let q = HyperPoint::from_direction_and_distance(DVec3::new(-1.0, 0.5, 0.0).normalize(), 1.8);
        assert!(approx_eq(p.distance(&q), q.distance(&p)));
    }

    #[test]
    fn distance_self_is_zero() {
        let p = HyperPoint::from_direction_and_distance(DVec3::Y, 3.0);
        assert!(approx_eq(p.distance(&p), 0.0));
    }

    #[test]
    fn boost_to_origin_sends_point_to_origin() {
        let p = HyperPoint::from_direction_and_distance(
            DVec3::new(1.0, 2.0, -1.0).normalize(),
            2.0,
        );
        let boost = LorentzTransform::boost_to_origin(&p);
        let result = boost.apply(&p);
        assert!(
            approx_eq_point(&result, &HyperPoint::ORIGIN),
            "boost(p) should be origin, got ({:.6},{:.6},{:.6},{:.6})",
            result.t, result.x, result.y, result.z,
        );
    }

    #[test]
    fn boost_inverse_round_trip() {
        let p = HyperPoint::from_direction_and_distance(DVec3::Z, 1.5);
        let boost = LorentzTransform::boost_to_origin(&p);
        let boosted = boost.apply(&p);

        // Boost to origin
        assert!(approx_eq_point(&boosted, &HyperPoint::ORIGIN));

        // Inverse should recover the original
        let inv = boost.inverse();
        let recovered = inv.apply(&boosted);
        assert!(
            approx_eq_point(&recovered, &p),
            "inverse failed: expected ({:.6},{:.6},{:.6},{:.6}), got ({:.6},{:.6},{:.6},{:.6})",
            p.t, p.x, p.y, p.z,
            recovered.t, recovered.x, recovered.y, recovered.z,
        );
    }

    #[test]
    fn boost_preserves_other_points() {
        // Boosting should be a rigid transform — preserve distances
        let focus = HyperPoint::from_direction_and_distance(DVec3::X, 1.0);
        let other = HyperPoint::from_direction_and_distance(DVec3::Y, 1.0);

        let boost = LorentzTransform::boost_to_origin(&focus);
        let boosted_focus = boost.apply(&focus);
        let boosted_other = boost.apply(&other);

        // Distance between the two should be preserved
        let d_before = focus.distance(&other);
        let d_after = boosted_focus.distance(&boosted_other);
        assert!(
            approx_eq(d_before, d_after),
            "distance not preserved: before={d_before}, after={d_after}"
        );
    }

    #[test]
    fn renormalize_fixes_drift() {
        let mut p = HyperPoint::from_direction_and_distance(DVec3::X, 2.0);
        // Add noise to t
        p.t += 0.01;
        // Constraint is violated
        assert!((p.minkowski_dot(&p) + 1.0).abs() > 1e-6);

        p.renormalize();
        // Constraint restored
        assert!(
            (p.minkowski_dot(&p) + 1.0).abs() < 1e-10,
            "renormalize failed: norm = {}",
            p.minkowski_dot(&p),
        );
    }

    #[test]
    fn geodesic_lerp_endpoints() {
        let target = HyperPoint::from_direction_and_distance(
            DVec3::new(1.0, 1.0, 0.0).normalize(),
            3.0,
        );

        // t=0: identity (origin stays at origin)
        let t0 = geodesic_lerp(&target, 0.0);
        let result0 = t0.apply(&HyperPoint::ORIGIN);
        assert!(approx_eq_point(&result0, &HyperPoint::ORIGIN));

        // t=1: full boost (target → origin)
        let t1 = geodesic_lerp(&target, 1.0);
        let result1 = t1.apply(&target);
        assert!(
            approx_eq_point(&result1, &HyperPoint::ORIGIN),
            "geodesic_lerp(1.0) should map target to origin, got ({:.6},{:.6},{:.6},{:.6})",
            result1.t, result1.x, result1.y, result1.z,
        );
    }

    #[test]
    fn geodesic_lerp_midpoint() {
        let target = HyperPoint::from_direction_and_distance(DVec3::X, 2.0);
        let half = geodesic_lerp(&target, 0.5);

        // The intermediate boost should move the origin to the halfway point
        let inv = half.inverse();
        let mid = inv.apply(&HyperPoint::ORIGIN);
        let d_mid = HyperPoint::ORIGIN.distance(&mid);

        // Halfway point should be at distance 1.0 (half of 2.0)
        assert!(
            approx_eq(d_mid, 1.0),
            "midpoint distance = {d_mid}, expected 1.0"
        );
    }

    #[test]
    fn area_radius_round_trip() {
        for r in [0.1, 0.5, 1.0, 2.0, 5.0, 10.0] {
            let area = hyper_disc_area(r);
            let recovered_r = hyper_radius_from_area(area);
            assert!(
                approx_eq(r, recovered_r),
                "round-trip failed: r={r}, area={area}, recovered={recovered_r}"
            );
        }
    }

    #[test]
    fn ball_radius_is_tanh_half_distance() {
        for dist in [0.5, 1.0, 2.0, 4.0] {
            let p = HyperPoint::from_direction_and_distance(DVec3::X, dist);
            let ball_r = p.to_ball().length();
            let expected = (dist / 2.0).tanh();
            assert!(
                (ball_r - expected).abs() < 1e-10,
                "dist={dist}: ball_r={ball_r}, expected tanh({})={expected}",
                dist / 2.0,
            );
        }
    }

    #[test]
    fn batch_apply_matches_individual() {
        let boost = LorentzTransform::boost_to_origin(
            &HyperPoint::from_direction_and_distance(DVec3::Y, 1.5),
        );

        let points: Vec<HyperPoint> = (0..5)
            .map(|i| {
                let angle = i as f64 * 0.7;
                HyperPoint::from_direction_and_distance(
                    DVec3::new(angle.cos(), angle.sin(), 0.3),
                    0.5 + i as f64 * 0.3,
                )
            })
            .collect();

        let individual: Vec<HyperPoint> = points.iter().map(|p| boost.apply(p)).collect();

        let mut batch = points;
        boost.apply_batch(&mut batch);

        for (i, (ind, bat)) in individual.iter().zip(batch.iter()).enumerate() {
            assert!(
                approx_eq_point(ind, bat),
                "mismatch at index {i}"
            );
        }
    }

    #[test]
    fn identity_boost_origin() {
        let boost = LorentzTransform::boost_to_origin(&HyperPoint::ORIGIN);
        // Should be identity
        for i in 0..4 {
            for j in 0..4 {
                let expected = if i == j { 1.0 } else { 0.0 };
                assert!(
                    approx_eq(boost.get(i, j), expected),
                    "identity check failed at ({i},{j}): {} vs {expected}",
                    boost.get(i, j),
                );
            }
        }
    }

    #[test]
    fn band_circumference_positive() {
        let c = hyper_band_circumference(2.0, PI / 4.0);
        assert!(c > 0.0, "circumference should be positive: {c}");
    }

    #[test]
    fn from_ball_origin_is_hyperboloid_origin() {
        let p = HyperPoint::from_ball(DVec3::ZERO);
        assert!(approx_eq_point(&p, &HyperPoint::ORIGIN));
    }

    #[test]
    fn boost_large_distance() {
        // Verify stability at large distances
        let p = HyperPoint::from_direction_and_distance(DVec3::X, 10.0);
        let boost = LorentzTransform::boost_to_origin(&p);
        let result = boost.apply(&p);
        assert!(
            result.distance(&HyperPoint::ORIGIN) < 1e-6,
            "large-distance boost failed: distance to origin = {}",
            result.distance(&HyperPoint::ORIGIN),
        );
    }

    #[test]
    fn to_ball_f32_matches_to_ball() {
        let p = HyperPoint::from_direction_and_distance(DVec3::new(1.0, 2.0, 3.0).normalize(), 1.5);
        let ball64 = p.to_ball();
        let ball32 = p.to_ball_f32();
        assert!((ball64.x as f32 - ball32.x).abs() < 1e-6);
        assert!((ball64.y as f32 - ball32.y).abs() < 1e-6);
        assert!((ball64.z as f32 - ball32.z).abs() < 1e-6);
    }
}
