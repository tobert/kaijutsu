//! Pure camera framing solve for the heroic view (`docs/newui.md`,
//! "Framing is solved, not placed").
//!
//! World coordinates:
//! - Strike line ("now") lies along X at `y = 0, z = 0`, spanning
//!   `[-road_width * 0.5, +road_width * 0.5]`.
//! - The future road extends along `-Z`.
//! - Horizon lies at `y = 0, z -> -infinity`.

use bevy::prelude::*;
use kaijutsu_types::theme::HeroicFramingData;

/// Solve the camera `Transform` and `Projection` from `[heroic.framing]`
/// screen-fraction targets, total `road_width` in world units, viewport
/// `aspect` (`width / height`), and horizontal `orbit_angle` in radians.
///
/// Screen fractions (`horizon`, `strike_line`) are measured from the bottom of
/// the frame in `[0, 1]`. `strike_width` is the fraction of frame width the
/// strike line spans.
pub fn solve_camera(
    framing: &HeroicFramingData,
    road_width: f32,
    aspect: f32,
    orbit_angle: f32,
) -> (Transform, Projection) {
    let fov_y = framing.fov_degrees.clamp(15.0, 120.0).to_radians();
    let tan_half_vfov = (fov_y * 0.5).tan();
    let safe_aspect = aspect.max(0.1);
    let tan_half_hfov = safe_aspect * tan_half_vfov;

    // Pitch downward by `theta`: the horizon (direction (0, 0, -1)) sits at
    // NDC y = 2 * horizon - 1, so tan(theta) = (2 * horizon - 1) * tan(fov_y / 2).
    let ndc_horizon = 2.0 * framing.horizon - 1.0;
    let theta = (ndc_horizon * tan_half_vfov).atan();

    // Horizontal span at the strike line fixes camera-space depth `depth_s`:
    // half_width / (depth_s * tan_half_hfov) = strike_width
    let half_road = (road_width * 0.5).max(0.25);
    let target_strike_w = framing.strike_width.clamp(0.05, 0.98);
    let depth_s = half_road / (target_strike_w * tan_half_hfov);

    // Strike-line screen height fixes camera-space Y offset `up_s`:
    // up_s / (depth_s * tan_half_vfov) = 2 * strike_line - 1
    let ndc_strike = 2.0 * framing.strike_line - 1.0;
    let up_s = depth_s * tan_half_vfov * ndc_strike;

    // In camera-local space rotated by `Rx(-theta)` (pitch down by `theta`),
    // the strike line at world origin is at `cam_forward * depth_s + cam_up * up_s`.
    // Therefore camera world offset from origin (before orbit) is:
    //   pos = -(forward * depth_s + up * up_s)
    // where forward = (0, -sin(theta), -cos(theta)) and up = (0, cos(theta), -sin(theta)).
    let (sin_t, cos_t) = theta.sin_cos();
    let base_pos = Vec3::new(
        0.0,
        (depth_s * sin_t - up_s * cos_t).max(0.15),
        depth_s * cos_t + up_s * sin_t,
    );

    let clamped_orbit = orbit_angle.clamp(-framing.orbit_radians, framing.orbit_radians);
    let orbit_rot = Quat::from_rotation_y(clamped_orbit);
    let pos = orbit_rot * base_pos;
    let rot = orbit_rot * Quat::from_rotation_x(-theta);

    let transform = Transform::from_translation(pos).with_rotation(rot);
    let projection = Projection::Perspective(PerspectiveProjection {
        fov: fov_y,
        aspect_ratio: safe_aspect,
        near: 0.1,
        far: 400.0,
        ..default()
    });
    (transform, projection)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_bottom_fraction(vp: Mat4, p: Vec3) -> (f32, f32) {
        let clip = vp * p.extend(1.0);
        let ndc = clip.truncate() / clip.w;
        let x_frac = (ndc.x + 1.0) * 0.5;
        let y_from_bottom = (ndc.y + 1.0) * 0.5;
        (x_frac, y_from_bottom)
    }

    #[test]
    #[allow(deprecated)]
    fn framing_solve_places_horizon_and_strike_line_at_requested_fractions() {
        let framing = HeroicFramingData::default();
        let road_width = 4.0;
        let aspect = 16.0 / 9.0;
        let (tf, proj) = solve_camera(&framing, road_width, aspect, 0.0);

        let Projection::Perspective(persp) = proj else {
            panic!("expected perspective projection");
        };
        let proj_mat = Mat4::perspective_rh(persp.fov, aspect, persp.near, persp.far);
        let view_mat = tf.to_matrix().inverse();
        let vp = proj_mat * view_mat;

        // Far horizon point along -Z.
        let (_, horizon_y) = project_bottom_fraction(vp, Vec3::new(0.0, 0.0, -50_000.0));
        assert!(
            (horizon_y - framing.horizon).abs() < 1e-3,
            "horizon y={horizon_y}, expected {}",
            framing.horizon
        );

        // Strike line center and edges at z = 0.
        let (left_x, strike_y) = project_bottom_fraction(vp, Vec3::new(-road_width * 0.5, 0.0, 0.0));
        let (right_x, _) = project_bottom_fraction(vp, Vec3::new(road_width * 0.5, 0.0, 0.0));
        let span = right_x - left_x;
        assert!(
            (strike_y - framing.strike_line).abs() < 1e-3,
            "strike_line y={strike_y}, expected {}",
            framing.strike_line
        );
        assert!(
            (span - framing.strike_width).abs() < 1e-3,
            "strike_width={span}, expected {}",
            framing.strike_width
        );
    }

    #[test]
    #[allow(deprecated)]
    fn framing_solve_adjusts_when_horizon_changes() {
        let mut framing = HeroicFramingData::default();
        framing.horizon = 0.50;
        let (tf, proj) = solve_camera(&framing, 5.0, 1.5, 0.0);
        let Projection::Perspective(persp) = proj else {
            panic!("expected perspective");
        };
        let vp = Mat4::perspective_rh(persp.fov, 1.5, persp.near, persp.far)
            * tf.to_matrix().inverse();
        let (_, horizon_y) = project_bottom_fraction(vp, Vec3::new(0.0, 0.0, -50_000.0));
        assert!((horizon_y - 0.50).abs() < 1e-3);
    }
}
