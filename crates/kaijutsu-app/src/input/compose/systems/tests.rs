//! Compose driven the way the app drives it: grabbed keys in, the draft
//! outbox and the overlays out.

use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;

use bevy::input::ButtonState;
use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::tasks::{IoTaskPool, TaskPool};
use kaijutsu_client::draft_outbox::{self, DraftEdit, DraftSink};
use kaijutsu_types::{BlockId, InputEdge, PrincipalId};

use super::*;

/// A kernel stand-in: applies each edit to its own copy of the draft, char
/// indexed as the kernel is, and logs every call in the order it arrived.
#[derive(Clone, Default)]
struct Kernel {
    log: Rc<RefCell<Vec<String>>>,
    draft: Rc<RefCell<String>>,
}

impl DraftSink for Kernel {
    fn edit(&self, _: ContextId, edit: &DraftEdit) -> impl Future<Output = Result<u64, String>> {
        let mut chars: Vec<char> = self.draft.borrow().chars().collect();
        let at = edit.offset as usize;
        chars.splice(at..at + edit.delete as usize, edit.insert.chars());
        *self.draft.borrow_mut() = chars.into_iter().collect();
        self.log.borrow_mut().push(format!("edit {:?}", self.draft.borrow()));
        let version = self.log.borrow().len() as u64;
        async move { Ok(version) }
    }

    fn submit(&self, context_id: ContextId, _: Option<InputEdge>) -> impl Future<Output = Result<BlockId, String>> {
        let text = std::mem::take(&mut *self.draft.borrow_mut());
        self.log.borrow_mut().push(format!("submit {text:?}"));
        async move { Ok(BlockId::new(context_id, PrincipalId::system(), 7)) }
    }

    fn read(&self, _: ContextId) -> impl Future<Output = Result<String, String>> {
        self.log.borrow_mut().push("read".into());
        let text = self.draft.borrow().clone();
        async move { Ok(text) }
    }
}

struct Harness {
    app: App,
    kernel: Kernel,
    writer: Option<std::pin::Pin<Box<dyn Future<Output = ()>>>>,
    context: ContextId,
    chat: Entity,
}

impl Harness {
    /// A focused chat surface over `draft`, the kernel's copy, not yet read.
    fn new(draft: &str) -> Self {
        IoTaskPool::get_or_init(TaskPool::new);
        let kernel = Kernel::default();
        *kernel.draft.borrow_mut() = draft.to_string();
        let (outbox, writer, landed) = draft_outbox::channel(kernel.clone());
        let context = ContextId::new();
        let mut app = App::new();
        app.add_message::<GrabbedKey>()
            .add_message::<LiteralPrefix>()
            .add_message::<ActionFired>()
            .add_message::<RpcResultMessage>()
            .init_resource::<ButtonInput<KeyCode>>()
            .insert_resource(KeyboardGrab::ComposeVim)
            .init_resource::<ComposeState>()
            .insert_resource(DraftOutboxState::for_test(outbox, landed))
            .init_resource::<ActiveSurface>()
            .insert_resource(FocusArea::Compose)
            .init_resource::<DocumentCache>()
            .insert_resource(SessionPrincipal(Some(PrincipalId::new())))
            .insert_resource(RpcResultChannel::new())
            .init_resource::<ConversationScrollState>()
            .init_resource::<GlobalErrorQueue>()
            .init_resource::<Time>()
            .init_resource::<crate::input::interrupt::InterruptState>()
            .add_systems(
                Update,
                (
                    compose_dispatch_keys,
                    handle_focus_compose,
                    handle_pop_level,
                    handle_prompt_prefill,
                    handle_compose_paste,
                    crate::input::systems::handle_interrupt,
                    follow_draft_context,
                    drain_draft_acks,
                    reconcile_chat_draft,
                    project_compose_overlays,
                )
                    .chain(),
            );
        app.world_mut().resource_mut::<DocumentCache>().set_active(context);
        let chat = app.world_mut().spawn((InputOverlayMarker, InputOverlay::default())).id();
        Self { app, kernel, writer: Some(Box::pin(writer)), context, chat }
    }

