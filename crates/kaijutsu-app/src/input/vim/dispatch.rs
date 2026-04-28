//! Vim dispatch system for compose input.
//!
//! When the compose overlay is focused, keyboard events are routed through
//! the VimMachine instead of the flat binding table. This system converts
//! Bevy keyboard events to TerminalKey, feeds them to the state machine,
//! and translates the resulting modalkit Actions into kaijutsu ActionFired
//! or TextInputReceived messages.

use bevy::input::keyboard::KeyboardInput;
use bevy::prelude::*;

use editor_types::{
    Action as MkAction, EditAction, EditorAction, InsertTextAction, PromptAction,
};
use editor_types::context::Resolve;
use editor_types::prelude::{Char, EditTarget, MoveDir1D, RangeType, Specifier};
use modalkit::editing::context::EditContext;
use modalkit::keybindings::BindingMachine;

use super::keyconv::bevy_to_terminal_key;
use super::motion::{self, MotionContext};
use super::textutil;
use super::{KaijutsuAction, KaijutsuInfo, VimMachineResource};
use crate::input::action::Action;
use crate::input::events::{ActionFired, TextInputReceived};

/// Per-overlay vim editing state.
/// Tracks desired_col for j/k column memory and the unnamed yank register.
#[derive(Resource, Default)]
pub struct VimMotionState {
    pub desired_col: Option<usize>,
    /// Unnamed register — holds last deleted/yanked text.
    pub register: String,
    /// Whether the register content is linewise (for `p`/`P` paste behavior).
    pub register_linewise: bool,
}

/// Resolve an EditTarget to a byte range (start, end) in the overlay text.
/// Returns None if the target can't be resolved.
fn resolve_target_range(
    text: &str,
    cursor: usize,
    target: &EditTarget,
    ctx: &EditContext,
    motion_state: &VimMotionState,
) -> Option<(usize, usize, bool)> {
    // Returns (start, end, linewise)
    match target {
        EditTarget::Motion(move_type, count) => {
            let motion_ctx = MotionContext {
                desired_col: motion_state.desired_col,
            };
            let result = motion::resolve_motion(text, cursor, move_type, count, ctx, &motion_ctx);
            let (start, end) = if result.cursor < cursor {
                (result.cursor, cursor)
            } else {
                (cursor, result.cursor)
            };
            // Inclusive motions (e/$/g_) consume the char at the destination;
            // exclusive motions (w/b/h/l/0/^) stop short of it.
            let end = if motion::is_inclusive(move_type) && end < text.len() {
                textutil::next_char_boundary(text, end)
            } else {
                end
            };
            Some((start, end, false))
        }
        EditTarget::Range(RangeType::Line, _inclusive, count) => {
            let n: usize = ctx.resolve(count);
            let cur_line = textutil::line_of(text, cursor);
            let end_line = (cur_line + n.max(1) - 1).min(textutil::line_count(text).saturating_sub(1));
            let start = textutil::line_start(text, cur_line);
            let end = textutil::line_end(text, end_line);
            // Include the trailing newline if there is one
            let end = if end < text.len() && text.as_bytes().get(end) == Some(&b'\n') {
                end + 1
            } else if start > 0 && start <= text.len() {
                // Last line with no trailing newline — eat the preceding newline instead
                // so that dd on the last line doesn't leave a trailing blank
                let adjusted_start = textutil::prev_char_boundary(text, start);
                return Some((adjusted_start, end, true));
            } else {
                end
            };
            Some((start, end, true))
        }
        EditTarget::CurrentPosition => {
            // x — delete char at cursor
            if cursor < text.len() {
                let end = textutil::next_char_boundary(text, cursor);
                Some((cursor, end, false))
            } else {
                None
            }
        }
        _ => {
            log::trace!("vim: unhandled EditTarget for range: {:?}", target);
            None
        }
    }
}

