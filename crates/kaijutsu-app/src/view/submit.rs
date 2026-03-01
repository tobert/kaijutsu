//! Prompt submission and error handling.
//!
//! Routes submitted text to either LLM prompt or kaish shell execution.

use bevy::prelude::*;

use crate::cell::{
    ComposeError, ConversationScrollState, DocumentCache, InputOverlay, InputOverlayMarker,
    PromptSubmitted, SubmitFailed,
};
use crate::ui::theme::Theme;

/// Detect whether submitted text is a shell command based on prefix.
fn is_shell_command(text: &str) -> bool {
    text.starts_with(':') || text.starts_with('`')
}

/// Strip the shell prefix from a command before execution.
fn strip_shell_prefix(text: &str) -> &str {
    if text.starts_with(':') || text.starts_with('`') {
        &text[1..]
    } else {
        text
    }
}

/// Send submitted prompts to the server.
///
/// Routes to either LLM prompt (Chat) or kaish shell execution (Shell),
/// auto-detected from text prefix.
pub fn handle_prompt_submitted(
    mut submit_events: MessageReader<PromptSubmitted>,
    mut fail_writer: MessageWriter<SubmitFailed>,
    doc_cache: Res<DocumentCache>,
    mut scroll_state: ResMut<ConversationScrollState>,
    actor: Option<Res<crate::connection::RpcActor>>,
    channel: Res<crate::connection::RpcResultChannel>,
) {
    if submit_events.is_empty() {
        return;
    }

    let Some(ctx_id) = doc_cache.active_id() else {
        warn!("No active context to add message to");
        return;
    };

    for event in submit_events.read() {
        if let Some(ref actor) = actor {
            let handle = actor.handle.clone();
            let tx = channel.sender();

            if is_shell_command(&event.text) {
                let text = strip_shell_prefix(&event.text).to_string();
                bevy::tasks::IoTaskPool::get()
                    .spawn(async move {
                        if let Err(e) = handle.shell_execute(&text, ctx_id).await {
                            log::error!("shell_execute failed: {e}");
                            let _ = tx.send(crate::connection::RpcResultMessage::RpcError {
                                operation: "shell_execute".into(),
                                error: e.to_string(),
                            });
                        }
                    })
                    .detach();
                info!("Sent shell command to server (ctx={})", ctx_id);
            } else {
                let text = event.text.clone();
                bevy::tasks::IoTaskPool::get()
                    .spawn(async move {
                        if let Err(e) = handle.prompt(&text, None, ctx_id).await {
                            log::error!("prompt failed: {e}");
                            let _ = tx.send(crate::connection::RpcResultMessage::RpcError {
                                operation: "prompt".into(),
                                error: e.to_string(),
                            });
                        }
                    })
                    .detach();
                info!("Sent prompt to server (ctx={})", ctx_id);
            }
            scroll_state.start_following();
        } else {
            warn!("No connection available, prompt not sent to server");
            fail_writer.write(SubmitFailed {
                text: event.text.clone(),
                reason: "Not connected to server".into(),
            });
        }
    }
}

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
