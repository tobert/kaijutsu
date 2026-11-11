use super::*;
use kaijutsu_types::Status;

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

/// Type `keys` one char at a time; returns every edit they made.
fn typed(buffer: &mut ComposeBuffer, keys: &str) -> Vec<EditOp> {
    keys.chars().flat_map(|c| buffer.press(Submit::FromNormalMode, press(KeyCode::Char(c))).ops).collect()
}

fn ready(state: &mut ComposeState, text: &str) -> ContextId {
    let context = ContextId::new();
    let read = state.begin_load(context, PrincipalId::new()).read;
    state.finish_load(context, read, Some(text)).expect("the newest read");
    context
}

fn block(status: Status, content: &str, principal: PrincipalId) -> BlockSnapshot {
    let id = BlockId::new(ContextId::new(), principal, 1);
    kaijutsu_types::BlockSnapshotBuilder::new(id, kaijutsu_types::BlockKind::Text)
        .status(status)
        .content(content)
        .build()
}

/// Every keystroke in insert mode is one char-indexed edit, the kernel's
/// `edit_input` addressing.
#[test]
fn typing_makes_one_char_indexed_edit_per_key() {
    let mut buffer = ComposeBuffer::default();
    buffer.press(Submit::FromNormalMode, press(KeyCode::Char('i')));
    let ops = typed(&mut buffer, "café!");
    assert_eq!(ops.len(), 5);
    assert_eq!(ops.last().unwrap(), &EditOp { offset: 4, insert: "!".into(), delete: 0 });
    assert_eq!(buffer.text(), "café!");
}

/// Chat matches the tui: Enter submits from normal mode, and in insert mode
/// it is the vi engine's own Enter, a newline.
#[test]
fn chat_enter_submits_from_normal_mode_and_is_a_newline_in_insert() {
    let mut buffer = ComposeBuffer::default();
    buffer.press(Submit::FromNormalMode, press(KeyCode::Char('i')));
    typed(&mut buffer, "one");
    let newline = buffer.press(Submit::FromNormalMode, press(KeyCode::Enter));
    assert!(!newline.submit, "insert mode types the newline");
    typed(&mut buffer, "two");
    assert_eq!(buffer.text(), "one\ntwo");
    buffer.press(Submit::FromNormalMode, press(KeyCode::Esc));
    let enter = buffer.press(Submit::FromNormalMode, press(KeyCode::Enter));
    assert!(enter.submit && enter.ops.is_empty());
    assert_eq!(buffer.text(), "one\ntwo", "a submit edits nothing");
}

/// The shell line matches the tui's `:` bar: Enter runs it in any mode.
#[test]
fn the_shell_line_runs_on_enter_in_any_mode() {
    let mut state = ComposeState { shell: ComposeBuffer::prefilled("kj context rotate "), ..Default::default() };
    let enter = state.press_shell(press(KeyCode::Enter));
    assert!(enter.submit && enter.ops.is_empty(), "insert mode runs the line");
    assert_eq!(state.shell.text(), "kj context rotate ");
    ready(&mut state, "");
    state.chat.press(Submit::FromNormalMode, press(KeyCode::Char('i')));
    assert!(!state.press_chat(press(KeyCode::Enter)).unwrap().submit, "chat types the newline");
}

/// `u` and `Ctrl-r` reach the draft: undo and redo, each as edits.
#[test]
fn undo_and_redo_reach_the_buffer() {
    let mut buffer = ComposeBuffer::default();
    buffer.press(Submit::FromNormalMode, press(KeyCode::Char('i')));
    typed(&mut buffer, "hello");
    buffer.press(Submit::FromNormalMode, press(KeyCode::Esc));
    let undo = buffer.press(Submit::FromNormalMode, press(KeyCode::Char('u')));
    assert_eq!(buffer.text(), "");
    assert!(!undo.ops.is_empty(), "the undo is sent as an edit");
    buffer.press(Submit::FromNormalMode, ctrl('r'));
    assert_eq!(buffer.text(), "hello");
}

/// Visual mode selects, and `d` deletes the selection as one edit.
#[test]
fn a_visual_delete_removes_the_selection() {
    let mut buffer = ComposeBuffer::over("one two three");
    typed(&mut buffer, "0wv");
    assert!(buffer.selection().is_some());
    typed(&mut buffer, "e");
    let ops = typed(&mut buffer, "d");
    assert_eq!(buffer.text(), "one  three");
    assert_eq!(ops, vec![EditOp { offset: 4, insert: String::new(), delete: 3 }]);
    assert!(buffer.selection().is_none(), "the delete leaves visual mode");
}

