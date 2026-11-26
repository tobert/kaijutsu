use bevy::prelude::*;

pub(super) const CAMERA_DISTANCE: f32 = 16.0;
pub(super) const STACK_DEPTH: usize = 3;
pub(super) const GROUPS: usize = 3;

fn frame_height(aspect: f32) -> f32 {
    7.2_f32.max(13.0 / aspect)
}

pub(super) fn fov(aspect: f32) -> f32 {
    2.0 * (frame_height(aspect) / (2.0 * CAMERA_DISTANCE)).atan()
}

pub(super) fn pose(group: usize, rank: usize, aspect: f32) -> Transform {
    if group == 0 {
        return Transform::from_xyz(-3.19, 0.0, 0.0)
            .with_rotation(Quat::from_rotation_y(12_f32.to_radians()))
            .with_scale(Vec3::new(5.72, 5.72, 1.0));
    }

    let rank = rank as f32;
    let height = 2.34 + (frame_height(aspect) - 7.2).min(4.6) * 0.29;
    let y = if group == 1 { height / 2.0 + 0.23 } else { -height / 2.0 - 0.8 };
    Transform::from_xyz(2.54 + 0.36 * rank, y + 0.45 * rank, -0.56 * rank)
        .with_rotation(Quat::from_euler(EulerRot::YXZ, -5_f32.to_radians(), 3_f32.to_radians(), 0.0))
        .with_scale(Vec3::new(5.28, height, 1.0))
}

pub(super) fn texture_size(pixels: UVec2, pose: Transform) -> UVec2 {
    let aspect = pixels.x as f32 / pixels.y as f32;
    let scale = pixels.y as f32 / frame_height(aspect)
        * (CAMERA_DISTANCE / (CAMERA_DISTANCE - pose.translation.z));
    (pose.scale.truncate() * scale).round().as_uvec2().max(UVec2::ONE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn projected_bounds(pose: Transform) -> Rect {
        let mut min = Vec2::splat(f32::INFINITY);
        let mut max = Vec2::splat(f32::NEG_INFINITY);
        for x in [-0.5, 0.5] {
            for y in [-0.5, 0.5] {
                let point = pose.transform_point(Vec3::new(x, y, 0.0));
                let projected = point.truncate() * (CAMERA_DISTANCE / (CAMERA_DISTANCE - point.z));
                min = min.min(projected);
                max = max.max(projected);
            }
        }
        Rect { min, max }
    }

    #[test]
    fn desktop_keeps_the_conversation_square_left_of_both_stacks() {
        let conversation = pose(0, 0, 16.0 / 9.0);
        let diff = pose(1, 0, 16.0 / 9.0);
        let event = pose(2, 0, 16.0 / 9.0);

        assert_eq!(conversation.scale.x, conversation.scale.y);
        assert!(projected_bounds(conversation).max.x < projected_bounds(diff).min.x);
        assert!(projected_bounds(event).max.y < projected_bounds(diff).min.y);
        assert!((conversation.rotation * Vec3::X).z < 0.0);
    }

    #[test]
    fn both_stacks_fit_inside_the_camera_frame() {
        for aspect in [0.75, 1.0, 16.0 / 9.0, 2.75] {
            let camera_half_height = (fov(aspect) / 2.0).tan() * CAMERA_DISTANCE;
            let camera_half_width = camera_half_height * aspect;
            for group in 0..GROUPS {
                let count = if group == 0 { 1 } else { STACK_DEPTH };
                for rank in 0..count {
                    let bounds = projected_bounds(pose(group, rank, aspect));
                    assert!(bounds.min.x > -camera_half_width && bounds.max.x < camera_half_width);
                    assert!(bounds.min.y > -camera_half_height && bounds.max.y < camera_half_height);
                }
            }
        }
    }

    #[test]
    fn narrow_view_uses_more_height_for_stack_content() {
        let viewport = UVec2::new(640, 534);
        let aspect = viewport.x as f32 / viewport.y as f32;
        let conversation = pose(0, 0, aspect);
        let diff = pose(1, 0, aspect);
        let event = pose(2, 2, aspect);
        assert_eq!(conversation.scale.x, conversation.scale.y);
        assert!(projected_bounds(event).max.y < projected_bounds(diff).min.y);
        let texture = texture_size(viewport, diff);
        assert!((240..=280).contains(&texture.x));
        assert!(texture.y >= 160);
    }

    #[test]
    fn rear_cards_expose_their_titles_above_the_front_card() {
        for group in [1, 2] {
            let mut closer = pose(group, 0, 16.0 / 9.0);
            for rank in 1..3 {
                let rear = pose(group, rank, 16.0 / 9.0);
                assert!(rear.translation.z < closer.translation.z);
                let front_bounds = projected_bounds(closer);
                let rear_bounds = projected_bounds(rear);
                assert!(rear_bounds.max.x > front_bounds.max.x);
                assert!(rear_bounds.min.y < front_bounds.max.y);
                assert!(rear_bounds.max.y - front_bounds.max.y >= rear_bounds.height() * 0.11);
                closer = rear;
            }
        }
    }
}
