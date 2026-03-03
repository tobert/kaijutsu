//! Scroll system — smooth interpolation for conversation scrolling.

use bevy::prelude::*;

use crate::cell::{ConversationScrollState, EditorEntities};

/// Smooth scroll interpolation system.
///
/// In follow mode, locks directly to bottom (no interpolation).
/// Interpolation is only used for manual scrolling (wheel, Page Up/Down, etc.)
/// to prevent "chasing a moving target" stutter during streaming.
pub fn smooth_scroll(
    mut scroll_state: ResMut<ConversationScrollState>,
    time: Res<Time>,
    entities: Res<EditorEntities>,
    mut scroll_positions: Query<(&mut ScrollPosition, &ComputedNode), With<crate::cell::ConversationContainer>>,
) {
    scroll_state.bypass_change_detection().user_scrolled_this_frame = false;

    let old_offset = scroll_state.offset;
    let old_target = scroll_state.target_offset;
    let old_visible = scroll_state.visible_height;

    let max = scroll_state.max_offset();
    let clamped_target = scroll_state.target_offset.min(max).max(0.0);

    let (new_offset, new_target) = if scroll_state.following {
        if (max - old_offset).abs() >= 1.0 {
            (max, max)
        } else {
            (old_offset, old_target)
        }
    } else {
        const SCROLL_SPEED: f32 = 12.0;
        let t = (time.delta_secs() * SCROLL_SPEED).min(1.0);
        let new_offset = old_offset + (clamped_target - old_offset) * t;

        let snapped = if (new_offset - clamped_target).abs() < 0.5 {
            clamped_target
        } else {
            new_offset
        };
        (snapped, clamped_target)
    };

    let new_visible = if let Some(conv) = entities.conversation_container {
        if let Ok((_, computed)) = scroll_positions.get(conv) {
            let content_box = computed.content_box();
            let h = content_box.height();
            if h > 0.0 { h } else { old_visible }
        } else {
            old_visible
        }
    } else {
        old_visible
    };

    let offset_changed = (new_offset - old_offset).abs() > 0.01;
    let target_changed = (new_target - old_target).abs() > 0.01;
    let visible_changed = (new_visible - old_visible).abs() > 0.5;

    if offset_changed || target_changed || visible_changed {
        let state = scroll_state.as_mut();
        state.offset = new_offset;
        state.target_offset = new_target;
        state.visible_height = new_visible;
    }

    if let Some(conv) = entities.conversation_container {
        if let Ok((mut scroll_pos, _)) = scroll_positions.get_mut(conv) {
            // Round to integer pixels to prevent sub-pixel jitter at clip boundaries.
            // Fractional scroll offsets cause clip rects to land between pixels,
            // producing antialiasing artifacts at the container edge.
            let pixel_offset = new_offset.round();
            let current_y = scroll_pos.y;
            if (pixel_offset - current_y).abs() > 0.01 {
                **scroll_pos = Vec2::new(scroll_pos.x, pixel_offset);
            }
        }
    }
}