/// `/` opens the bar, Enter searches, `n` and `N` repeat; the bar never
/// touches the text, and Enter on the bar is not a submit.
#[test]
fn search_moves_the_cursor_and_never_submits() {
    let mut buffer = ComposeBuffer::over("alpha beta alpha beta");
    typed(&mut buffer, "0/beta");
    assert_eq!(buffer.command_line().as_deref(), Some("/beta"));
    let enter = buffer.press(Submit::FromNormalMode, press(KeyCode::Enter));
    assert!(!enter.submit, "Enter on the bar searches");
    assert_eq!(buffer.cursor(), 6);
    typed(&mut buffer, "n");
    assert_eq!(buffer.cursor(), 17);
    typed(&mut buffer, "N");
    assert_eq!(buffer.cursor(), 6);
    assert_eq!(buffer.text(), "alpha beta alpha beta");
    typed(&mut buffer, "/zeta");
    let missing = buffer.press(Submit::FromNormalMode, press(KeyCode::Enter));
    assert_eq!(missing.notice.as_deref(), Some("Pattern not found: zeta"));
}

/// `.` repeats the last change.
#[test]
fn dot_repeats_the_last_change() {
    let mut buffer = ComposeBuffer::over("abcdef");
    typed(&mut buffer, "0x..");
    assert_eq!(buffer.text(), "def");
}

/// A `:` line comes back raw: the app runs none of the editor's dialect.
#[test]
fn a_colon_line_comes_back_raw() {
    let mut buffer = ComposeBuffer::over("draft");
    typed(&mut buffer, ":w");
    let action = buffer.press(Submit::FromNormalMode, press(KeyCode::Enter));
    assert_eq!(action.command.as_deref(), Some(":w"));
    assert!(!action.submit);
    assert_eq!(buffer.text(), "draft");
}

/// Esc Esc in normal mode dismisses; `Esc x Esc` does not, and neither does
/// a lone Esc out of insert mode.
#[test]
fn a_double_escape_in_normal_mode_dismisses() {
    let mut state = ComposeState::default();
    ready(&mut state, "hi");
    state.chat.press(Submit::FromNormalMode, press(KeyCode::Char('a')));
    let first = state.press_chat(press(KeyCode::Esc)).unwrap();
    assert!(!first.dismiss, "the first Esc leaves insert mode");
    assert!(state.press_chat(press(KeyCode::Esc)).unwrap().dismiss);

    state.press_chat(press(KeyCode::Esc));
    state.press_chat(press(KeyCode::Char('l')));
    assert!(!state.press_chat(press(KeyCode::Esc)).unwrap().dismiss, "Esc l Esc is no gesture");
}

/// A surface opens in insert mode when empty and in normal mode when it
/// holds text.
#[test]
fn focus_opens_an_empty_surface_in_insert_mode() {
    let mut empty = ComposeBuffer::default();
    empty.settle_for_focus();
    assert_eq!(empty.mode().as_deref(), Some("-- INSERT --"));
    let mut full = ComposeBuffer::over("held");
    full.press(Submit::FromNormalMode, press(KeyCode::Char('a')));
    full.settle_for_focus();
    assert!(full.in_normal_mode());
}

/// The prefilled prompt is in insert mode with the cursor after the text.
#[test]
fn a_prefilled_line_types_after_its_text() {
    let mut buffer = ComposeBuffer::prefilled("kj context rotate ");
    typed(&mut buffer, "banto");
    assert_eq!(buffer.text(), "kj context rotate banto");
}

/// Keys typed while the draft loads wait for it, and a read that is not the
/// newest is ignored.
#[test]
fn keys_wait_for_the_draft_to_load() {
    let mut state = ComposeState::default();
    let (a, b) = (ContextId::new(), ContextId::new());
    let principal = PrincipalId::new();
    let stale = state.begin_load(a, principal).read;
    let read = state.begin_load(b, principal).read;
    assert!(state.press_chat(press(KeyCode::Char('a'))).is_none(), "held");
    assert!(state.paste_chat("!").is_none(), "held");
    assert_eq!(state.finish_load(a, stale, Some("wrong")), None);
    assert_eq!(state.finish_load(b, stale, Some("wrong")), None);
    let held = state.finish_load(b, read, Some("kept")).expect("the newest read");
    assert_eq!(held, vec![HeldInput::Key(press(KeyCode::Char('a'))), HeldInput::Paste("!".into())]);
    assert_eq!(state.chat.text(), "kept");
    assert_eq!(state.ready_context(), Some(b));
}

