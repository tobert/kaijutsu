//! Submit error handling: restore a failed shell line, flash the chat
//! overlay.

use bevy::prelude::*;

use crate::cell::block_border::BlockBorderStyle;
use crate::cell::{ComposeError, InputOverlayMarker, MsdfOverlayText, PendingSubmitRecoveries, SubmitFailed};
use crate::input::compose::{ComposeBuffer, ComposeState};

/// Restore a failed shell line only into an empty shell surface. A newer
/// line keeps the failed text queued for its original principal and context.
fn restore_failed_submission(shell: &mut ComposeBuffer, text: &str) -> bool {
    if !shell.text().is_empty() {
        return false;
    }
    *shell = ComposeBuffer::over(text);
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

/// Restore a failed shell line once its context and principal are on
/// screen again. Shell text typed under a principal that has since been
/// replaced is kept the same way, so the new principal cannot run it.
pub fn handle_submit_failed(
    mut fail_events: MessageReader<SubmitFailed>,
    mut compose: ResMut<ComposeState>,
    doc_cache: Res<crate::cell::DocumentCache>,
    session_principal: Res<crate::cell::SessionPrincipal>,
    mut pending: ResMut<PendingSubmitRecoveries>,
    mut identity_transition: ResMut<crate::cell::PendingIdentityTransition>,
) {
    if let Some((previous_principal, _)) = identity_transition.0.take()
        && !compose.shell.text().is_empty()
        && let Some(context_id) = doc_cache.active_id()
    {
        pending.0.push(SubmitFailed {
            text: std::mem::take(&mut compose.shell).text(),
            reason: "authenticated identity changed before this text was submitted".into(),
            context_id,
            principal_id: previous_principal,
        });
    }
    for failed in fail_events.read() {
        warn!("Submit failed: {}", failed.reason);
        pending.0.push(failed.clone());
    }
    let mut remaining = Vec::new();
    for failed in pending.0.drain(..) {
        if can_restore_failed_submission(doc_cache.active_id(), session_principal.0, &failed)
            && restore_failed_submission(&mut compose.shell, &failed.text)
        {
            continue;
        }
        remaining.push(failed);
    }
    pending.0 = remaining;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_submission_restores_text_only_when_the_shell_is_still_empty() {
        let mut shell = ComposeBuffer::default();
        assert!(restore_failed_submission(&mut shell, "write release notes"));
        assert_eq!(shell.text(), "write release notes");

        shell = ComposeBuffer::over("newer text");
        assert!(!restore_failed_submission(&mut shell, "older text"));
        assert_eq!(shell.text(), "newer text");
    }

    #[test]
    fn delayed_failure_cannot_restore_text_into_a_different_context() {
        let context_a = kaijutsu_types::ContextId::new();
        let context_b = kaijutsu_types::ContextId::new();
        let amy = kaijutsu_types::PrincipalId::new();
        let failed = SubmitFailed {
            text: "old command".into(),
            reason: "connection reset".into(),
            context_id: context_a,
            principal_id: amy,
        };

        assert!(can_restore_failed_submission(Some(context_a), Some(amy), &failed));
        assert!(!can_restore_failed_submission(Some(context_b), Some(amy), &failed));
        assert!(!can_restore_failed_submission(Some(context_a), None, &failed));
    }

    fn submit_app(principal: kaijutsu_types::PrincipalId) -> App {
        let mut app = App::new();
        app.add_message::<SubmitFailed>()
            .init_resource::<crate::cell::DocumentCache>()
            .insert_resource(crate::cell::SessionPrincipal(Some(principal)))
            .init_resource::<PendingSubmitRecoveries>()
            .init_resource::<crate::cell::PendingIdentityTransition>()
            .init_resource::<ComposeState>()
            .add_systems(Update, handle_submit_failed);
        app
    }

    fn shell_text(app: &App) -> String {
        app.world().resource::<ComposeState>().shell.text()
    }

    #[test]
    fn delayed_failure_waits_for_its_context_then_restores_the_saved_text() {
        let context_a = kaijutsu_types::ContextId::new();
        let context_b = kaijutsu_types::ContextId::new();
        let amy = kaijutsu_types::PrincipalId::new();
        let mut app = submit_app(amy);

        app.world_mut().resource_mut::<crate::cell::DocumentCache>().set_active(context_b);
        app.world_mut().write_message(SubmitFailed {
            text: "old command".into(),
            reason: "connection reset".into(),
            context_id: context_a,
            principal_id: amy,
        });
        app.update();
        assert_eq!(shell_text(&app), "");
        assert_eq!(app.world().resource::<PendingSubmitRecoveries>().0.len(), 1);

        app.world_mut().resource_mut::<crate::cell::DocumentCache>().set_active(context_a);
        app.update();
        assert_eq!(shell_text(&app), "old command");
        assert!(app.world().resource::<PendingSubmitRecoveries>().0.is_empty());
    }

    #[test]
    fn identity_change_holds_shell_text_for_its_original_context_and_principal() {
        let context_a = kaijutsu_types::ContextId::new();
        let context_b = kaijutsu_types::ContextId::new();
        let amy = kaijutsu_types::PrincipalId::new();
        let banto = kaijutsu_types::PrincipalId::new();
        let mut app = submit_app(banto);
        app.insert_resource(crate::cell::PendingIdentityTransition(Some((amy, Some(banto)))));
        app.world_mut().resource_mut::<ComposeState>().shell = ComposeBuffer::over("Amy's line");
        app.world_mut().resource_mut::<crate::cell::DocumentCache>().set_active(context_a);

        app.update();
        assert_eq!(shell_text(&app), "", "banto cannot run Amy's line");
        assert_eq!(app.world().resource::<PendingSubmitRecoveries>().0.len(), 1);

        app.world_mut().resource_mut::<crate::cell::DocumentCache>().set_active(context_b);
        app.world_mut().resource_mut::<crate::cell::SessionPrincipal>().0 = Some(amy);
        app.update();
        assert_eq!(shell_text(&app), "", "the line waits for its own context");

        app.world_mut().resource_mut::<crate::cell::DocumentCache>().set_active(context_a);
        app.update();
        assert_eq!(shell_text(&app), "Amy's line");
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
