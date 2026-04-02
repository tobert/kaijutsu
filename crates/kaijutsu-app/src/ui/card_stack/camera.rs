//! Camera3d for the Conversation Stack view.
//!
//! Spawned on `OnEnter(ConversationStack)`, despawned on `OnExit`.
//! This avoids the Camera3d interfering with GpuImage preparation
//! for the 2D MSDF rendering pipeline when the stack view isn't active.

use bevy::prelude::*;

use crate::ui::theme::Theme;

/// Marker component for the stack view's 3D camera.
#[derive(Component, Reflect, Debug, Default)]
#[reflect(Component)]
pub struct StackCameraTag;

/// Marker for the stack view's light.
#[derive(Component)]
pub struct StackLight;

/// Spawn the 3D camera and light when entering the stack view.
pub fn spawn_stack_camera(mut commands: Commands, theme: Res<Theme>) {
    commands.spawn((
        StackCameraTag,
        Camera3d::default(),
        Camera {
            clear_color: ClearColorConfig::Custom(darken(theme.bg, 0.5)),
            order: 2,
            ..default()
        },
        // Closer to origin, slight downward angle for depth perception
        Transform::from_xyz(0.0, 20.0, 180.0).looking_at(Vec3::new(0.0, -5.0, 0.0), Vec3::Y),
    ));

    commands.spawn((
        StackLight,
        DirectionalLight {
            illuminance: 5000.0,
            shadows_enabled: false,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(
            EulerRot::XYZ,
            -0.4,
            0.3,
            0.0,
        )),
    ));
}

/// Despawn the 3D camera and light when leaving the stack view.
pub fn despawn_stack_camera(
    mut commands: Commands,
    cameras: Query<Entity, With<StackCameraTag>>,
    lights: Query<Entity, With<StackLight>>,
) {
    for entity in cameras.iter() {
        commands.entity(entity).despawn();
    }
    for entity in lights.iter() {
        commands.entity(entity).despawn();
    }
}

/// Darken a color by a factor (0.0 = black, 1.0 = unchanged).
fn darken(color: Color, factor: f32) -> Color {
    let lin = color.to_linear();
    Color::LinearRgba(LinearRgba::new(
        lin.red * factor,
        lin.green * factor,
        lin.blue * factor,
        1.0,
    ))
}