/// The echo race: a mirror that predates our own acked edit is refused, and
/// one that arrives while an edit is in flight waits for it.
#[test]
fn a_mirror_never_rewinds_our_own_typing() {
    let mut state = ComposeState::default();
    ready(&mut state, "");
    state.chat.press(Submit::FromNormalMode, press(KeyCode::Char('i')));
    typed(&mut state.chat, "ab");
    let generation = state.sent();
    state.landed(generation, Some(9));
    assert!(!state.offer_mirror("a", 8));
    assert_eq!(state.chat.text(), "ab");

    let generation = state.sent();
    assert!(!state.offer_mirror("Xab", 11), "held while an edit is in flight");
    assert!(state.landed(generation, Some(10)), "the held mirror applies once it lands");
    assert_eq!(state.chat.text(), "Xab");
}

/// After a submit the feed may still show the submitted text as the draft;
/// nothing is taken from it until the submit is answered, and the block it
/// became is never read back.
#[test]
fn a_submitted_draft_never_comes_back() {
    let mut state = ComposeState::default();
    ready(&mut state, "sent");
    let me = PrincipalId::new();
    let old = block(Status::Draft, "sent", me);
    state.begin_submit();
    assert!(!state.offer_mirror("sent", 4), "no mirror while the submit is unanswered");
    assert_eq!(state.chat.text(), "");
    state.finish_submit(Some(old.id));
    assert_eq!(state.draft_in(std::slice::from_ref(&old), old.id.context_id, me), None, "the submitted block is skipped");
    let next = block(Status::Draft, "next", me);
    assert_eq!(state.draft_in(&[old.clone(), next], old.id.context_id, me), Some("next"));
}

/// A chat submit in a context a person performs runs as shell and answers
/// with the command block, not the draft. The kernel numbers the command
/// block after the draft it ran, so the draft's late echo is still skipped,
/// and the next draft, numbered after both, is read.
#[test]
fn a_draft_run_as_shell_never_comes_back() {
    let mut state = ComposeState::default();
    ready(&mut state, "echo hi");
    let (context, me) = (ContextId::new(), PrincipalId::new());
    let numbered = |seq: u64, content: &str| {
        kaijutsu_types::BlockSnapshotBuilder::new(BlockId::new(context, me, seq), kaijutsu_types::BlockKind::Text)
            .status(Status::Draft)
            .content(content)
            .build()
    };
    state.begin_submit();
    state.finish_submit(Some(BlockId::new(context, me, 5)));
    let old = numbered(1, "echo hi");
    assert_eq!(state.draft_in(std::slice::from_ref(&old), context, me), None, "the draft the shell ran is skipped");
    assert_eq!(state.draft_in(&[old, numbered(6, "next")], context, me), Some("next"));
}

/// Each context keeps its own submit: a submit in B while A's echo is still
/// on the way does not let A's submitted draft back onto the compose line.
#[test]
fn a_submit_elsewhere_does_not_forget_this_contexts_submit() {
    let mut state = ComposeState::default();
    let (a, b, me) = (ContextId::new(), ContextId::new(), PrincipalId::new());
    let draft = |context: ContextId, seq: u64, content: &str| {
        kaijutsu_types::BlockSnapshotBuilder::new(BlockId::new(context, me, seq), kaijutsu_types::BlockKind::Text)
            .status(Status::Draft)
            .content(content)
            .build()
    };
    let read = state.begin_load(a, me).read;
    state.finish_load(a, read, Some("echo hi"));
    state.begin_submit();
    state.finish_submit(Some(BlockId::new(a, me, 9)));
    let read = state.begin_load(b, me).read;
    state.finish_load(b, read, Some("ls"));
    state.begin_submit();
    state.finish_submit(Some(BlockId::new(b, me, 4)));
    let read = state.begin_load(a, me).read;
    state.finish_load(a, read, Some(""));

    let mirror = [draft(a, 1, "echo hi")];
    if let Some(text) = state.draft_in(&mirror, a, me).map(str::to_string) {
        state.offer_mirror(&text, 10);
    }
    assert_eq!(state.chat.text(), "", "A's submitted draft came back after a submit in B");
}