/// Apply a text deletion to the overlay and fire the CRDT sync RPC.
///
/// `skip_crdt`: true for shell surface (local-only, no CRDT sync).
fn apply_delete(
    overlay: &mut crate::cell::InputOverlay,
    start: usize,
    end: usize,
    actor: &Option<Res<crate::connection::RpcActor>>,
    doc_cache: &crate::cell::DocumentCache,
    skip_crdt: bool,
) {
    let del_len = end - start;
    if del_len == 0 {
        return;
    }

    // Update local overlay
    overlay.text.drain(start..end);
    overlay.cursor = start.min(overlay.text.len());
    overlay.selection_anchor = None;

    // Fire CRDT sync (chat only)
    if !skip_crdt {
        if let (Some(actor), Some(ctx_id)) = (actor, doc_cache.active_id()) {
            let handle = actor.handle.clone();
            let pos = start as u64;
            let delete = del_len as u64;
            bevy::tasks::IoTaskPool::get()
                .spawn(async move {
                    if let Err(e) = handle.edit_input(ctx_id, pos, "", delete).await {
                        log::warn!("edit_input (vim delete) failed: {e}");
                    }
                })
                .detach();
        }
    }
}

/// Apply a text insertion to the overlay and fire the CRDT sync RPC.
///
/// `skip_crdt`: true for shell surface (local-only, no CRDT sync).
fn apply_insert(
    overlay: &mut crate::cell::InputOverlay,
    pos: usize,
    text_to_insert: &str,
    actor: &Option<Res<crate::connection::RpcActor>>,
    doc_cache: &crate::cell::DocumentCache,
    skip_crdt: bool,
) {
    if text_to_insert.is_empty() {
        return;
    }

    overlay.text.insert_str(pos, text_to_insert);
    overlay.cursor = pos + text_to_insert.len();
    overlay.selection_anchor = None;

    if !skip_crdt {
        if let (Some(actor), Some(ctx_id)) = (actor, doc_cache.active_id()) {
            let handle = actor.handle.clone();
            let p = pos as u64;
            let insert = text_to_insert.to_string();
            bevy::tasks::IoTaskPool::get()
                .spawn(async move {
                    if let Err(e) = handle.edit_input(ctx_id, p, &insert, 0).await {
                        log::warn!("edit_input (vim insert) failed: {e}");
                    }
                })
                .detach();
        }
    }
}

/// In Normal mode, cursor must be ON a printable character, not past end
/// or on a newline (which belongs to the next line's boundary).
/// In Insert mode, cursor can be at text.len() (appending position).
/// Call after any mutation to enforce the invariant based on current vim mode.
fn clamp_normal_cursor(overlay: &mut crate::cell::InputOverlay) {
    if overlay.vim_mode.is_some() || overlay.text.is_empty() {
        return;
    }
    // Can't be past end of text
    if overlay.cursor >= overlay.text.len() {
        overlay.cursor = textutil::prev_char_boundary(&overlay.text, overlay.text.len());
    }
    // Can't rest on a newline in Normal mode — back up to last char on line
    if overlay.cursor < overlay.text.len()
        && overlay.text.as_bytes()[overlay.cursor] == b'\n'
        && overlay.cursor > 0
    {
        overlay.cursor = textutil::prev_char_boundary(&overlay.text, overlay.cursor);
    }
}