    /// Run the writer until everything queued so far has reached the
    /// kernel, by dropping the outbox it reads from. Its acks still land.
    fn drain_the_writer(&mut self) {
        self.app.world_mut().resource_mut::<DraftOutboxState>().retire();
        futures::executor::block_on(self.writer.take().expect("the writer runs once"));
    }

    /// Answer the draft read the first frame started, as the outbox would.
    fn load(&mut self) {
        self.app.update();
        let DraftTarget::Loading { read, .. } = self.app.world().resource::<ComposeState>().draft() else {
            panic!("the first frame starts the read");
        };
        let text = self.kernel.draft.borrow().clone();
        self.app.world_mut().write_message(RpcResultMessage::DraftRead { context_id: self.context, read, result: Ok(text) });
        self.app.update();
    }

    /// Press `keys` in one frame: each char is a key; `<Esc>` and `<CR>` are
    /// the named keys.
    fn keys(&mut self, keys: &str) {
        for event in key_events(keys) {
            self.app.world_mut().write_message(GrabbedKey(event));
        }
        self.app.update();
    }

    fn overlay(&self) -> &InputOverlay {
        self.app.world().get::<InputOverlay>(self.chat).unwrap()
    }

    fn log(&self) -> Vec<String> {
        self.kernel.log.borrow().clone()
    }
}

