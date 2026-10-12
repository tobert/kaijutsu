//! The systems that drive compose: keys from the grab, the actions that
//! summon and leave it, the draft's load and reconcile, and the overlays
//! that draw it (`docs/input.md`, "Escape"; `docs/tui.md`, "Compose").

use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use bevy::tasks::IoTaskPool;
use kaijutsu_editor::EditOp;
use kaijutsu_types::ContextId;
use modalkit::crossterm::event::{KeyCode as CtKeyCode, KeyEvent, KeyModifiers};

use super::outbox::DraftOutboxState;
use super::{
    ComposeAction, ComposeBuffer, ComposeState, DraftTarget, HeldInput, LoadStarted, byte_offset, colon_vi_statement,
    shell_kj_argv,
};
use crate::cell::{
    ComposeError, ConversationScrollState, DocumentCache, InputOverlay, InputOverlayMarker, SessionPrincipal,
};
use crate::connection::{RpcActor, RpcResultChannel, RpcResultMessage};
use crate::input::context::KeyboardGrab;
use crate::input::events::{ActionFired, GrabbedKey, LiteralPrefix};
use crate::input::focus::{ActiveSurface, FocusArea};
use crate::input::vim::keyconv::bevy_to_key_event;
use crate::input::{Action, InputContext, SystemClipboard};
use crate::view::components::GlobalErrorQueue;
use crate::view::shell_dock::ShellDockMarker;

/// Everything a compose keystroke may touch.
#[derive(SystemParam)]
pub struct ComposeIo<'w, 's> {
    compose: ResMut<'w, ComposeState>,
    outbox: ResMut<'w, DraftOutboxState>,
    surface: ResMut<'w, ActiveSurface>,
    focus: ResMut<'w, FocusArea>,
    doc_cache: Res<'w, DocumentCache>,
    session_principal: Res<'w, SessionPrincipal>,
    actor: Option<Res<'w, RpcActor>>,
    rpc_results: Res<'w, RpcResultChannel>,
    scroll: ResMut<'w, ConversationScrollState>,
    errors: ResMut<'w, GlobalErrorQueue>,
    time: Res<'w, Time>,
    /// Actions a keystroke fires (`PopLevel`, the Ctrl bypasses), written
    /// by [`compose_dispatch_keys`]. The handlers that also take this
    /// parameter read `ActionFired`, so it cannot hold the writer itself.
    fired: Local<'s, Vec<Action>>,
    commands: Commands<'w, 's>,
    chat_overlay: Query<'w, 's, Entity, With<InputOverlayMarker>>,
}

impl ComposeIo<'_, '_> {
    fn error(&mut self, operation: &str, message: impl Into<String>) {
        let message = message.into();
        warn!("compose {operation}: {message}");
        self.errors.push(operation, message, self.time.elapsed_secs_f64());
    }

    /// Queue the chat buffer's edits on the draft outbox.
    fn send_chat_ops(&mut self, ops: &[EditOp]) {
        if ops.is_empty() {
            return;
        }
        let Some(context) = self.compose.ready_context() else {
            // Typed with no draft to hold it: the text stays on screen and
            // the player is told it is not saved.
            self.error("draft", "no context is attached; this text is not saved");
            return;
        };
        let ComposeIo { compose, outbox, .. } = self;
        if let Err(e) = outbox.send_ops(compose, context, ops) {
            self.error("draft", format!("draft edit failed: {e}"));
        }
    }

    /// Act on one keystroke's [`ComposeAction`] for `surface`.
    fn act(&mut self, surface: ActiveSurface, action: ComposeAction) {
        // The shell surface is local text until it is submitted.
        if !surface.is_shell() {
            self.send_chat_ops(&action.ops);
        }
        if let Some(line) = action.command {
            match colon_vi_statement(&line) {
                Some(statement) => self.run_colon_vi(statement),
                None => self.error("compose", format!("{line}: compose runs no `:` commands; Ctrl+Z is the shell")),
            }
        }
        if let Some(notice) = action.notice {
            self.error("compose", notice);
        }
        if action.submit {
            match surface {
                ActiveSurface::Chat => self.submit_chat(),
                ActiveSurface::Shell => self.submit_shell(),
            }
        }
        if action.dismiss {
            self.fired.push(Action::PopLevel);
        }
    }