/// Bevy system that dispatches keyboard input through the VimMachine
/// when the compose overlay is focused.
///
/// Run condition: `in_compose()` — only active when FocusArea::Compose.
/// Routes keyboard to the correct overlay (chat or shell) based on ActiveSurface.
pub fn vim_dispatch_compose(
    mut keyboard: MessageReader<KeyboardInput>,
    keys: Res<ButtonInput<KeyCode>>,
    mut vim: ResMut<VimMachineResource>,
    mut motion_state: ResMut<VimMotionState>,
    mut chat_overlay: Query<
        &mut crate::cell::InputOverlay,
        With<crate::cell::InputOverlayMarker>,
    >,
    mut shell_overlay: Query<
        &mut crate::cell::InputOverlay,
        (
            With<crate::view::shell_dock::ShellDockMarker>,
            Without<crate::cell::InputOverlayMarker>,
        ),
    >,
    surface: Res<crate::input::focus::ActiveSurface>,
    actor: Option<Res<crate::connection::RpcActor>>,
    doc_cache: Res<crate::cell::DocumentCache>,
    mut action_writer: MessageWriter<ActionFired>,
    mut text_writer: MessageWriter<TextInputReceived>,
) {
    // Select the active overlay based on ActiveSurface
    let mut overlay = if surface.is_shell() {
        match shell_overlay.single_mut() {
            Ok(o) => o,
            Err(_) => return,
        }
    } else {
        match chat_overlay.single_mut() {
            Ok(o) => o,
            Err(_) => return,
        }
    };

    for event in keyboard.read() {
        if !event.state.is_pressed() {
            continue;
        }

        let ctrl = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);

        // Ctrl+C bypasses VimMachine — interrupt must always work
        // regardless of vim state (operator-pending, visual, etc.).
        if ctrl && event.key_code == KeyCode::KeyC {
            action_writer.write(ActionFired(Action::InterruptContext { immediate: false }));
            continue;
        }

        // Ctrl+Z bypasses VimMachine — toggle between chat and shell surface.
        if ctrl && event.key_code == KeyCode::KeyZ {
            action_writer.write(ActionFired(Action::ToggleSurface));
            continue;
        }

        let Some(tkey) = bevy_to_terminal_key(event, &keys) else {
            continue;
        };

        // Allow key repeats in all vim modes — holding h/j/k/l to scrub
        // is fundamental to vim muscle memory, and modalkit handles repeat
        // semantics (operator-pending won't re-fire on repeat).

        vim.machine.input_key(tkey);

        // Update vim mode display on overlay
        overlay.vim_mode = vim.machine.show_mode();

        // Drain all produced actions
        while let Some((mk_action, ctx)) = vim.machine.pop() {
            translate_action(
                &mk_action,
                &ctx,
                &mut overlay,
                &mut motion_state,
                &actor,
                &doc_cache,
                &mut action_writer,
                &mut text_writer,
                surface.is_shell(),
            );
        }

        // Enforce cursor invariant after all mutations for this keystroke.
        clamp_normal_cursor(&mut overlay);
    }
}

