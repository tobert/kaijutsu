//! Submit error handling (flash + restore).

use bevy::prelude::*;

use crate::cell::{
    ComposeError, InputOverlay, InputOverlayMarker, SubmitFailed,
};
use crate::ui::theme::Theme;

/// Restore overlay text and flash error border when submit fails.
pub fn handle_submit_failed(
    mut commands: Commands,
    mut fail_events: MessageReader<SubmitFailed>,
    mut overlay: Query<(Entity, &mut InputOverlay), With<InputOverlayMarker>>,
) {
    for failed in fail_events.read() {
        warn!("Submit failed: {}", failed.reason);
        if let Ok((entity, mut overlay)) = overlay.single_mut() {
            overlay.text = failed.text.clone();
            overlay.cursor = overlay.text.len();
            commands.entity(entity).insert(ComposeError {
                started: std::time::Instant::now(),
            });
        }
    }
}

/// Animate compose error border: flash red then fade back to theme color.
pub fn animate_compose_error(
    mut commands: Commands,
    theme: Res<Theme>,
    mut query: Query<(Entity, &ComposeError, &mut BorderColor)>,
) {
    for (entity, error, mut border) in query.iter_mut() {
        let elapsed = error.started.elapsed().as_secs_f32();
        const DURATION: f32 = 2.0;

        if elapsed >= DURATION {
            *border = BorderColor::all(theme.compose_border);
            commands.entity(entity).remove::<ComposeError>();
        } else {
            let t = elapsed / DURATION;
            let red = Color::srgb(0.9, 0.2, 0.2);
            let target = theme.compose_border;
            let r = red.mix(&target, t);
            *border = BorderColor::all(r);
        }
    }
}