    /// `:vi <args>` from the `:` bar: the shell statement `vi <args>`, run the
    /// way the shell surface runs any line. The builtin opens the editor, and
    /// its kernel open signal lands the editor on this window.
    fn run_colon_vi(&mut self, statement: String) {
        let (Some(actor), Some(context)) = (self.actor.as_ref(), self.doc_cache.active_id()) else {
            self.error("vi", "not connected; no editor was opened");
            return;
        };
        let handle = actor.handle.clone();
        let tx = self.rpc_results.sender();
        let span = tracing::info_span!("app.vi", context.id = %context);
        IoTaskPool::get()
            .spawn(tracing::Instrument::instrument(
                async move {
                    if let Err(e) = handle.shell_execute(&statement, context, true).await {
                        let _ = tx.send(RpcResultMessage::RpcError { operation: "vi".into(), error: e.to_string() });
                    }
                },
                span,
            ))
            .detach();
    }

    /// Feed one key to the active surface.
    fn press(&mut self, key: KeyEvent) {
        match *self.surface {
            ActiveSurface::Chat => self.press_chat(key),
            ActiveSurface::Shell => {
                let action = self.compose.press_shell(key);
                self.act(ActiveSurface::Shell, action);
            }
        }
    }

    /// Feed one key to the chat draft, whatever surface is active: held
    /// chat input replays here, never into the shell line.
    fn press_chat(&mut self, key: KeyEvent) {
        if let Some(action) = self.compose.press_chat(key) {
            self.act(ActiveSurface::Chat, action);
        }
    }

    /// Paste `text` into the active surface as one edit.
    fn paste(&mut self, text: &str) {
        match *self.surface {
            ActiveSurface::Chat => self.paste_chat(text),
            ActiveSurface::Shell => {
                self.compose.shell.paste(text);
            }
        }
    }

    fn paste_chat(&mut self, text: &str) {
        if let Some(ops) = self.compose.paste_chat(text) {
            self.send_chat_ops(&ops);
        }
    }

    /// Submit the chat draft: the outbox queues the submit behind every edit
    /// already sent, and the buffer starts over at once.
    fn submit_chat(&mut self) {
        if self.compose.chat.text().trim().is_empty() {
            return;
        }
        let Some(context) = self.compose.ready_context() else {
            self.error("submit", "no context is attached; nothing was submitted");
            return;
        };
        let Some(outbox) = self.outbox.outbox() else {
            self.error("submit", "not connected; nothing was submitted");
            return;
        };
        // No edge: the app does not record which block it last drew, and
        // the kernel never guesses one (`docs/prompts.md`, "The submit verb").
        let submitted = outbox.submit(context, None);
        self.compose.begin_submit();
        let tx = self.rpc_results.sender();
        let span = tracing::info_span!("app.submit", context.id = %context, surface = "chat");
        IoTaskPool::get()
            .spawn(tracing::Instrument::instrument(
                async move {
                    let result = submitted.await.map_err(|e| e.to_string());
                    let _ = tx.send(RpcResultMessage::DraftSubmitted { context_id: context, result });
                },
                span,
            ))
            .detach();
        // A submit is a strong sign the player wants to watch the result.
        self.scroll.start_following();
        *self.focus = FocusArea::Conversation;
    }

    /// Submit the shell line. A plain `kj` line runs through `execute_kj`, so
    /// its result data can say where to go next; any other line runs in the
    /// shell. The line is cleared at once and restored if the call fails.
    fn submit_shell(&mut self) {
        let code = self.compose.shell.text();
        if code.trim().is_empty() {
            return;
        }
        let (Some(actor), Some(context)) = (self.actor.as_ref(), self.doc_cache.active_id()) else {
            self.error("shell", "not connected; nothing was run");
            return;
        };
        let Some(principal_id) = self.session_principal.0 else {
            self.error("shell", "cannot run until the authenticated connection identity is known");
            return;
        };
        let handle = actor.handle.clone();
        let tx = self.rpc_results.sender();
        let span = tracing::info_span!("app.submit", principal.id = %principal_id, context.id = %context, surface = "shell");
        let restore = move |error: kaijutsu_client::CallError, text: String| {
            let reason = error.to_string();
            if submission_needs_restore(&error) {
                RpcResultMessage::SubmitFailed { text, reason, context_id: context, principal_id }
            } else {
                RpcResultMessage::RpcError { operation: "shell submit".into(), error: reason }
            }
        };
        let task = match shell_kj_argv(&code) {
            Some(argv) => IoTaskPool::get().spawn(tracing::Instrument::instrument(
                async move {
                    let message = match handle.execute_kj(context, argv).await {
                        Ok(result) => RpcResultMessage::KjLineFinished { from: context, result },
                        Err(e) => restore(e, code),
                    };
                    let _ = tx.send(message);
                },
                span,
            )),
            None => IoTaskPool::get().spawn(tracing::Instrument::instrument(
                async move {
                    match handle.shell_execute(&code, context, true).await {
                        Ok(block_id) => log::info!("shell_execute ok: {block_id:?}"),
                        Err(e) => {
                            let _ = tx.send(restore(e, code));
                        }
                    }
                },
                span,
            )),
        };
        task.detach();
        self.compose.shell = ComposeBuffer::default();
        self.scroll.start_following();
        *self.focus = FocusArea::Conversation;
    }