/// Type `keys` on the chat surface through the compose state, one char at a
/// time.
fn chat_keys(state: &mut ComposeState, keys: &str) {
    for c in keys.chars() {
        state.press_chat(press(KeyCode::Char(c))).expect("the draft is loaded");
    }
}

/// The registers are the app's, not one draft's: `dd` in one context's
/// draft, a switch, and `p` puts the line in the next, on its own line.
#[test]
fn a_deleted_line_survives_a_context_switch() {
    let mut state = ComposeState::default();
    ready(&mut state, "wrong spot\nkeep");
    chat_keys(&mut state, "ggdd");
    assert_eq!(state.chat.text(), "keep");
    ready(&mut state, "other draft");
    chat_keys(&mut state, "p");
    assert_eq!(state.chat.text(), "other draft\nwrong spot");
}

/// A submit and a clear start a new draft and keep the registers.
#[test]
fn a_yank_survives_a_submit_and_a_clear() {
    let mut state = ComposeState::default();
    ready(&mut state, "echo hi");
    chat_keys(&mut state, "0yw");
    state.begin_submit();
    state.finish_submit(None);
    chat_keys(&mut state, "p");
    assert_eq!(state.chat.text(), "echo ");
    state.clear_chat();
    chat_keys(&mut state, "p");
    assert_eq!(state.chat.text(), "echo ");
}

/// A `y` reports its text for the clipboard; a delete does not.
#[test]
fn a_yank_is_reported_and_a_delete_is_not() {
    let mut state = ComposeState::default();
    ready(&mut state, "one\ntwo");
    let last = |state: &mut ComposeState, keys: &str| {
        keys.chars().map(|c| state.press_chat(press(KeyCode::Char(c))).expect("the draft is loaded").yanked).last().flatten()
    };
    assert_eq!(last(&mut state, "ggyy").as_deref(), Some("one\n"));
    assert_eq!(last(&mut state, "j"), None);
    assert_eq!(last(&mut state, "dd"), None, "a delete stays out of the clipboard");
}

/// A yank made outside compose puts on either surface.
#[test]
fn an_outside_yank_puts_in_the_chat_draft() {
    let mut state = ComposeState::default();
    ready(&mut state, "draft");
    state.yank(&kaijutsu_editor::Yank::lines("+added"));
    chat_keys(&mut state, "p");
    assert_eq!(state.chat.text(), "draft\n+added");
}

/// `Ctrl+A ]` puts the unnamed register as `p` does, and returns the edits.
#[test]
fn put_places_the_register_in_the_chat_draft() {
    let mut state = ComposeState::default();
    ready(&mut state, "draft");
    state.yank(&kaijutsu_editor::Yank::lines("+added"));
    let action = state.put_chat().expect("the draft is loaded");
    assert_eq!(state.chat.text(), "draft\n+added");
    assert!(!action.ops.is_empty() && action.notice.is_none());
}

/// The shell line puts from the same registers.
#[test]
fn put_places_the_register_in_the_shell_line() {
    let mut state = ComposeState { shell: ComposeBuffer::over("ls"), ..Default::default() };
    state.yank(&kaijutsu_editor::Yank { text: "-la".into(), linewise: false });
    let action = state.put_shell();
    assert_eq!(state.shell.text(), "ls-la");
    assert!(action.notice.is_none());
}

/// Empty registers say so and edit nothing.
#[test]
fn put_with_nothing_yanked_is_a_notice() {
    let mut state = ComposeState::default();
    ready(&mut state, "draft");
    let action = state.put_chat().expect("the draft is loaded");
    assert!(action.ops.is_empty());
    assert_eq!(action.notice.as_deref(), Some("nothing to put"));
    assert_eq!(state.chat.text(), "draft");
}

/// While the `:` bar is open the put is refused, and the bar keeps its line.
#[test]
fn put_with_the_bar_open_is_refused() {
    let mut state = ComposeState::default();
    ready(&mut state, "draft");
    state.yank(&kaijutsu_editor::Yank::lines("+added"));
    chat_keys(&mut state, ":");
    let action = state.put_chat().expect("the draft is loaded");
    assert!(action.ops.is_empty());
    assert_eq!(action.notice.as_deref(), Some("close the ':' line before putting"));
    assert_eq!(state.chat.command_line().as_deref(), Some(":"));
    assert_eq!(state.chat.text(), "draft");
}

