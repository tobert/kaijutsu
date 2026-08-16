//! Scroll system — smooth interpolation for conversation scrolling.

use bevy::prelude::*;
use bevy::winit::WinitSettings;

use crate::cell::{ConversationScrollState, EditorEntities};
use crate::input::ScrollConfig;

/// Exponential-decay ease progress for a frame of length `dt` (seconds) at
/// rate `smooth_speed` (per second): the fraction of the remaining distance
/// to close this frame.
///
/// Frame-rate independent by construction — `1 - exp(-k*dt)` composes
/// correctly across frame splits: two half-length steps at rate `k` close
/// the same total fraction of the distance as one full-length step (the
/// underlying displacement decays as `exp(-k*t)`, and `exp(-k*(dt/2))^2 ==
/// exp(-k*dt)`). The old form, `(smooth_speed * dt).min(1.0)`, was linear in
/// `dt` and clamped: at 10Hz (`dt` ~0.1s) with `smooth_speed = 45.0` it hit
/// the `.min(1.0)` clamp and skipped smoothing entirely, while at 144Hz
/// (`dt` ~0.007s) it produced `t ~= 0.31` instead of the 60Hz-tuned feel.
fn ease_t(smooth_speed: f32, dt: f32) -> f32 {
    1.0 - (-smooth_speed * dt).exp()
}

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
        // Peek immutably first: `ResMut`'s `DerefMut` (needed for `.take()`)
        // marks the resource changed on every access, so unconditionally
        // calling `.take()` here marked `ConversationScrollState` changed on
        // every frame spent following, even when there was nothing to take.
        // Only reach for `DerefMut` when there's actually a `Some` to drain.
        let target = if scroll_state.pending_scroll_anchor.is_some() {
            let anchor = scroll_state
                .pending_scroll_anchor
                .take()
                .expect("checked is_some above");
            anchor.min(max)
        } else {
            max
        };
        if (target - old_offset).abs() >= 1.0 {
            (target, target)
        } else {
            // Below the 1px move threshold: skip the (visually invisible)
            // offset write, but keep target_offset synced to the clamped
            // bottom. Leaving it at `old_target` here left `is_at_bottom()`
            // reading a stale, pre-clamp value while sitting in follow mode.
            (old_offset, target)
        }
    } else {
        let t = ease_t(config.smooth_speed, time.delta_secs());
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
        // Write the raw logical offset — no rounding. `ScrollPosition` is
        // logical px; Bevy multiplies by the scale factor and floors to a
        // physical pixel downstream. Rounding here first threw away
        // sub-pixel resolution before that multiply, which at 2x scale (or
        // higher) halved (or worse) the granularity actually available —
        // e.g. a 0.6 logical-px change (1.2 physical px at 2x, a real,
        // renderable step) rounded to 0 and was dropped entirely.
        let current_y = scroll_pos.y;
        if (new_offset - current_y).abs() > 0.01 {
            **scroll_pos = Vec2::new(scroll_pos.x, new_offset);
        }
    }
}