    /// A draft read or a submit came back.
    fn land(&mut self, result: &RpcResultMessage) {
        match result {
            RpcResultMessage::DraftRead { context_id, read, result } => {
                let text = match result {
                    Ok(text) => Some(text.as_str()),
                    Err(e) => {
                        self.error("draft", format!("the draft of {} was not read: {e}", context_id.short()));
                        None
                    }
                };
                let Some(held) = self.compose.finish_load(*context_id, *read, text) else {
                    return;
                };
                for input in held {
                    match input {
                        HeldInput::Key(key) => self.press_chat(key),
                        HeldInput::Paste(text) => self.paste_chat(&text),
                    }
                }
            }
            RpcResultMessage::DraftSubmitted { context_id, result } => match result {
                Ok(block) => self.compose.finish_submit(Some(*block)),
                Err(e) => {
                    self.compose.finish_submit(None);
                    self.error("submit", format!("submit failed: {e}"));
                    if let Ok(entity) = self.chat_overlay.single() {
                        self.commands.entity(entity).insert(ComposeError { started: std::time::Instant::now() });
                    }
                    // The kernel keeps a draft whose submit failed; read it
                    // back rather than guess what it holds.
                    if let Some((context, principal)) = self.compose.draft().owner()
                        && context == *context_id
                    {
                        self.load_draft(context, principal);
                    }
                }
            },
            _ => {}
        }
    }

    /// Start reading `context`'s draft through the outbox, behind every
    /// edit already queued for it.
    fn load_draft(&mut self, context: ContextId, principal: kaijutsu_types::PrincipalId) {
        let Some(outbox) = self.outbox.outbox() else {
            return;
        };
        let reading = outbox.read(context);
        let LoadStarted { read, dropped } = self.compose.begin_load(context, principal);
        if dropped > 0 {
            self.error("draft", format!("{dropped} keys typed for the last draft were dropped before it loaded"));
        }
        let tx = self.rpc_results.sender();
        IoTaskPool::get()
            .spawn(async move {
                let result = reading.await.map_err(|e| e.to_string());
                let _ = tx.send(RpcResultMessage::DraftRead { context_id: context, read, result });
            })
            .detach();
    }
}

/// Whether a failed shell submission gives its text back. A pending review
/// keeps the submission, so its text stays out of the line.
fn submission_needs_restore(error: &kaijutsu_client::CallError) -> bool {
    !matches!(error, kaijutsu_client::CallError::Refused(refusal) if refusal.is_pending())
}

/// The compose grab's key path. Draft reads and submit answers land here
/// too, so input held while a draft loaded replays through the same path.
///
/// Runs every frame and reads every `GrabbedKey`; it acts on them only
/// while compose holds the grab, so a key meant for the editor is never
/// read here later.
pub fn compose_dispatch_keys(
    grab: Res<KeyboardGrab>,
    keys: Res<ButtonInput<KeyCode>>,
    mut keyboard: MessageReader<GrabbedKey>,
    mut literal: MessageReader<LiteralPrefix>,
    mut results: MessageReader<RpcResultMessage>,
    mut actions: MessageWriter<ActionFired>,
    mut io: ComposeIo,
) {
    for result in results.read() {
        io.land(result);
    }
    let ours = *grab == KeyboardGrab::ComposeVim;
    // `Ctrl+A a`: a literal Ctrl+A for the vi engine (increment, i_CTRL-A).
    for _ in literal.read() {
        if ours {
            io.press(KeyEvent::new(CtKeyCode::Char('a'), KeyModifiers::CONTROL));
        }
    }
    for GrabbedKey(event) in keyboard.read() {
        if !ours {
            continue;
        }
        let ctrl = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
        // Ctrl+C, Ctrl+Z and Ctrl+V are the app's in every vi mode: the
        // interrupt ladder, the chat/shell toggle, and the clipboard paste.
        let bypass = match event.key_code {
            KeyCode::KeyC if ctrl => Some(Action::InterruptContext { immediate: false }),
            KeyCode::KeyZ if ctrl => Some(Action::ToggleSurface),
            KeyCode::KeyV if ctrl => Some(Action::Paste),
            _ => None,
        };
        if let Some(action) = bypass {
            io.compose.reset_escape();
            io.fired.push(action);
            continue;
        }
        if let Some(key) = bevy_to_key_event(event, &keys) {
            io.press(key);
        }
    }
    for action in io.fired.drain(..) {
        actions.write(ActionFired::new(action, InputContext::TextInput));
    }
}

