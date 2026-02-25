//! Focus navigation for the 3D constellation.
//!
//! Focus changes apply Lorentz boosts that rigidly translate the hyperbolic space
//! through the camera — the camera stays put, the space moves. This provides
//! navigational stability.
//!
//! ## Systems
//!
//! - `update_focus_transform` — when Constellation.focus_id changes, compute
//!   a new LorentzTransform and trigger reprojection
//! - `interpolate_focus` — smooth animation of focus transitions via geodesic lerp
//! - `update_camera_orbit` — orbit yaw/pitch/distance for Shift+hjkl and +/-

use bevy::prelude::*;

use super::{
    Constellation,
    hyper::{HyperPoint, LorentzTransform, geodesic_lerp},
    render3d::ConstellationScene,
    viewport::ConstellationCamera3d,
};
use crate::ui::screen::Screen;

/// Resource for 3D focus animation state.
#[derive(Resource)]
pub struct FocusAnimation {
    /// The point we're focusing on (in original hyperboloid coordinates).
    pub target_hyper: HyperPoint,
    /// The current interpolation source point.
    pub source_hyper: HyperPoint,
    /// Interpolation progress: 0.0 = at source, 1.0 = at target.
    pub progress: f32,
    /// Interpolation speed (higher = faster).
    pub speed: f32,
    /// Whether we're currently animating.
    pub animating: bool,
    /// Last known focus_id to detect changes.
    pub last_focus_id: Option<String>,
}

impl Default for FocusAnimation {
    fn default() -> Self {
        Self {
            target_hyper: HyperPoint::ORIGIN,
            source_hyper: HyperPoint::ORIGIN,
            progress: 1.0,
            speed: 5.0,
            animating: false,
            last_focus_id: None,
        }
    }
}

/// Resource for camera orbit state.
#[derive(Resource)]
pub struct CameraOrbit {
    pub yaw: f32,
    pub pitch: f32,
    pub distance: f32,
    pub target_yaw: f32,
    pub target_pitch: f32,
    pub target_distance: f32,
    pub speed: f32,
}

impl Default for CameraOrbit {
    fn default() -> Self {
        Self {
            yaw: 0.0,
            pitch: 0.0,
            distance: 3.0,
            target_yaw: 0.0,
            target_pitch: 0.0,
            target_distance: 3.0,
            speed: 6.0,
        }
    }
}

impl CameraOrbit {
    /// Reset to default view.
    pub fn reset(&mut self) {
        self.target_yaw = 0.0;
        self.target_pitch = 0.0;
        self.target_distance = 3.0;
    }
}

/// Register navigation systems.
pub fn setup_navigation_systems(app: &mut App) {
    app.init_resource::<FocusAnimation>()
        .init_resource::<CameraOrbit>()
        .add_systems(
            Update,
            (
                update_focus_transform,
                interpolate_focus,
                update_camera_orbit,
            )
                .chain(),
        );
}

/// Detect focus_id changes and start a new focus animation.
fn update_focus_transform(
    constellation: Res<Constellation>,
    scene: Res<ConstellationScene>,
    mut anim: ResMut<FocusAnimation>,
) {
    let current_focus = constellation.focus_id.as_deref();
    let last_focus = anim.last_focus_id.as_deref();

    if current_focus == last_focus {
        return;
    }

    anim.last_focus_id = current_focus.map(|s| s.to_string());

    // Find the new focus node's hyper_pos in the layout
    let Some(focus_id) = current_focus else {
        return;
    };

    let Some(idx) = constellation
        .nodes
        .iter()
        .position(|n| n.context_id == focus_id)
    else {
        return;
    };

    if idx >= scene.layout.nodes.len() {
        return;
    }

    let target_hyper = scene.layout.nodes[idx].hyper_pos;

    // Start animation from current position to new target
    anim.source_hyper = if anim.animating {
        // If already animating, use current interpolated position
        let t = anim.progress as f64;
        let partial_boost = geodesic_lerp(&anim.target_hyper, t);
        let inv = partial_boost.inverse();
        inv.apply(&HyperPoint::ORIGIN) // Current "virtual" origin in original space
    } else {
        anim.target_hyper // Previous focus (already there)
    };

    anim.target_hyper = target_hyper;
    anim.progress = 0.0;
    anim.animating = true;
}