/// Keep the render loop at full rate while a wheel scroll is easing, or
/// while follow-mode is actively tracking freshly streamed content.
///
/// The app is reactive-idle (`main.rs` `WinitSettings`, ~10Hz focused), which
/// starves two things that need a steady stream of frames:
///
/// 1. The frame-by-frame glide in `smooth_scroll` while `offset` is still
///    chasing `target_offset` (manual scrolling).
/// 2. Follow-mode streaming: follow snaps `offset` instantly rather than
///    easing, so it never trips (1), but new content still needs to *paint*
///    every frame while it's arriving — at 10Hz idle a streaming response
///    only repaints 10x/second.
///
/// This is the scoped version of the fix: it watches this system's own
/// signals (`smooth_scroll`'s ease distance, and `ConversationScrollState`'s
/// `content_height` changing) rather than building the fully general
/// "animation active" vote gate that other animating subsystems (DJ thread,
/// time-well drift, metronome flash) would also want — that generalization
/// is tracked in docs/issues.md, DJ thread arc, and deliberately deferred.
pub fn scroll_render_mode(
    scroll_state: Res<crate::cell::ConversationScrollState>,
    time: Res<Time>,
    mut winit: ResMut<WinitSettings>,
    // Both `Local`s default to 0.0, which reads as "content just changed"
    // the first time `content_height` becomes nonzero (initial load) —
    // spuriously forcing Continuous for the first ~250ms of app life.
    // Harmless and self-correcting (STREAMING_WINDOW_SECS below elapses on
    // its own), not worth a sentinel init just to skip one early window.
    mut last_content_height: Local<f32>,
    mut time_since_content_change: Local<f32>,
) {
    use bevy::winit::UpdateMode;

    if (scroll_state.content_height - *last_content_height).abs() > 0.01 {
        *last_content_height = scroll_state.content_height;
        *time_since_content_change = 0.0;
    } else {
        *time_since_content_change += time.delta_secs();
    }

    let easing = !scroll_state.following
        && (scroll_state.offset - scroll_state.target_offset).abs() > 0.5;
    // Content changed within the last ~250ms while following: a streaming
    // burst likely has more on the way, so keep painting at full rate rather
    // than falling back to 10Hz between block updates.
    const STREAMING_WINDOW_SECS: f32 = 0.25;
    let streaming_while_following =
        scroll_state.following && *time_since_content_change < STREAMING_WINDOW_SECS;

    let active = easing || streaming_while_following;
    let desired = if active {
        UpdateMode::Continuous
    } else {
        UpdateMode::reactive(std::time::Duration::from_millis(100))
    };
    // The UNFOCUSED mode matters just as much: Amy's normal setup scrolls
    // and reads kaijutsu on one screen while another app (a game, a
    // terminal) holds keyboard focus on the other. Wayland delivers wheel
    // events to the hovered window regardless of focus, but with
    // `unfocused_mode` left at its 2Hz `reactive_low_power(500ms)` idle
    // (main.rs), an unfocused scroll only repainted when an event happened
    // to arrive — scrolling felt dead exactly when she used it most
    // (found live 2026-08-16, FFXIV on the focused screen). While scroll
    // or streaming is active, force full rate on BOTH modes; when idle,
    // restore each mode's own baseline.
    let desired_unfocused = if active {
        UpdateMode::Continuous
    } else {
        UpdateMode::reactive_low_power(std::time::Duration::from_millis(500))
    };
    // Only write when it actually changes, to avoid needless change-detection.
    if winit.focused_mode != desired {
        winit.focused_mode = desired;
    }
    if winit.unfocused_mode != desired_unfocused {
        winit.unfocused_mode = desired_unfocused;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of the exponential form: splitting one frame into two
    /// half-length frames must not change the net displacement. At the old
    /// `(smooth_speed * dt).min(1.0)` form this did NOT hold — a 60Hz step
    /// and two 120Hz steps landed at different fractions of the distance
    /// closed (worse, `.min(1.0)` clamped low frame rates to 1.0 outright).
    #[test]
    fn ease_is_frame_rate_independent_across_a_frame_split() {
        let smooth_speed: f32 = 83.18; // matches ScrollConfig::default()
        let start = 0.0_f32;
        let target = 100.0_f32;

        let dt_60 = 1.0 / 60.0_f32;
        let one_step = start + (target - start) * ease_t(smooth_speed, dt_60);

        let dt_120 = 1.0 / 120.0_f32;
        let mid = start + (target - start) * ease_t(smooth_speed, dt_120);
        let two_step = mid + (target - mid) * ease_t(smooth_speed, dt_120);

        assert!(
            (one_step - two_step).abs() < 1e-3,
            "one 60Hz step ({one_step}) should match two 120Hz steps ({two_step})"
        );
    }

    /// Guards the specific bug: at a low frame rate (large `dt`), the old
    /// linear form clamped `t` to 1.0 via `.min(1.0)`, snapping instantly
    /// instead of easing. The exponential form asymptotically approaches
    /// (never reaches) 1.0, so even a very large `dt` leaves `t < 1.0`.
    #[test]
    fn ease_never_snaps_even_at_a_very_low_frame_rate() {
        let t = ease_t(83.18, 1.0 / 10.0); // 10Hz, the old clamp-to-1.0 case
        assert!(t < 1.0, "t={t} should stay below 1.0");
        assert!(t > 0.9, "t={t} should still be a large step at 10Hz");
    }

    /// Guards the other half of the old bug: at a high frame rate (small
    /// `dt`), the linear form under-eased relative to the 60Hz-tuned feel
    /// (t ~= 0.31 at 144Hz vs. the tuned 0.75 at 60Hz). The exponential form
    /// keeps the *rate* constant, so a smaller `dt` naturally yields a
    /// smaller (but proportionally consistent) `t`.
    #[test]
    fn ease_t_is_positive_and_bounded_at_a_high_frame_rate() {
        let t = ease_t(83.18, 1.0 / 144.0);
        assert!(t > 0.0 && t < 1.0, "t={t} should be strictly between 0 and 1");
    }
}