/// Translate a modalkit Action into overlay mutations, ActionFired, or TextInputReceived.
fn translate_action(
    action: &MkAction<KaijutsuInfo>,
    ctx: &EditContext,
    overlay: &mut crate::cell::InputOverlay,
    motion_state: &mut VimMotionState,
    actor: &Option<Res<crate::connection::RpcActor>>,
    doc_cache: &crate::cell::DocumentCache,
    action_writer: &mut MessageWriter<ActionFired>,
    text_writer: &mut MessageWriter<TextInputReceived>,
    skip_crdt: bool,
) {
    match action {
        // --- Insert mode: character typing ---
        MkAction::Editor(EditorAction::InsertText(InsertTextAction::Type(spec, _dir, _count))) => {
            if let Specifier::Exact(ch) = spec {
                match ch {
                    Char::Single(c) => {
                        text_writer.write(TextInputReceived(c.to_string()));
                    }
                    Char::Digraph(a, b) => {
                        log::debug!("vim: digraph ({}, {}) not yet supported", a, b);
                    }
                    _ => {
                        log::debug!("vim: unsupported Char variant: {:?}", ch);
                    }
                }
            }
        }

        // --- Paste ---
        MkAction::Editor(EditorAction::InsertText(InsertTextAction::Paste(style, _count))) => {
            if motion_state.register.is_empty() {
                return;
            }
            let paste_text = motion_state.register.clone();
            if motion_state.register_linewise {
                // Linewise paste: insert on line below (p) or above (P)
                use editor_types::prelude::PasteStyle;
                let line = textutil::line_of(&overlay.text, overlay.cursor);
                let pos = match style {
                    PasteStyle::Side(MoveDir1D::Next) => {
                        // p — after current line
                        let end = textutil::line_end(&overlay.text, line);
                        if end < overlay.text.len() { end + 1 } else { end }
                    }
                    _ => {
                        // P — before current line
                        textutil::line_start(&overlay.text, line)
                    }
                };
                // Ensure newline at end of pasted text
                let text = if paste_text.ends_with('\n') {
                    paste_text
                } else {
                    format!("{}\n", paste_text)
                };
                apply_insert(overlay, pos, &text, actor, doc_cache, skip_crdt);
            } else {
                // Charwise paste: insert after cursor (p) or at cursor (P)
                use editor_types::prelude::PasteStyle;
                let pos = match style {
                    PasteStyle::Side(MoveDir1D::Next) => {
                        if overlay.cursor < overlay.text.len() {
                            textutil::next_char_boundary(&overlay.text, overlay.cursor)
                        } else {
                            overlay.cursor
                        }
                    }
                    _ => overlay.cursor,
                };
                apply_insert(overlay, pos, &paste_text, actor, doc_cache, skip_crdt);
            }
        }

        // --- Edit operations (motion, delete, yank, change) ---
        MkAction::Editor(EditorAction::Edit(op_spec, target)) => {
            let resolved_op: EditAction = ctx.resolve(op_spec);
            match resolved_op {
                // --- Motion only (cursor movement) ---
                EditAction::Motion => {
                    if let EditTarget::Motion(move_type, count) = target {
                        let motion_ctx = MotionContext {
                            desired_col: motion_state.desired_col,
                        };
                        let result = motion::resolve_motion(
                            &overlay.text,
                            overlay.cursor,
                            move_type,
                            count,
                            ctx,
                            &motion_ctx,
                        );
                        overlay.cursor = result.cursor.min(overlay.text.len());
                        motion_state.desired_col = result.desired_col;
                        overlay.selection_anchor = None;
                    }
                }

                // --- Delete ---
                EditAction::Delete => {
                    if let Some((start, end, linewise)) =
                        resolve_target_range(&overlay.text, overlay.cursor, target, ctx, motion_state)
                    {
                        // Yank deleted text to unnamed register
                        motion_state.register = overlay.text[start..end].to_string();
                        motion_state.register_linewise = linewise;
                        apply_delete(overlay, start, end, actor, doc_cache, skip_crdt);
                    }
                }

                // --- Yank ---
                EditAction::Yank => {
                    if let Some((start, end, linewise)) =
                        resolve_target_range(&overlay.text, overlay.cursor, target, ctx, motion_state)
                    {
                        motion_state.register = overlay.text[start..end].to_string();
                        motion_state.register_linewise = linewise;
                        // Yank doesn't modify text — just copies to register
                    }
                }

                _ => {
                    log::trace!("vim: unhandled edit op: {:?}", resolved_op);
                }
            }
        }

        // --- Submit (Enter with submit_on_enter) ---
        MkAction::Prompt(PromptAction::Submit) => {
            action_writer.write(ActionFired(Action::Submit));
        }

        // --- Prompt abort (Ctrl+D when empty, or Escape in command mode) ---
        MkAction::Prompt(PromptAction::Abort(..)) => {
            action_writer.write(ActionFired(Action::Unfocus));
        }

        // --- Application-specific actions ---
        MkAction::Application(app_action) => match app_action {
            KaijutsuAction::Submit => {
                action_writer.write(ActionFired(Action::Submit));
            }
            KaijutsuAction::DismissCompose => {
                action_writer.write(ActionFired(Action::Unfocus));
            }
        },

        // --- Editor actions not yet handled ---
        MkAction::Editor(_) => {
            log::trace!("vim: editor action not yet handled: {:?}", action);
        }

        // --- NoOp ---
        MkAction::NoOp => {}

        // --- Everything else: log for now ---
        _ => {
            log::trace!("vim: unhandled action: {:?}", action);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor_types::context::EditContextBuilder;
    use editor_types::prelude::{Count, MoveDir1D, MoveType, RangeType};

    fn ctx() -> EditContext {
        EditContextBuilder::default().build()
    }

    fn state() -> VimMotionState {
        VimMotionState::default()
    }

    // ── Motion targets ──
    //
    // Inclusivity comes from the MoveType variant via motion::is_inclusive:
    // w/b/h/l/0/^ are exclusive (don't include the destination char);
    // e/$/g_ are inclusive. EditContext.target_shape doesn't carry this.

    #[test]
    fn motion_forward_word_dw() {
        let text = "hello world";
        let (start, end, linewise) =
            resolve_target_range(text, 0, &EditTarget::Motion(
                MoveType::WordBegin(editor_types::prelude::WordStyle::Little, MoveDir1D::Next),
                Count::Contextual,
            ), &ctx(), &state()).unwrap();
        // dw on "hello world" from cursor 0: w lands at 6 ('w'), exclusive
        // → end=6, deletes "hello ".
        assert_eq!(start, 0);
        assert_eq!(end, 6);
        assert!(!linewise);
    }

    #[test]
    fn motion_backward_char_dh() {
        let text = "hello";
        let (start, end, linewise) =
            resolve_target_range(text, 3, &EditTarget::Motion(
                MoveType::Column(MoveDir1D::Previous, false),
                Count::Contextual,
            ), &ctx(), &state()).unwrap();
        // dh on "hello" from cursor 3 ('l'): h lands at 2, exclusive → end=3,
        // deletes the second 'l' only.
        assert_eq!(start, 2);
        assert_eq!(end, 3);
        assert!(!linewise);
    }

    #[test]
    fn motion_forward_char_dl() {
        let text = "hello";
        let (start, end, linewise) =
            resolve_target_range(text, 1, &EditTarget::Motion(
                MoveType::Column(MoveDir1D::Next, false),
                Count::Contextual,
            ), &ctx(), &state()).unwrap();
        // dl on "hello" from cursor 1 ('e'): l lands at 2 ('l'), exclusive →
        // end=2, deletes 'e' only.
        assert_eq!(start, 1);
        assert_eq!(end, 2);
        assert!(!linewise);
    }

    #[test]
    fn motion_word_end_de_inclusive() {
        let text = "hello world";
        let (start, end, linewise) =
            resolve_target_range(text, 0, &EditTarget::Motion(
                MoveType::WordEnd(editor_types::prelude::WordStyle::Little, MoveDir1D::Next),
                Count::Contextual,
            ), &ctx(), &state()).unwrap();
        // de on "hello world" from cursor 0: e lands at 4 ('o'), inclusive →
        // end=5, deletes "hello".
        assert_eq!(start, 0);
        assert_eq!(end, 5);
        assert!(!linewise);
    }

    #[test]
    fn motion_dollar_d_dollar_inclusive() {
        let text = "hello\nworld";
        let (start, end, linewise) =
            resolve_target_range(text, 1, &EditTarget::Motion(
                MoveType::LinePos(editor_types::prelude::MovePosition::End),
                Count::Contextual,
            ), &ctx(), &state()).unwrap();
        // d$ on "hello\nworld" from cursor 1 ('e'): $ lands at 4 ('o'),
        // inclusive → end=5, deletes "ello".
        assert_eq!(start, 1);
        assert_eq!(end, 5);
        assert!(!linewise);
    }

    #[test]
    fn motion_zero_d_zero_exclusive() {
        let text = "  hello";
        let (start, end, linewise) =
            resolve_target_range(text, 4, &EditTarget::Motion(
                MoveType::LinePos(editor_types::prelude::MovePosition::Beginning),
                Count::Contextual,
            ), &ctx(), &state()).unwrap();
        // d0 on "  hello" from cursor 4 ('l'): 0 lands at 0, exclusive →
        // end=4, deletes "  he".
        assert_eq!(start, 0);
        assert_eq!(end, 4);
        assert!(!linewise);
    }

    // ── Line range (dd) ──

    #[test]
    fn dd_first_line() {
        let text = "hello\nworld\nfoo";
        let (start, end, linewise) =
            resolve_target_range(text, 2, &EditTarget::Range(
                RangeType::Line, true, Count::Contextual,
            ), &ctx(), &state()).unwrap();
        assert_eq!(start, 0);
        assert_eq!(end, 6); // includes the \n
        assert!(linewise);
    }

    #[test]
    fn dd_middle_line() {
        let text = "hello\nworld\nfoo";
        let (start, end, linewise) =
            resolve_target_range(text, 8, &EditTarget::Range(
                RangeType::Line, true, Count::Contextual,
            ), &ctx(), &state()).unwrap();
        assert_eq!(start, 6);
        assert_eq!(end, 12); // includes the \n after "world"
        assert!(linewise);
    }

    #[test]
    fn dd_last_line_no_trailing_newline() {
        let text = "hello\nworld";
        let (start, end, linewise) =
            resolve_target_range(text, 8, &EditTarget::Range(
                RangeType::Line, true, Count::Contextual,
            ), &ctx(), &state()).unwrap();
        // Last line with no trailing \n — eats preceding \n
        assert_eq!(start, 5); // the \n before "world"
        assert_eq!(end, 11); // end of text
        assert!(linewise);
    }

    #[test]
    fn dd_single_line_no_newline() {
        let text = "hello";
        let (start, end, linewise) =
            resolve_target_range(text, 2, &EditTarget::Range(
                RangeType::Line, true, Count::Contextual,
            ), &ctx(), &state()).unwrap();
        // Only line, no newlines at all
        assert_eq!(start, 0);
        assert_eq!(end, 5);
        assert!(linewise);
    }

    // ── CurrentPosition (x) ──

    #[test]
    fn x_mid_text() {
        let text = "hello";
        let (start, end, linewise) =
            resolve_target_range(text, 2, &EditTarget::CurrentPosition, &ctx(), &state()).unwrap();
        assert_eq!(start, 2);
        assert_eq!(end, 3); // one char
        assert!(!linewise);
    }

    #[test]
    fn x_at_end() {
        let text = "hello";
        // cursor past end — x does nothing
        let result = resolve_target_range(text, 5, &EditTarget::CurrentPosition, &ctx(), &state());
        assert!(result.is_none());
    }

    #[test]
    fn x_utf8() {
        let text = "café"; // é is 2 bytes (bytes 3-4)
        let (start, end, linewise) =
            resolve_target_range(text, 3, &EditTarget::CurrentPosition, &ctx(), &state()).unwrap();
        assert_eq!(start, 3);
        assert_eq!(end, 5); // é is 2 bytes
        assert!(!linewise);
    }

    #[test]
    fn x_empty_text() {
        let result = resolve_target_range("", 0, &EditTarget::CurrentPosition, &ctx(), &state());
        assert!(result.is_none());
    }

    // ── Backspace in Insert mode ──
    //
    // Verify what modalkit emits when Backspace is pressed in Insert mode.
    // This is the verification test called out in docs/issues.md:136-137.
    // If the emitted action is `EditAction::Delete` with a `Column(Previous)`
    // motion target, the existing Delete arm in `translate_action` (combined
    // with the exclusive-motion fix) handles it correctly.

    use modalkit::crossterm::event::{KeyCode as CtKeyCode, KeyEvent, KeyModifiers};
    use modalkit::editing::store::Store;
    use modalkit::env::vim::keybindings::{VimBindings, VimMachine};
    use modalkit::key::TerminalKey;
    use modalkit::keybindings::{BindingMachine, InputBindings};
    use editor_types::Action as MkAction;
    use editor_types::EditorAction;
    use crate::input::vim::{KaijutsuInfo, KaijutsuStore};

    fn make_machine() -> (VimMachine<TerminalKey, KaijutsuInfo>, Store<KaijutsuInfo>) {
        let mut machine = VimMachine::<TerminalKey, KaijutsuInfo>::empty();
        VimBindings::default().submit_on_enter().setup(&mut machine);
        let store = Store::new(KaijutsuStore);
        (machine, store)
    }

    fn key(code: CtKeyCode) -> TerminalKey {
        TerminalKey::from(KeyEvent::new(code, KeyModifiers::empty()))
    }

    /// Drain all actions the VimMachine has buffered into a Vec.
    fn drain_actions(
        machine: &mut VimMachine<TerminalKey, KaijutsuInfo>,
    ) -> Vec<MkAction<KaijutsuInfo>> {
        let mut out = Vec::new();
        while let Some((action, _ctx)) = machine.pop() {
            out.push(action);
        }
        out
    }

    #[test]
    fn backspace_insert_mode_emits_delete() {
        let (mut machine, _store) = make_machine();

        // Enter Insert mode with `i`.
        machine.input_key(key(CtKeyCode::Char('i')));
        let _ = drain_actions(&mut machine);
        assert_eq!(
            machine.show_mode().as_deref(),
            Some("-- INSERT --"),
            "expected INSERT mode after pressing 'i'"
        );

        // Press Backspace.
        machine.input_key(key(CtKeyCode::Backspace));
        let actions = drain_actions(&mut machine);

        // Modalkit should produce at least one Editor action containing a
        // delete (either an Edit(Delete, Motion) or an InsertText delete
        // variant). Confirm something edit-related came out so we can wire
        // it through translate_action — pin the exact variant once we know
        // what modalkit produces.
        let saw_editor = actions.iter().any(|a| matches!(a, MkAction::Editor(_)));
        assert!(
            saw_editor,
            "Backspace in Insert mode produced no editor action; \
             actions = {:#?}",
            actions
        );
    }

    #[test]
    fn backspace_insert_mode_action_shape() {
        // Pin the exact shape of the emitted action so future modalkit
        // changes don't silently break Insert-mode Backspace.
        let (mut machine, _store) = make_machine();
        machine.input_key(key(CtKeyCode::Char('i')));
        let _ = drain_actions(&mut machine);
        machine.input_key(key(CtKeyCode::Backspace));
        let actions = drain_actions(&mut machine);

        // Find the editor action — print debug for diagnostics if shape changes.
        let editor_action = actions
            .iter()
            .find_map(|a| match a {
                MkAction::Editor(e) => Some(e),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no editor action; got {:#?}", actions));

        // We expect Edit(EditAction::Delete, EditTarget::Motion(Column(Previous), _))
        // — the standard "delete one char to the left" shape. If modalkit
        // emits something else (e.g. an InsertText delete variant), this
        // assertion fails and we add a new translate_action arm.
        match editor_action {
            EditorAction::Edit(op_spec, target) => {
                let is_column_prev = matches!(
                    target,
                    EditTarget::Motion(MoveType::Column(MoveDir1D::Previous, _), _)
                );
                assert!(
                    is_column_prev,
                    "expected Edit(_, Motion(Column(Previous, _), _)); got Edit(_, {:?})",
                    target
                );

                // The op spec must resolve to Delete — that's what makes
                // translate_action's Delete arm handle it correctly.
                let resolved: EditAction = ctx().resolve(op_spec);
                assert!(
                    matches!(resolved, EditAction::Delete),
                    "expected op to resolve to EditAction::Delete; got {:?}",
                    resolved
                );
            }
            other => {
                panic!(
                    "expected EditorAction::Edit(...); got {:?}\nfull actions = {:#?}",
                    other, actions
                );
            }
        }
    }

    #[test]
    fn backspace_range_is_one_char() {
        // End-to-end of the data flow inside resolve_target_range: given
        // the action shape modalkit emits for Insert-mode Backspace, the
        // resolved range should be exactly one char before the cursor.
        // This is the property that makes Backspace work post-Item-1.
        let text = "hello";
        let target = EditTarget::Motion(
            MoveType::Column(MoveDir1D::Previous, false),
            Count::Contextual,
        );
        let (start, end, linewise) =
            resolve_target_range(text, 3, &target, &ctx(), &state()).unwrap();
        assert_eq!(start, 2);
        assert_eq!(end, 3);
        assert!(!linewise);
    }
}