fn key_events(keys: &str) -> Vec<KeyboardInput> {
    let event = |key_code: KeyCode, logical_key: Key| KeyboardInput {
        key_code,
        logical_key,
        state: ButtonState::Pressed,
        text: None,
        repeat: false,
        window: Entity::PLACEHOLDER,
    };
    let mut out = Vec::new();
    let mut rest = keys;
    while let Some(c) = rest.chars().next() {
        if let Some(named) = rest.strip_prefix("<Esc>") {
            out.push(event(KeyCode::Escape, Key::Escape));
            rest = named;
        } else if let Some(named) = rest.strip_prefix("<CR>") {
            out.push(event(KeyCode::Enter, Key::Enter));
            rest = named;
        } else {
            out.push(event(KeyCode::Unidentified(bevy::input::keyboard::NativeKeyCode::Unidentified), Key::Character(c.to_string().into())));
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

/// The ordering hazard: rapid typing, Enter, and more typing in the same
/// frame. The kernel sees every keystroke before the submit, the submit
/// takes exactly the typed text, and the keys after it start the next
/// draft.
#[test]
fn typing_then_enter_submits_exactly_the_typed_text() {
    let mut h = Harness::new("");
    h.load();
    h.keys("ihéllo<Esc><CR>inext");
    h.drain_the_writer();
    assert_eq!(
        h.log(),
        [
            "read",
            "edit \"h\"",
            "edit \"hé\"",
            "edit \"hél\"",
            "edit \"héll\"",
            "edit \"héllo\"",
            "submit \"héllo\"",
            "edit \"n\"",
            "edit \"ne\"",
            "edit \"nex\"",
            "edit \"next\"",
        ]
    );
    assert_eq!(h.overlay().text, "next");
}

/// Keys pressed while the draft is being read wait for it, then land on it.
#[test]
fn keys_typed_while_the_draft_loads_land_after_the_read() {
    let mut h = Harness::new("half");
    h.app.update();
    h.keys("A done");
    assert_eq!(h.overlay().text, "", "nothing drawn before the draft is read");
    let DraftTarget::Loading { read, .. } = h.app.world().resource::<ComposeState>().draft() else {
        panic!("still loading");
    };
    h.app.world_mut().write_message(RpcResultMessage::DraftRead { context_id: h.context, read, result: Ok("half".into()) });
    h.app.update();
    assert_eq!(h.overlay().text, "half done");
    h.drain_the_writer();
    assert_eq!(h.log().first().map(String::as_str), Some("read"));
    assert_eq!(h.log().last().map(String::as_str), Some("edit \"half done\""));
}

/// Undo through the app path: `u` takes the typing back on the overlay and
/// in the kernel, and `Ctrl-r` is the redo.
#[test]
fn undo_reaches_the_overlay_and_the_kernel() {
    let mut h = Harness::new("");
    h.load();
    h.keys("iword<Esc>u");
    assert_eq!(h.overlay().text, "");
    h.drain_the_writer();
    assert_eq!(h.kernel.draft.borrow().as_str(), "");
}

/// Visual `d` and `/` search through the app path. The overlay shows the
/// bar's line while it is open and the cursor where the search left it.
#[test]
fn a_visual_delete_and_a_search_reach_the_overlay() {
    let mut h = Harness::new("one two three");
    h.load();
    h.keys("0wved");
    assert_eq!(h.overlay().text, "one  three");
    h.keys("/thr");
    assert_eq!(h.overlay().text, "/thr", "the bar's line is drawn while it is open");
    h.keys("<CR>");
    assert_eq!(h.overlay().text, "one  three");
    assert_eq!(h.overlay().cursor, 5, "the cursor is on the match");
    h.drain_the_writer();
    assert_eq!(h.kernel.draft.borrow().as_str(), "one  three");
}

/// A small in-place fix, "rost" to "rest" with `x`, `i e`, `Esc`, changes
/// what the overlay draws on each frame.
#[test]
fn a_small_in_place_edit_redraws_the_overlay() {
    let mut h = Harness::new("rost");
    h.load();
    h.keys("0lx");
    assert_eq!(h.overlay().text, "rst");
    h.keys("ie<Esc>");
    assert_eq!(h.overlay().text, "rest");
    assert_eq!(h.overlay().vim_mode, None, "back in normal mode");
}

/// A second Esc in normal mode is vi's no-op: compose keeps the keyboard,
/// fires nothing, and `dd` after it still clears the draft.
#[test]
fn a_double_escape_stays_in_compose() {
    let mut h = Harness::new("");
    h.load();
    h.keys("ihi<Esc><Esc>");
    assert_eq!(h.pops(), 0, "Esc Esc fired PopLevel");
    assert_eq!(h.focus(), FocusArea::Compose);
    h.keys("dd");
    assert_eq!(h.overlay().text, "");
}

/// Keys outside the compose grab are not compose's, now or later.
#[test]
fn keys_under_another_grab_are_never_typed() {
    let mut h = Harness::new("");
    h.load();
    h.app.insert_resource(KeyboardGrab::EditorSession);
    h.keys("ihi");
    h.app.insert_resource(KeyboardGrab::ComposeVim);
    h.app.update();
    assert_eq!(h.overlay().text, "");
}

/// A failed chat submit says so and reads the kernel's draft back, which
/// still holds what was typed.
#[test]
fn a_failed_submit_reloads_the_kernels_draft() {
    let mut h = Harness::new("kept");
    h.load();
    h.app.world_mut().resource_mut::<ComposeState>().begin_submit();
    h.app.world_mut().write_message(RpcResultMessage::DraftSubmitted {
        context_id: h.context,
        result: Err("submit input: no performer".into()),
    });
    h.app.update();
    assert!(matches!(h.app.world().resource::<ComposeState>().draft(), DraftTarget::Loading { .. }));
    let errors = h.app.world().resource::<GlobalErrorQueue>();
    assert!(errors.entries.iter().any(|e| e.message.contains("no performer")));
    assert!(h.app.world().get::<ComposeError>(h.chat).is_some(), "the overlay flashes");
}

impl Harness {
    /// The read token of the draft load in progress.
    fn loading_read(&self) -> u64 {
        let DraftTarget::Loading { read, .. } = self.app.world().resource::<ComposeState>().draft() else {
            panic!("a draft load is in progress");
        };
        read
    }

    fn summon_chat(&mut self) {
        self.app.world_mut().write_message(ActionFired::new(Action::SummonChat, InputContext::Navigation));
    }

    fn submit_refused(&mut self) {
        self.app.world_mut().write_message(RpcResultMessage::DraftSubmitted {
            context_id: self.context,
            result: Err("submit input: No performer assigned".into()),
        });
    }

    fn draft_read(&mut self, text: &str) {
        let read = self.loading_read();
        self.app.world_mut().write_message(RpcResultMessage::DraftRead { context_id: self.context, read, result: Ok(text.into()) });
    }
}

/// A refused submit, then typing into the re-summoned box while the kernel's
/// draft is read back. The player sees an empty box in insert mode and types
/// text; the keys must land as text after the read-back draft, never run as
/// normal-mode commands against it (`d<Space>` deleting, `p` pasting).
#[test]
fn typing_while_a_refused_draft_reloads_lands_as_text() {
    for summon_first in [false, true] {
        let mut h = Harness::new("");
        h.load();
        h.keys("ir<Esc><CR>");
        if summon_first {
            h.summon_chat();
            h.app.update();
            h.submit_refused();
            h.app.update();
        } else {
            h.submit_refused();
            h.app.update();
            h.summon_chat();
            h.app.update();
        }
        h.keys("d typing");
        h.draft_read("r");
        h.app.update();
        assert_eq!(h.overlay().text, "rd typing", "summoned before the refusal landed: {summon_first}");
        assert_eq!(h.overlay().vim_mode.as_deref(), Some("-- INSERT --"));
    }
}

/// A failed edit is never silent.
#[test]
fn a_failed_edit_is_shown() {
    let mut h = Harness::new("");
    h.load();
    h.keys("ix");
    h.app.world_mut().resource_mut::<DraftOutboxState>().retire();
    drop(h.writer.take());
    h.app.update();
    let errors = h.app.world().resource::<GlobalErrorQueue>();
    assert!(
        errors.entries.iter().any(|e| e.message.contains("draft edit failed") && e.message.contains("writer stopped")),
        "{:?}",
        errors.entries.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
    assert_eq!(h.app.world().resource::<ComposeState>().edits_in_flight(), 0);
}

#[test]
fn pending_refusal_is_visible_without_restoring_a_retryable_submission() {
    let pending = kaijutsu_client::CallError::Refused(kaijutsu_types::Refusal {
        kind: kaijutsu_types::RefusalKind::Pending,
        reason: "review pending".into(),
        subject: "shell".into(),
        ask: None,
        remedy: None,
    });
    let fault = kaijutsu_client::CallError::Rpc("connection reset".into());

    assert!(!submission_needs_restore(&pending));
    assert!(submission_needs_restore(&fault));
}

impl Harness {
    /// Press `Ctrl` + `code` in one frame, as the grab sees it.
    fn ctrl(&mut self, code: KeyCode, c: char) {
        self.app.world_mut().resource_mut::<ButtonInput<KeyCode>>().press(KeyCode::ControlLeft);
        self.app.world_mut().write_message(GrabbedKey(KeyboardInput {
            key_code: code,
            logical_key: Key::Character(c.to_string().into()),
            state: ButtonState::Pressed,
            text: None,
            repeat: false,
            window: Entity::PLACEHOLDER,
        }));
        self.app.update();
        self.app.world_mut().resource_mut::<ButtonInput<KeyCode>>().release(KeyCode::ControlLeft);
    }

    fn fire(&mut self, action: Action) {
        self.app.world_mut().write_message(ActionFired::new(action, InputContext::Global));
        self.app.update();
    }

    /// Put the shell surface on screen with compose focused and the shell
    /// line settled for focus, the state a summoned shell is left in.
    fn open_shell(&mut self) {
        *self.app.world_mut().resource_mut::<ActiveSurface>() = ActiveSurface::Shell;
        *self.app.world_mut().resource_mut::<FocusArea>() = FocusArea::Compose;
        self.app.world_mut().resource_mut::<ComposeState>().shell.settle_for_focus();
        self.app.update();
    }

    fn compose(&self) -> &ComposeState {
        self.app.world().resource::<ComposeState>()
    }

    fn surface(&self) -> ActiveSurface {
        self.app.world().resource::<ActiveSurface>().clone()
    }

    fn focus(&self) -> FocusArea {
        self.app.world().resource::<FocusArea>().clone()
    }

    fn errors(&self) -> Vec<String> {
        self.app.world().resource::<GlobalErrorQueue>().entries.iter().map(|e| e.message.clone()).collect()
    }

    fn pops(&self) -> usize {
        let actions = self.app.world().resource::<Messages<ActionFired>>();
        actions.iter_current_update_messages().filter(|a| a.action == Action::PopLevel).count()
    }

    /// The actions fired during the last update, in order.
    fn fired(&self) -> Vec<Action> {
        let actions = self.app.world().resource::<Messages<ActionFired>>();
        actions.iter_current_update_messages().map(|a| a.action.clone()).collect()
    }
}

/// Chat keys held while the draft loads replay into the chat draft even
/// when Ctrl+Z moved to the shell meanwhile: they never type into, or run,
/// the shell line.
#[test]
fn held_chat_keys_replay_into_chat_after_a_surface_change() {
    let mut h = Harness::new("draft");
    h.app.update();
    h.keys("A more<Esc><CR>");
    h.open_shell();
    assert_eq!(h.surface(), ActiveSurface::Shell);
    h.draft_read("draft");
    h.app.update();
    assert_eq!(h.compose().shell.text(), "", "nothing reached the shell line");
    assert!(h.errors().iter().all(|e| !e.contains("nothing was run")), "{:?}", h.errors());
    h.drain_the_writer();
    assert!(h.log().contains(&"submit \"draft more\"".to_string()), "{:?}", h.log());
}

/// Two switches before the first draft loads: keys typed for the first are
/// dropped with a notice that counts them, and never reach the second.
#[test]
fn a_double_switch_drops_the_first_drafts_keys_loudly() {
    let mut h = Harness::new("b's draft");
    h.app.update();
    h.keys("ix");
    let second = ContextId::new();
    h.app.world_mut().resource_mut::<DocumentCache>().set_active(second);
    h.app.update();
    assert!(h.errors().iter().any(|e| e.contains("2 keys typed for the last draft were dropped")), "{:?}", h.errors());
    let read = h.loading_read();
    h.app.world_mut().write_message(RpcResultMessage::DraftRead { context_id: second, read, result: Ok("b's draft".into()) });
    h.app.update();
    assert_eq!(h.overlay().text, "b's draft");
}

/// An edit the dead writer never sent lands as a failure even when the next
/// keystroke finds the writer gone first: it is counted and shown, so the
/// ledger does not hold mirrors for it forever.
#[test]
fn an_edit_lost_before_a_failed_send_is_counted_and_shown() {
    let mut h = Harness::new("");
    h.load();
    h.keys("ia");
    drop(h.writer.take());
    h.keys("b");
    assert_eq!(h.compose().edits_in_flight(), 0, "both edits counted as landed");
    assert!(h.errors().iter().any(|e| e.contains("writer stopped")), "{:?}", h.errors());
}

/// Ctrl+Z in compose asks for the root/shell context toggle. The chat draft
/// stays on screen: the surface is not what Ctrl+Z moves now.
#[test]
fn ctrl_z_in_compose_asks_for_the_context_toggle() {
    let mut h = Harness::new("hi");
    h.load();
    h.ctrl(KeyCode::KeyZ, 'z');
    assert_eq!(h.surface(), ActiveSurface::Chat);
    assert_eq!(h.fired(), vec![Action::ToggleShellContext]);
}

/// PopLevel from the shell: back to the conversation, the shell in normal
/// mode, and chat the surface for next time.
#[test]
fn pop_level_leaves_compose_in_normal_mode() {
    let mut h = Harness::new("");
    h.load();
    h.open_shell();
    h.keys("ls");
    h.fire(Action::PopLevel);
    assert_eq!((h.focus(), h.surface()), (FocusArea::Conversation, ActiveSurface::Chat));
    assert!(h.compose().shell.in_normal_mode());
    assert_eq!(h.compose().shell.text(), "ls", "the line is kept");
}

#[test]
fn prompt_prefill_opens_the_shell_line_typed_up_to_its_argument() {
    let mut h = Harness::new("");
    h.load();
    h.fire(Action::PromptContextRotate);
    assert_eq!((h.surface(), h.focus()), (ActiveSurface::Shell, FocusArea::Compose));
    h.keys("banto");
    assert_eq!(h.compose().shell.text(), "kj context rotate banto");
}

/// A paste with no clipboard says so.
#[test]
fn a_paste_with_no_clipboard_is_shown() {
    let mut h = Harness::new("");
    h.load();
    h.fire(Action::Paste);
    assert!(h.errors().iter().any(|e| e.contains("no clipboard")), "{:?}", h.errors());
}

/// The third Ctrl+C clears the chat draft with one delete queued behind the
/// typing.
#[test]
fn the_third_interrupt_clears_the_chat_draft() {
    let mut h = Harness::new("");
    h.load();
    h.keys("idoomed");
    for _ in 0..3 {
        h.app.world_mut().write_message(ActionFired::new(Action::InterruptContext { immediate: false }, InputContext::TextInput));
    }
    h.app.update();
    assert_eq!(h.overlay().text, "");
    h.drain_the_writer();
    assert_eq!(h.kernel.draft.borrow().as_str(), "");
    assert_eq!(h.log().last().map(String::as_str), Some("edit \"\""));
}

/// A shell line with no connection stays put and says so; with one, it is
/// sent, cleared, and compose gives way to the conversation.
#[test]
fn a_shell_line_runs_through_the_connection() {
    let mut h = Harness::new("");
    h.load();
    h.fire(Action::PromptContextRotate);
    h.keys("<CR>");
    assert!(h.errors().iter().any(|e| e.contains("not connected")), "{:?}", h.errors());
    assert_eq!(h.compose().shell.text(), "kj context rotate ");

    let (handle, mut commands) = kaijutsu_client::ActorHandle::never_answers_for_test();
    h.app.insert_resource(crate::connection::RpcActor { handle, generation: 1 });
    h.keys("<CR>");
    assert_eq!(h.compose().shell.text(), "");
    assert_eq!(h.focus(), FocusArea::Conversation);
    let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
    let sent = rt.block_on(async { tokio::time::timeout(std::time::Duration::from_secs(2), commands.recv()).await });
    assert!(matches!(sent, Ok(Some(_))), "the line reached the connection");
}

/// A replaced connection's draft writer finishes before the new one sends
/// anything: an edit made before the replacement never lands after one made
/// after it.
#[test]
fn a_new_draft_writer_waits_for_the_old_one() {
    IoTaskPool::get_or_init(TaskPool::new);
    let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
    let wait = |rx: &mut kaijutsu_client::UnansweredCommands, ms| {
        rt.block_on(async { tokio::time::timeout(std::time::Duration::from_millis(ms), rx.recv()).await }).ok().flatten()
    };
    let mut app = App::new();
    app.init_resource::<DraftOutboxState>().add_systems(Update, super::super::outbox::sync_draft_outbox);
    let ctx = ContextId::new();
    let edit = || DraftEdit { offset: 0, insert: "x".into(), delete: 0 };

    let (first, mut first_rx) = kaijutsu_client::ActorHandle::never_answers_for_test();
    app.insert_resource(crate::connection::RpcActor { handle: first, generation: 1 });
    app.update();
    app.world().resource::<DraftOutboxState>().outbox().unwrap().edit(ctx, 1, edit()).unwrap();
    let in_flight = wait(&mut first_rx, 2000).expect("the first writer sent its edit");

    let (second, mut second_rx) = kaijutsu_client::ActorHandle::never_answers_for_test();
    app.insert_resource(crate::connection::RpcActor { handle: second, generation: 2 });
    app.update();
    app.world().resource::<DraftOutboxState>().outbox().unwrap().edit(ctx, 1, edit()).unwrap();
    assert!(wait(&mut second_rx, 300).is_none(), "the new writer waits while the old edit is in flight");

    drop(in_flight);
    assert!(wait(&mut second_rx, 2000).is_some(), "the new writer starts once the old one ends");
}
