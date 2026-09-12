//! Submit error handling (flash + restore).

use bevy::prelude::*;

use crate::cell::block_border::BlockBorderStyle;
use crate::cell::{
    ComposeError, InputOverlay, InputOverlayMarker, MsdfOverlayText, PendingSubmitRecoveries,
    SubmitFailed,
};
use crate::view::shell_dock::ShellDockMarker;

/// Restore a failed submission only when nothing newer has been typed.
/// Returning false preserves the newer text while the global error still
/// names the failed submission.
fn restore_failed_submission(overlay: &mut InputOverlay, text: &str) -> bool {
    if !overlay.text.is_empty() {
        return false;
    }
    overlay.text = text.to_string();
    overlay.cursor = overlay.text.len();
    true
}

fn can_restore_failed_submission(
    active_context: Option<kaijutsu_types::ContextId>,
    current_principal: Option<kaijutsu_types::PrincipalId>,
    failed: &SubmitFailed,
) -> bool {
    active_context == Some(failed.context_id) && current_principal == Some(failed.principal_id)
}
use crate::ui::theme::Theme;

/// Restore overlay text and flash error border when submit fails.
pub fn handle_submit_failed(
    mut commands: Commands,
    mut fail_events: MessageReader<SubmitFailed>,
    mut overlay: Query<(Entity, &mut InputOverlay), With<InputOverlayMarker>>,
    mut shell_overlay: Query<
        (Entity, &mut InputOverlay),
        (With<ShellDockMarker>, Without<InputOverlayMarker>),
    >,
    doc_cache: Res<crate::cell::DocumentCache>,
    session_principal: Res<crate::cell::SessionPrincipal>,
    mut pending: ResMut<PendingSubmitRecoveries>,
    mut identity_transition: ResMut<crate::cell::PendingIdentityTransition>,
) {
    if let Some((previous_principal, _)) = identity_transition.0.take() {
        if let Ok((_, mut chat)) = overlay.single_mut()
            && !chat.text.is_empty()
            && let Some(context_id) = chat.target_context.or(doc_cache.active_id())
        {
            pending.0.push(SubmitFailed {
                text: std::mem::take(&mut chat.text),
                reason: "authenticated identity changed before this text was submitted".into(),
                is_shell: false,
                context_id,
                principal_id: previous_principal,
            });
            chat.cursor = 0;
            chat.selection_anchor = None;
            chat.target_context = None;
        }
        if let Ok((_, mut shell)) = shell_overlay.single_mut()
            && !shell.text.is_empty()
            && let Some(context_id) = shell.target_context.or(doc_cache.active_id())
        {
            pending.0.push(SubmitFailed {
                text: std::mem::take(&mut shell.text),
                reason: "authenticated identity changed before this text was submitted".into(),
                is_shell: true,
                context_id,
                principal_id: previous_principal,
            });
            shell.cursor = 0;
            shell.selection_anchor = None;
            shell.target_context = None;
        }
    }
    for failed in fail_events.read() {
        warn!("Submit failed: {}", failed.reason);
        pending.0.push(failed.clone());
    }
    let mut remaining = Vec::new();
    for failed in pending.0.drain(..) {
        if !can_restore_failed_submission(doc_cache.active_id(), session_principal.0, &failed) {
            remaining.push(failed);
            continue;
        }
        let target = if failed.is_shell {
            shell_overlay.single_mut()
        } else {
            overlay.single_mut()
        };
        if let Ok((entity, mut overlay)) = target
            && restore_failed_submission(&mut overlay, &failed.text)
        {
            commands.entity(entity).insert(ComposeError {
                started: std::time::Instant::now(),
            });
            overlay.target_context = Some(failed.context_id);
        } else {
            remaining.push(failed);
        }
    }
    pending.0 = remaining;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_submission_restores_text_only_when_the_overlay_is_still_empty() {
        let mut overlay = InputOverlay::default();
        assert!(restore_failed_submission(&mut overlay, "write release notes"));
        assert_eq!(overlay.text, "write release notes");

        overlay.text = "newer text".into();
        overlay.cursor = overlay.text.len();
        assert!(!restore_failed_submission(&mut overlay, "older text"));
        assert_eq!(overlay.text, "newer text");
    }

    #[test]
    fn delayed_failure_cannot_restore_text_into_a_different_context() {
        let context_a = kaijutsu_types::ContextId::new();
        let context_b = kaijutsu_types::ContextId::new();
        let amy = kaijutsu_types::PrincipalId::new();
        let failed = SubmitFailed {
            text: "old command".into(),
            reason: "connection reset".into(),
            is_shell: true,
            context_id: context_a,
            principal_id: amy,
        };

        assert!(can_restore_failed_submission(Some(context_a), Some(amy), &failed));
        assert!(!can_restore_failed_submission(Some(context_b), Some(amy), &failed));
        assert!(!can_restore_failed_submission(Some(context_a), None, &failed));
    }

    #[test]
    fn delayed_failure_waits_for_its_context_then_restores_the_saved_text() {
        let context_a = kaijutsu_types::ContextId::new();
        let context_b = kaijutsu_types::ContextId::new();
        let amy = kaijutsu_types::PrincipalId::new();
        let mut app = App::new();
        app.add_message::<SubmitFailed>()
            .init_resource::<crate::cell::DocumentCache>()
            .insert_resource(crate::cell::SessionPrincipal(Some(amy)))
            .init_resource::<PendingSubmitRecoveries>()
            .init_resource::<crate::cell::PendingIdentityTransition>()
            .add_systems(Update, handle_submit_failed);
        let overlay = app
            .world_mut()
            .spawn((InputOverlayMarker, InputOverlay::default()))
            .id();

        app.world_mut().resource_mut::<crate::cell::DocumentCache>().set_active(context_b);
        app.world_mut().write_message(SubmitFailed {
            text: "old command".into(),
            reason: "connection reset".into(),
            is_shell: false,
            context_id: context_a,
            principal_id: amy,
        });
        app.update();
        assert_eq!(app.world().get::<InputOverlay>(overlay).unwrap().text, "");
        assert_eq!(app.world().resource::<PendingSubmitRecoveries>().0.len(), 1);

        app.world_mut().resource_mut::<crate::cell::DocumentCache>().set_active(context_a);
        app.update();
        assert_eq!(app.world().get::<InputOverlay>(overlay).unwrap().text, "old command");
        assert!(app.world().resource::<PendingSubmitRecoveries>().0.is_empty());
    }

    #[test]
    fn identity_change_holds_text_for_its_original_context_and_principal() {
        let context_a = kaijutsu_types::ContextId::new();
        let context_b = kaijutsu_types::ContextId::new();
        let amy = kaijutsu_types::PrincipalId::new();
        let banto = kaijutsu_types::PrincipalId::new();
        let mut app = App::new();
        app.add_message::<SubmitFailed>()
            .init_resource::<crate::cell::DocumentCache>()
            .insert_resource(crate::cell::SessionPrincipal(Some(banto)))
            .init_resource::<PendingSubmitRecoveries>()
            .insert_resource(crate::cell::PendingIdentityTransition(Some((amy, Some(banto)))))
            .add_systems(Update, handle_submit_failed);
        let overlay = app
            .world_mut()
            .spawn((
                InputOverlayMarker,
                InputOverlay {
                    text: "Amy's draft".into(),
                    target_context: Some(context_a),
                    ..Default::default()
                },
            ))
            .id();

        app.world_mut().resource_mut::<crate::cell::DocumentCache>().set_active(context_b);
        app.update();
        assert_eq!(app.world().get::<InputOverlay>(overlay).unwrap().text, "");
        assert_eq!(app.world().resource::<PendingSubmitRecoveries>().0.len(), 1);

        app.world_mut().resource_mut::<crate::cell::DocumentCache>().set_active(context_a);
        app.world_mut().resource_mut::<crate::cell::SessionPrincipal>().0 = Some(amy);
        app.update();
        assert_eq!(app.world().get::<InputOverlay>(overlay).unwrap().text, "Amy's draft");
        assert!(app.world().resource::<PendingSubmitRecoveries>().0.is_empty());
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