/// Load the chat draft of the context on screen, for the connection's
/// principal, whenever either changes. With no context or no identity the
/// chat buffer has no draft to follow.
pub fn follow_draft_context(mut io: ComposeIo) {
    let wanted = io.doc_cache.active_id().zip(io.session_principal.0);
    if io.compose.draft().owner() == wanted {
        return;
    }
    match wanted {
        Some((context, principal)) => io.load_draft(context, principal),
        None => {
            let dropped = io.compose.detach();
            if !dropped.is_empty() {
                io.error("draft", "the context left before its draft loaded; the keys typed meanwhile were dropped");
            }
        }
    }
}

/// Take the change feed's copy of the chat draft: a sibling's typing, or
/// the kernel's own state after our edits land (`DraftLedger` decides when).
pub fn reconcile_chat_draft(mut compose: ResMut<ComposeState>, doc_cache: Res<DocumentCache>) {
    let DraftTarget::Ready { context, principal } = compose.draft() else {
        return;
    };
    let Some(cached) = doc_cache.get(context) else {
        return;
    };
    let version = cached.mirror.version();
    let Some(text) = compose.draft_in(cached.mirror.blocks(), principal).map(str::to_string) else {
        return;
    };
    compose.offer_mirror(&text, version);
}

/// Read the acks for our draft edits. A failed one is never silent: it is
/// the only sign that the kernel's draft and ours have parted, or that the
/// writer stopped.
pub fn drain_draft_acks(
    mut outbox: ResMut<DraftOutboxState>,
    mut compose: ResMut<ComposeState>,
    mut errors: ResMut<GlobalErrorQueue>,
    time: Res<Time>,
) {
    for landed in outbox.drain() {
        match landed.result {
            Ok(version) => {
                compose.landed(landed.generation, Some(version));
            }
            Err(e) => {
                compose.landed(landed.generation, None);
                warn!("draft edit for {} failed: {e}", landed.context_id.short());
                errors.push("draft", format!("draft edit failed: {e}"), time.elapsed_secs_f64());
            }
        }
    }
}

/// `i`/Space from the conversation, and `SummonChat`: open the chat surface,
/// in insert mode when it is empty and normal mode otherwise.
pub fn handle_focus_compose(mut actions: MessageReader<ActionFired>, mut io: ComposeIo) {
    for ActionFired { action, .. } in actions.read() {
        if !matches!(action, Action::FocusCompose | Action::SummonChat) {
            continue;
        }
        *io.focus = FocusArea::Compose;
        *io.surface = ActiveSurface::Chat;
        io.compose.reset_escape();
        let ops = io.compose.chat.settle_for_focus();
        io.send_chat_ops(&ops);
    }
}

/// `Ctrl+Z`: toggle between the chat and shell surfaces. Each keeps its own
/// text; the one summoned opens as [`handle_focus_compose`] opens chat.
pub fn handle_toggle_surface(mut actions: MessageReader<ActionFired>, mut io: ComposeIo) {
    for ActionFired { action, .. } in actions.read() {
        if !matches!(action, Action::ToggleSurface) {
            continue;
        }
        io.surface.toggle();
        *io.focus = FocusArea::Compose;
        io.compose.reset_escape();
        if io.surface.is_shell() {
            io.compose.shell.settle_for_focus();
        } else {
            let ops = io.compose.chat.settle_for_focus();
            io.send_chat_ops(&ops);
        }
    }
}

