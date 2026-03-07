//! Focus navigation for the constellation.
//!
//! Handles camera orbit controls for Shift+hjkl pan and +/- zoom.

use bevy::prelude::*;

/// Resource for camera orbit state (used for pan/zoom controls).
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
    app.init_resource::<CameraOrbit>();
}
