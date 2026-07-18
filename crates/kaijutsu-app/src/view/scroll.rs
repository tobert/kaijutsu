//! Scroll system — smooth interpolation for conversation scrolling.

use bevy::prelude::*;
use bevy::winit::WinitSettings;

use crate::cell::{ConversationScrollState, EditorEntities};
use crate::input::ScrollConfig;

/// Smooth scroll interpolation system.
///
/// In follow mode, locks directly to bottom (no interpolation).
/// Interpolation is only used for manual scrolling (wheel, Page Up/Down, etc.)
/// to prevent "chasing a moving target" stutter during streaming.
pub fn smooth_scroll(
    mut scroll_state: ResMut<ConversationScrollState>,
    time: Res<Time>,
    config: Res<ScrollConfig>,
    entities: Res<EditorEntities>,
    mut scroll_positions: Query<
        (&mut ScrollPosition, &ComputedNode),
        With<crate::cell::ConversationContainer>,
    >,
) {
    scroll_state
        .bypass_change_detection()
        .user_scrolled_this_frame = false;

    let old_offset = scroll_state.offset;
    let old_target = scroll_state.target_offset;
    let old_visible = scroll_state.visible_height;

    let max = scroll_state.max_offset();
    let clamped_target = scroll_state.target_offset.min(max).max(0.0);

    let (new_offset, new_target) = if scroll_state.following {
        // When new blocks just appeared, reveal them from their start rather than
        // jumping to the absolute bottom. The anchor is the content height before
        // the new blocks were measured, i.e. the y-offset where they begin.
        // Using min(max, anchor) ensures we show the new block's top when it's
        // taller than the viewport, but still scroll normally for small blocks.
        let target = if let Some(anchor) = scroll_state.pending_scroll_anchor.take() {
            anchor.min(max)
        } else {
            max
        };
        if (target - old_offset).abs() >= 1.0 {
            (target, target)
        } else {
            (old_offset, old_target)
        }
    } else {
        let t = (time.delta_secs() * config.smooth_speed).min(1.0);
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
            // ComputedNode is physical px; scroll offsets (and the
            // ScrollPosition we write below) are logical.
            let h = crate::view::ui_rtt::logical_content_size(computed).y;
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

    if let Some(conv) = entities.conversation_container
        && let Ok((mut scroll_pos, _)) = scroll_positions.get_mut(conv)
    {
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

/// Keep the render loop at full rate *only* while a wheel scroll is easing.
///
/// The app is reactive-idle (`main.rs` `WinitSettings`, ~10Hz focused), which
/// starves the frame-by-frame glide in `smooth_scroll` — the ease needs a
/// stream of frames to run. While `offset` is still chasing `target_offset`,
/// force `Continuous`; once settled, restore reactive idle so we don't spin
/// the GPU when nothing is moving. Follow mode snaps instantly (no ease), so
/// it never trips this. (Generalizing this to one "animation active" gate the
/// DJ thread shares is tracked in docs/issues.md, DJ thread arc.)
pub fn scroll_render_mode(
    scroll_state: Res<crate::cell::ConversationScrollState>,
    mut winit: ResMut<WinitSettings>,
) {
    use bevy::winit::UpdateMode;
    let easing = !scroll_state.following
        && (scroll_state.offset - scroll_state.target_offset).abs() > 0.5;
    let desired = if easing {
        UpdateMode::Continuous
    } else {
        UpdateMode::reactive(std::time::Duration::from_millis(100))
    };
    // Only write when it actually changes, to avoid needless change-detection.
    if winit.focused_mode != desired {
        winit.focused_mode = desired;
    }
}