/// `PopLevel` while composing: back to the conversation in normal mode, and
/// from the shell surface back to chat as the default.
pub fn handle_pop_level(mut actions: MessageReader<ActionFired>, mut io: ComposeIo) {
    for ActionFired { action, .. } in actions.read() {
        if !matches!(action, Action::PopLevel) || !matches!(*io.focus, FocusArea::Compose) {
            continue;
        }
        *io.focus = FocusArea::Conversation;
        if io.surface.is_shell() {
            io.compose.shell.leave_to_normal();
            *io.surface = ActiveSurface::Chat;
        } else {
            let ops = io.compose.chat.leave_to_normal();
            io.send_chat_ops(&ops);
        }
    }
}

/// The prefilled-`kj` prompt (`docs/input.md`, "The prefix table"): open the
/// shell surface with a command typed up to its argument, in insert mode at
/// the end. Enter runs it; Esc Esc abandons it.
pub fn handle_prompt_prefill(mut actions: MessageReader<ActionFired>, mut io: ComposeIo) {
    for ActionFired { action, .. } in actions.read() {
        let prefill = match action {
            Action::PromptContextRename => "kj context rename ",
            Action::PromptContextSwitch => "kj context switch ",
            Action::PromptContextRotate => "kj context rotate ",
            _ => continue,
        };
        *io.surface = ActiveSurface::Shell;
        *io.focus = FocusArea::Compose;
        io.compose.reset_escape();
        io.compose.shell = ComposeBuffer::prefilled(prefill);
    }
}

/// Xterm-style paste (`docs/input.md`): `Ctrl+V` reads CLIPBOARD, a middle
/// click reads PRIMARY. The text lands at the cursor as one edit.
pub fn handle_compose_paste(
    mut actions: MessageReader<ActionFired>,
    mut clipboard: Option<ResMut<SystemClipboard>>,
    mut io: ComposeIo,
) {
    for ActionFired { action, .. } in actions.read() {
        if !matches!(action, Action::Paste | Action::PastePrimary) {
            continue;
        }
        let Some(clip) = clipboard.as_mut() else {
            io.error("paste", "no clipboard is available");
            continue;
        };
        let fetched = if matches!(action, Action::PastePrimary) {
            crate::input::systems::primary_selection_text(&mut clip.0)
        } else {
            clip.0.get_text()
        };
        match fetched {
            Ok(text) => io.paste(&text),
            Err(e) => io.error("paste", format!("paste failed: {e}")),
        }
    }
}

/// The third `Ctrl+C`: clear the surface. The chat draft is cleared with
/// one edit queued behind the keystrokes that wrote it.
pub fn clear_compose(io: &mut ComposeIo) {
    if io.surface.is_shell() {
        io.compose.shell = ComposeBuffer::default();
    } else if let Some(op) = io.compose.clear_chat() {
        io.send_chat_ops(&[op]);
    }
}

/// Draw both surfaces from their buffers: text, cursor, selection and mode.
/// While the `/`·`?`·`:` bar is open the surface shows the bar's line.
pub fn project_compose_overlays(
    mut compose: ResMut<ComposeState>,
    mut chat: Query<&mut InputOverlay, With<InputOverlayMarker>>,
    mut shell: Query<&mut InputOverlay, (With<ShellDockMarker>, Without<InputOverlayMarker>)>,
) {
    let ComposeState { chat: chat_buffer, shell: shell_buffer, .. } = &mut *compose;
    if let Ok(mut overlay) = chat.single_mut() {
        project(chat_buffer, &mut overlay);
    }
    if let Ok(mut overlay) = shell.single_mut() {
        project(shell_buffer, &mut overlay);
    }
}

fn project(buffer: &mut ComposeBuffer, overlay: &mut Mut<InputOverlay>) {
    let (text, cursor, selection, mode) = match buffer.command_line() {
        Some(line) => {
            let end = line.len();
            (line, end, None, Some(BAR_MODE.to_string()))
        }
        None => {
            let text = buffer.text();
            let cursor = byte_offset(&text, buffer.cursor());
            let selection = buffer
                .selection()
                .and_then(|s| s.spans.first().cloned())
                .map(|span| (byte_offset(&text, span.start), byte_offset(&text, span.end)));
            (text, cursor, selection, buffer.mode())
        }
    };
    if overlay.text != text || overlay.cursor != cursor || overlay.selection != selection || overlay.vim_mode != mode {
        overlay.text = text;
        overlay.cursor = cursor;
        overlay.selection = selection;
        overlay.vim_mode = mode;
    }
}

/// The mode the overlay draws while the bar is open: typing goes into the
/// bar, so the cursor is the insert beam and the dock says INSERT. The vi
/// engine names no mode of its own for the bar.
const BAR_MODE: &str = "-- INSERT --";

#[cfg(test)]
mod tests;
