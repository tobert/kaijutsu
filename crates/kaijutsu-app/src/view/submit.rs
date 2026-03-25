//! Submit error handling (flash + restore).

use bevy::prelude::*;

use crate::cell::block_border::BlockBorderStyle;
use crate::cell::{ComposeError, InputOverlay, InputOverlayMarker, MsdfOverlayText, SubmitFailed};
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
///
/// ComposeError is on the parent (InputOverlayMarker), but the visual border
/// is BlockBorderStyle on the MsdfOverlayText child.
pub fn animate_compose_error(
    mut commands: Commands,
    theme: Res<Theme>,
    query: Query<(Entity, &ComposeError, &Children), With<InputOverlayMarker>>,
    mut border_query: Query<&mut BlockBorderStyle, With<MsdfOverlayText>>,
) {
    for (entity, error, children) in query.iter() {
        let elapsed = error.started.elapsed().as_secs_f32();
        const DURATION: f32 = 2.0;

        if elapsed >= DURATION {
            // Animation complete — restore theme color, remove marker
            for child in children.iter() {
                if let Ok(mut border) = border_query.get_mut(child) {
                    border.color = theme.compose_palette_border;
                }
            }
            commands.entity(entity).remove::<ComposeError>();
        } else {
            let t = elapsed / DURATION;
            let red = Color::srgb(0.9, 0.2, 0.2);
            let target = theme.compose_palette_border;
            let r = red.mix(&target, t);
            for child in children.iter() {
                if let Ok(mut border) = border_query.get_mut(child) {
                    border.color = r;
                }
            }
        }
    }
}