/// A put while the draft loads is held like a key and replayed in order.
#[test]
fn a_put_while_the_draft_loads_is_held() {
    let mut state = ComposeState::default();
    state.yank(&kaijutsu_editor::Yank::lines("+added"));
    let context = ContextId::new();
    let read = state.begin_load(context, PrincipalId::new()).read;
    assert!(state.press_chat(press(KeyCode::Char('a'))).is_none());
    assert!(state.put_chat().is_none(), "held");
    let held = state.finish_load(context, read, Some("kept")).expect("the newest read");
    assert_eq!(held, vec![HeldInput::Key(press(KeyCode::Char('a'))), HeldInput::Put]);
}

/// Both surfaces share the registers, as two buffers in one vim do.
#[test]
fn the_shell_line_and_the_chat_draft_share_registers() {
    let mut state = ComposeState { shell: ComposeBuffer::over("ls -la"), ..Default::default() };
    ready(&mut state, "");
    for c in "0yw".chars() {
        state.press_shell(press(KeyCode::Char(c)));
    }
    chat_keys(&mut state, "p");
    assert_eq!(state.chat.text(), "ls ");
}

/// Clearing keeps the draft and returns the one edit that empties the
/// kernel's copy.
#[test]
fn clearing_the_chat_is_one_delete() {
    let mut state = ComposeState::default();
    ready(&mut state, "doomed");
    assert_eq!(state.clear_chat(), Some(EditOp { offset: 0, insert: String::new(), delete: 6 }));
    assert_eq!(state.chat.text(), "");
    assert_eq!(state.clear_chat(), None, "nothing left to delete");
}

/// A second switch while the first draft is still loading: keys typed for
/// draft A are dropped and counted, never typed into draft B.
#[test]
fn a_second_load_drops_the_keys_held_for_the_first() {
    let mut state = ComposeState::default();
    let (a, b) = (ContextId::new(), ContextId::new());
    let principal = PrincipalId::new();
    state.begin_load(a, principal);
    state.press_chat(press(KeyCode::Char('i')));
    state.paste_chat("for a");
    let second = state.begin_load(b, principal);
    assert_eq!(second.dropped, 2);
    let held = state.finish_load(b, second.read, Some("b's draft")).expect("the newest read");
    assert!(held.is_empty(), "nothing typed for a reaches b: {held:?}");
    assert_eq!(state.chat.text(), "b's draft");
}

/// The double-Esc is one surface's gesture: an Esc on chat, a surface
/// change, and an Esc on the shell is no dismiss.
#[test]
fn a_surface_change_breaks_the_double_escape() {
    let mut state = ComposeState::default();
    ready(&mut state, "hi");
    assert!(!state.press_chat(press(KeyCode::Esc)).unwrap().dismiss);
    state.reset_escape();
    assert!(!state.press_shell(press(KeyCode::Esc)).dismiss, "the shell's first Esc");
}

/// The tui's routing (`kaijutsu-tui`'s `cmdline::parse`): a line whose
/// first word is `kj` runs through `execute_kj`, its argv split on
/// whitespace with no shell quoting; any other line runs in the shell.
#[test]
fn a_kj_line_runs_structured_and_anything_else_runs_in_the_shell() {
    let argv = |words: &[&str]| Some(words.iter().map(|w| w.to_string()).collect::<Vec<_>>());
    assert_eq!(shell_kj_argv("kj context rotate "), argv(&["context", "rotate"]));
    assert_eq!(shell_kj_argv("  kj context rename \"two words\""), argv(&["context", "rename", "\"two", "words\""]));
    assert_eq!(shell_kj_argv("kj context list | head"), argv(&["context", "list", "|", "head"]));
    assert_eq!(shell_kj_argv("kj"), argv(&[]));
    assert_eq!(shell_kj_argv("kjx"), None);
    assert_eq!(shell_kj_argv("echo kj"), None);
}

#[test]
fn byte_offsets_count_multibyte_chars() {
    assert_eq!(byte_offset("café!", 4), 5);
    assert_eq!(byte_offset("café", 9), 5, "past the end is the end");
}