/// Smoothly interpolate focus animation and reproject all nodes.
fn interpolate_focus(
    mut anim: ResMut<FocusAnimation>,
    mut scene: ResMut<ConstellationScene>,
    screen: Res<State<Screen>>,
    time: Res<Time>,
) {
    if !matches!(screen.get(), Screen::Constellation) || !anim.animating {
        return;
    }

    let dt = time.delta_secs();
    anim.progress = (anim.progress + anim.speed * dt).min(1.0);

    // Compute the focus transform for the current interpolation state
    // We need the boost that maps the interpolated focus point to the origin
    let target_boost = LorentzTransform::boost_to_origin(&anim.target_hyper);

    if anim.progress >= 0.999 {
        // Animation complete — snap to final position
        anim.progress = 1.0;
        anim.animating = false;
        scene.focus_transform = target_boost;
    } else {
        // Intermediate: compute partial boost along geodesic
        // The geodesic_lerp function gives us the boost for a point that is
        // fraction t of the way from origin to target
        let partial = geodesic_lerp(&anim.target_hyper, anim.progress as f64);
        scene.focus_transform = partial;
    }

    // Reproject all ball positions
    scene.ball_positions = scene.layout.project_all(&scene.focus_transform);
}

/// Smoothly interpolate camera orbit and update the 3D camera transform.
fn update_camera_orbit(
    mut orbit: ResMut<CameraOrbit>,
    screen: Res<State<Screen>>,
    time: Res<Time>,
    mut cameras: Query<&mut Transform, With<ConstellationCamera3d>>,
) {
    if !matches!(screen.get(), Screen::Constellation) {
        return;
    }

    let dt = time.delta_secs();
    let t = (orbit.speed * dt).min(1.0);

    // Interpolate orbit params
    let yaw_diff = orbit.target_yaw - orbit.yaw;
    if yaw_diff.abs() > 0.001 {
        orbit.yaw += yaw_diff * t;
    } else {
        orbit.yaw = orbit.target_yaw;
    }

    let pitch_diff = orbit.target_pitch - orbit.pitch;
    if pitch_diff.abs() > 0.001 {
        orbit.pitch += pitch_diff * t;
    } else {
        orbit.pitch = orbit.target_pitch;
    }

    let dist_diff = orbit.target_distance - orbit.distance;
    if dist_diff.abs() > 0.001 {
        orbit.distance += dist_diff * t;
    } else {
        orbit.distance = orbit.target_distance;
    }

    // Compute camera position from spherical coordinates
    let cos_pitch = orbit.pitch.cos();
    let sin_pitch = orbit.pitch.sin();
    let cos_yaw = orbit.yaw.cos();
    let sin_yaw = orbit.yaw.sin();

    let cam_pos = Vec3::new(
        orbit.distance * cos_pitch * sin_yaw,
        orbit.distance * sin_pitch,
        orbit.distance * cos_pitch * cos_yaw,
    );

    for mut transform in cameras.iter_mut() {
        *transform = Transform::from_translation(cam_pos).looking_at(Vec3::ZERO, Vec3::Y);
    }
}

/// Find the nearest constellation node in a given 2D direction, using 3D ball positions.
///
/// Projects the 2D input direction into the camera's view plane, then scores
/// nodes by direction alignment and distance in ball space.
pub fn find_nearest_in_direction_3d(
    constellation: &Constellation,
    scene: &ConstellationScene,
    direction: Vec2,
) -> Option<String> {
    if scene.ball_positions.len() != constellation.nodes.len() {
        return None;
    }

    let focus_idx = constellation.focus_id.as_ref().and_then(|id| {
        constellation.nodes.iter().position(|n| &n.context_id == id)
    })?;

    let focus_ball = scene.ball_positions[focus_idx];

    // Convert 2D direction to 3D view-plane direction
    // Assume camera is looking along -Z, so screen-X = world-X, screen-Y = world-Y
    let dir_3d = Vec3::new(direction.x, direction.y, 0.0).normalize_or_zero();

    let mut best: Option<(f32, &str)> = None;

    for (i, node) in constellation.nodes.iter().enumerate() {
        if i == focus_idx {
            continue;
        }

        let delta = scene.ball_positions[i] - focus_ball;
        let dist = delta.length();
        if dist < 0.001 {
            continue;
        }

        // Project delta onto the view plane (XY) for direction comparison
        let delta_2d = Vec3::new(delta.x, delta.y, 0.0);
        let delta_2d_len = delta_2d.length();
        if delta_2d_len < 0.001 {
            continue;
        }

        let cos_angle = delta_2d.dot(dir_3d) / delta_2d_len;
        if cos_angle <= 0.0 {
            continue; // Wrong half-plane
        }

        // Score: closer and more aligned is better
        let score = dist / cos_angle.max(0.01);

        if best.is_none() || score < best.unwrap().0 {
            best = Some((score, &node.context_id));
        }
    }

    best.map(|(_, id)| id.to_string())
}
