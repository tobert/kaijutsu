//! In-app vi editor — the renderer + key-forwarder side (Design A).
//!
//! The kernel owns the editor session; the app is one *renderer* of it. The
//! `open_editor` peer signal lands here as [`EditorOpenRequested`], drives
//! `Screen::Editor`, and stores the live session in [`ActiveEditor`]. This
//! module is also home to the dedicated editor surface renderer (`render`,
//! fed by the `editor_state` subscription) and key forwarding to
//! `editor_keys` (`keys`, `editor_dispatch_keys`) — the screen/landing
//! foundation plus its renderer and forwarder.

use bevy::prelude::*;
use bevy::tasks::{IoTaskPool, Task};
use kaijutsu_client::editor_outbox::{self, EditorOutbox, Landed};
use kaijutsu_client::{EditorState, ServerEvent};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::connection::{RpcActor, ServerEventMessage};
use crate::input::SystemClipboard;
use crate::input::focus::{ActiveSurface, FocusArea};
use crate::ui::screen::Screen;

pub(crate) mod keys;
pub mod render;
mod viewport;

/// The editor outbox of the current connection: keys and pastes reach the
/// kernel in the order they were pressed, and the key path never waits on one
/// (`kaijutsu_client::editor_outbox`). One outbox per [`RpcActor`]; it is
/// replaced when the actor is. Answers come back as [`Landed`].
///
/// A replaced outbox is cancelled, not left to finish: its writer would
/// otherwise keep sending queued keys on the old handle after the new outbox
/// started, reordering across the seam. Cancelling drops the writer, whose
/// drop guard lands every queued or in-flight call as a failure. Those
/// answers arrive on the old receiver, which is kept in `retiring` until it
/// closes, and they are counted into a loud "lost across reconnect" notice.
#[derive(Resource, Default)]
pub struct EditorOutboxState {
    live: Option<LiveOutbox>,
    /// Receivers of replaced outboxes, drained until their writer is gone.
    retiring: Vec<UnboundedReceiver<Landed>>,
}

struct LiveOutbox {
    /// The [`RpcActor::generation`] the outbox writes to.
    generation: u64,
    outbox: EditorOutbox,
    landed: UnboundedReceiver<Landed>,
    /// The writer. Dropping it cancels the writer.
    writer: Task<()>,
}

impl EditorOutboxState {
    /// Queue `keys` for `session`, or say why they cannot be sent.
    fn keys(&self, session: u64, keys: &str) -> Result<(), String> {
        let live = self.live.as_ref().ok_or("no connection")?;
        live.outbox.keys(session, keys).map_err(|e| e.to_string())
    }

    /// Queue a paste of `text` for `session`, or say why it cannot be sent.
    fn insert(&self, session: u64, text: &str) -> Result<(), String> {
        let live = self.live.as_ref().ok_or("no connection")?;
        live.outbox.insert(session, text).map_err(|e| e.to_string())
    }

    /// Is an outbox for `generation` already running?
    fn serves(&self, generation: u64) -> bool {
        self.live.as_ref().is_some_and(|l| l.generation == generation)
    }

    /// Cancel the live outbox, keeping its receiver to collect what it lost.
    fn retire(&mut self) {
        if let Some(old) = self.live.take() {
            let LiveOutbox { landed, writer, .. } = old;
            drop(writer);
            self.retiring.push(landed);
        }
    }

    /// Make `outbox` the live one for `generation`, retiring any other.
    fn install(
        &mut self,
        generation: u64,
        outbox: EditorOutbox,
        writer: Task<()>,
        landed: UnboundedReceiver<Landed>,
    ) {
        self.retire();
        self.live = Some(LiveOutbox { generation, outbox, landed, writer });
    }
}

/// A kernel `open_editor` signal landed: the submitter ran `vi <path>` and the
/// kernel opened session `session`. Written by the peer dispatcher
/// (`peers::systems`), consumed by [`handle_editor_open`]. Carries the **initial
/// `EditorState`** (the signal is self-contained), so the renderer has text to
/// draw the instant it lands — subsequent `editor_state` pushes carry updates.
#[derive(Message, Clone, Debug)]
pub struct EditorOpenRequested {
    /// The path being edited (display / title).
    pub path: String,
    /// The initial editor state from the signal (carries the session id + text).
    pub state: EditorState,
}

/// The editor session the app is currently rendering, if any. Set when an
/// `open_editor` signal lands; cleared on exit/close. The MSDF renderer reads
/// `state` (text/cursor/mode) each frame.
#[derive(Resource, Default, Debug)]
pub struct ActiveEditor {
    /// The live session, or `None` when no editor is open.
    pub session: Option<EditorSessionView>,
    /// A local notice the strip shows when the kernel has nothing to say: set
    /// when keys or a paste could not reach the kernel, cleared by the next
    /// answer that landed, a new session, or a close.
    pub notice: Option<String>,
}

/// The session the renderer draws: identity + the latest pushed state.
#[derive(Clone, Debug)]
pub struct EditorSessionView {
    /// Kernel editor session handle (the currency of `editor_keys`/`editor_state`).
    pub session: u64,
    /// The path being edited, shown on the mode line.
    pub path: String,
    /// The [`RpcActor::generation`] the session is addressed through. An
    /// answer only acts on a session it carries the same generation for, so a
    /// late failure for a reused session id cannot pop a newer editor.
    pub generation: u64,
    /// The latest renderer-facing snapshot (seeded from the open signal, kept
    /// fresh by `editor_state` pushes — the human's keystrokes and peer merges).
    pub state: EditorState,
}

/// Land an `open_editor` signal: store the session + its initial state and reveal
/// `Screen::Editor`. The mirror of `handle_context_switch`'s screen reveal — the
/// editor drives its own screen from its own landing handler, not via `view/sync`.
fn handle_editor_open(
    mut events: MessageReader<EditorOpenRequested>,
    mut active: ResMut<ActiveEditor>,
    actor: Option<Res<RpcActor>>,
    mut next_screen: ResMut<NextState<Screen>>,
) {
    for ev in events.read() {
        info!(
            "open_editor: session {} on {}",
            ev.state.session, ev.path
        );
        // A new session replaces the old one; keys already queued carry their
        // session, so they cannot leak into the new one.
        active.notice = None;
        active.session = Some(EditorSessionView {
            session: ev.state.session,
            path: ev.path.clone(),
            generation: actor.as_ref().map_or(0, |a| a.generation),
            state: ev.state.clone(),
        });
        next_screen.set(Screen::Editor);
    }
}

/// Keep the active session's state fresh from the editor push channel: an
/// `EditorStateChanged` for our session replaces the cached snapshot (the
/// human's own keystrokes echoing, or a peer's merge); an `EditorClosed` drops
/// the session and pops back to the conversation.
fn handle_editor_events(
    mut server_events: MessageReader<ServerEventMessage>,
    mut active: ResMut<ActiveEditor>,
    mut next_screen: ResMut<NextState<Screen>>,
) {
    for ServerEventMessage(event) in server_events.read() {
        match event {
            ServerEvent::EditorStateChanged { state } => {
                if let Some(view) = active.session.as_mut()
                    && view.session == state.session
                {
                    view.state = state.clone();
                }
            }
            ServerEvent::EditorClosed { session_id }
                if active
                    .session
                    .as_ref()
                    .is_some_and(|v| v.session == *session_id) =>
            {
                info!("editor session {session_id} closed by server");
                active.session = None;
                        next_screen.set(Screen::Conversation);
            }
            _ => {}
        }
    }
}

/// Does an `editor_keys` failure mean the kernel session is gone (as opposed to
/// a transient timeout or a dropped connection)? The kernel returns
/// `editor: no such session N` (see `kaijutsu-kernel/src/editor.rs`) when the
/// session id isn't in `EditorSessions` — which is exactly what a kernel restart
/// produces: the sessions are in-memory and don't survive it, while the persisted
/// `kernel_id` is unchanged, so the reconnect looks ordinary. Match that verdict
/// (and not a generic error) so a momentary RPC hiccup never evicts a live editor.
fn is_session_lost_error(err: &str) -> bool {
    err.contains(kaijutsu_kernel::editor::NO_SUCH_SESSION)
}

/// What a failed [`Landed`] means for the editor.
#[derive(Debug, PartialEq, Eq)]
enum LandedVerdict {
    /// The kernel disowned the session (a restart): the buffer on screen is
    /// dead, so pop back to the conversation.
    SessionLost,
    /// The outbox writer stopped: nothing queued behind it will ever land, so
    /// the outbox is replaced.
    WriterStopped,
    /// This call failed; later calls are unaffected.
    Failed,
}

/// Classify a failed call's error text.
fn landed_verdict(err: &str) -> LandedVerdict {
    if is_session_lost_error(err) {
        LandedVerdict::SessionLost
    } else if err.contains(editor_outbox::WRITER_STOPPED) {
        LandedVerdict::WriterStopped
    } else {
        LandedVerdict::Failed
    }
}

/// Keep one [`EditorOutbox`] per connection: build it when the first actor
/// arrives and again whenever the actor is replaced, retiring the old one (see
/// [`EditorOutboxState`]). The writer is runtime-free and runs on Bevy's
/// `IoTaskPool`. A live session is rebound to the new generation: its calls
/// now travel through the new outbox.
fn editor_sync_outbox(
    actor: Option<Res<RpcActor>>,
    mut state: ResMut<EditorOutboxState>,
    mut active: ResMut<ActiveEditor>,
) {
    let Some(actor) = actor else {
        return;
    };
    if state.serves(actor.generation) {
        return;
    }
    let (outbox, writer, landed) = editor_outbox::channel(actor.handle.clone());
    let writer = IoTaskPool::get().spawn(writer);
    state.install(actor.generation, outbox, writer, landed);
    if let Some(view) = active.session.as_mut() {
        view.generation = actor.generation;
    }
}

/// Act on the answers to queued calls. A success applies the answered state
/// (the push channel is a broadcast and can lag, so the answer keeps the
/// screen from going stale) and clears the local notice; a `ZZ`/`ZQ` closes
/// via `EditorClosed`. A failure is never silent: the kernel disowning the
/// session pops the editor, a stopped writer is replaced, and any other
/// failure lands in the strip. The writer's death is only visible as a failed
/// answer, so this is where it surfaces. Calls an outbox lost when it was
/// replaced are counted into one notice.
///
/// `EditorState` carries no sequence number, so an answer and a push are
/// ordered only by arrival; the next push or answer corrects a regression.
fn editor_drain_landed(
    mut state: ResMut<EditorOutboxState>,
    mut active: ResMut<ActiveEditor>,
    mut next_screen: ResMut<NextState<Screen>>,
) {
    let mut writer_stopped = false;
    let mut answers: Vec<(u64, Landed)> = Vec::new();
    let mut lost_across_reconnect = 0usize;

    if let Some(live) = state.live.as_mut() {
        while let Ok(landed) = live.landed.try_recv() {
            answers.push((live.generation, landed));
        }
    }
    // Drain every retiring receiver until its writer is gone, so nothing the
    // writer's drop guard lands is left behind.
    state.retiring.retain_mut(|rx| loop {
        match rx.try_recv() {
            Ok(Landed { session, sent, result: Err(e) }) => {
                warn!("editor input {sent:?} for session {session} lost across reconnect: {e}");
                lost_across_reconnect += 1;
            }
            Ok(_) => {}
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break true,
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break false,
        }
    });
    if lost_across_reconnect > 0 {
        active.notice = Some(format!(
            "{lost_across_reconnect} editor call(s) lost across reconnect"
        ));
    }

    for (generation, Landed { session, sent, result }) in answers {
        let current = active
            .session
            .as_ref()
            .is_some_and(|v| v.session == session && v.generation == generation);
        let err = match result {
            Ok(editor_state) => {
                if current {
                    active.notice = None;
                    if let Some(view) = active.session.as_mut() {
                        view.state = editor_state;
                    }
                }
                continue;
            }
            Err(err) => err,
        };
        match landed_verdict(&err) {
            LandedVerdict::SessionLost => {
                if current {
                    warn!("editor session {session} lost (kernel restarted?); returning to conversation");
                    active.session = None;
                    active.notice = None;
                    next_screen.set(Screen::Conversation);
                } else {
                    warn!("ignoring a stale session-lost answer for session {session}");
                }
            }
            LandedVerdict::WriterStopped => {
                error!("editor outbox writer stopped; {sent:?} for session {session} was not sent: {err}");
                writer_stopped = true;
                if current {
                    active.notice = Some(format!("editor input lost: {err}"));
                }
            }
            LandedVerdict::Failed => {
                warn!("editor input {sent:?} for session {session} failed: {err}");
                if current {
                    active.notice = Some(format!("editor input lost: {err}"));
                }
            }
        }
    }
    if writer_stopped {
        // The next `editor_sync_outbox` builds a fresh one.
        state.retire();
    }
}

/// Where a paste reads from: Ctrl+Shift+V reads CLIPBOARD, middle-click reads
/// PRIMARY (xterm-style; `Ctrl+V` stays vi's visual-block key in the editor).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PasteSource {
    Clipboard,
    Primary,
}

/// A pasted newline as `\n`, whatever the clipboard held: the pair first, or
/// its `\r` would become a second newline.
fn normalize_paste(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// What to send for a paste of `raw`: `Ok(None)` for nothing, `Ok(Some(text))`
/// to insert, `Err(why)` to refuse. The kernel refuses a paste while the `:`
/// line is open too, but its reason rides `EditorState.message`, which the
/// strip does not draw while the `:` line is open, so the player would see
/// nothing happen. The notice says why.
fn paste_payload(raw: &str, command_line_open: bool) -> Result<Option<String>, &'static str> {
    if command_line_open {
        return Err("close the ':' line before pasting into the editor");
    }
    let text = normalize_paste(raw);
    Ok((!text.is_empty()).then_some(text))
}

/// Read `source` and queue the text as a paste into `session`. A missing
/// clipboard, an unreadable one, or a refused one is said, not swallowed.
fn paste_into(
    source: PasteSource,
    clipboard: Option<&mut SystemClipboard>,
    outbox: &EditorOutboxState,
    session: u64,
    command_line_open: bool,
    notice: &mut Option<String>,
) {
    let Some(clipboard) = clipboard else {
        warn!("editor paste: no system clipboard");
        *notice = Some("paste: no system clipboard".to_string());
        return;
    };
    let fetched = match source {
        PasteSource::Clipboard => clipboard.0.get_text(),
        PasteSource::Primary => crate::input::systems::primary_selection_text(&mut clipboard.0),
    };
    match fetched {
        Ok(raw) => match paste_payload(&raw, command_line_open) {
            Ok(None) => {}
            Ok(Some(text)) => {
                if let Err(e) = outbox.insert(session, &text) {
                    warn!("editor paste not sent: {e}");
                    *notice = Some(format!("paste lost: {e}"));
                }
            }
            Err(why) => *notice = Some(why.to_string()),
        },
        Err(e) => {
            warn!("editor paste: clipboard read failed: {e}");
            *notice = Some(format!("paste: {e}"));
        }
    }
}

/// Forward keystrokes on `Screen::Editor` toward the kernel's `editor_keys`.
/// The app is a pure key forwarder: each keystroke becomes vi notation and is
/// queued on the [`EditorOutboxState`] outbox, which keeps them in order, one
/// call in flight (burst input — key repeat, BRP `send_keys` — lands as one
/// coalesced batch). The kernel's `EditorCore` (the real VimMachine)
/// interprets the keys; the push channel echoes the resulting state back to
/// [`ActiveEditor`], so the panel re-renders as you type.
///
/// `Esc` is an ordinary key (→ normal mode). `ZZ`/`ZQ` also travel as ordinary
/// keys: the kernel — which alone knows the true mode, so it never mistakes an
/// *inserted* `ZZ` for a quit — saves/discards and drops the session, then
/// pushes `EditorClosed`, which [`handle_editor_events`] turns into the screen
/// pop. No app-side mode tracking, no quit detection.
fn editor_dispatch_keys(
    mut keyboard: MessageReader<crate::input::events::GrabbedKey>,
    mut literal: MessageReader<crate::input::events::LiteralPrefix>,
    keys: Res<ButtonInput<KeyCode>>,
    mut active: ResMut<ActiveEditor>,
    outbox: Res<EditorOutboxState>,
    mut clipboard: Option<ResMut<SystemClipboard>>,
    mut next_screen: ResMut<NextState<Screen>>,
    mut focus: ResMut<FocusArea>,
    mut surface: ResMut<ActiveSurface>,
) {
    let Some((session, command_line_open)) = active
        .session
        .as_ref()
        .map(|v| (v.session, v.state.command_line.is_some()))
    else {
        // Drop grabbed keys with no session to receive them.
        keyboard.read().for_each(|_| {});
        literal.read().for_each(|_| {});
        return;
    };
    let mut failure: Option<String> = None;

    // `Ctrl+A a` — the prefix machine delivered a literal Ctrl+A for the
    // kernel vi session (vim's increment). Modifier state is long gone, so
    // it travels as notation, not as a GrabbedKey.
    for _ in literal.read() {
        failure = outbox.keys(session, "<C-a>").err().or(failure);
    }

    for grabbed in keyboard.read() {
        // GrabbedKey carries only pressed events (dispatch filters releases).
        let event = &grabbed.0;
        // Bare modifier presses (Shift/Ctrl/… on their own) are never vi keys.
        if keys::is_modifier_key(event.key_code) {
            continue;
        }
        let mods = keys::Mods::from_input(&keys);

        // Ctrl+Z suspends the editor to the shell (the job-control metaphor):
        // leave to the conversation with the shell focused, but keep
        // `ActiveEditor` — the session stays alive in the kernel as the
        // "suspended job", and `fg` re-foregrounds it. This is a *local*
        // intercept (no kernel round-trip), so it also frees a frozen editor when
        // the kernel is unreachable — the escape hatch. (The Action-system
        // Ctrl+Z↔ToggleSurface can't double-handle: `KeyboardGrab::EditorSession`
        // leaves only Global bindings matchable, and Ctrl+Z isn't Global.) The
        // buffer isn't forwarded `<C-z>`. Keys already queued stay in the
        // outbox, so they still deliver to the live (suspended) session.
        if mods.ctrl && event.key_code == KeyCode::KeyZ {
            next_screen.set(Screen::Conversation);
            *focus = FocusArea::Compose;
            *surface = ActiveSurface::Shell;
            break;
        }

        if keys::paste_chord(event.key_code, mods) {
            // Holding the chord must not paste again and again.
            if !event.repeat {
                paste_into(
                    PasteSource::Clipboard,
                    clipboard.as_deref_mut(),
                    &outbox,
                    session,
                    command_line_open,
                    &mut failure,
                );
            }
            continue;
        }

        if let Some(notation) = keys::bevy_to_vi_notation(event, mods) {
            failure = outbox.keys(session, &notation).err().or(failure);
        }
    }

    if let Some(err) = failure {
        warn!("editor keys not sent: {err}");
        active.notice = Some(format!("editor input lost: {err}"));
    }
}

/// Middle-click pastes PRIMARY into the editor, as it does into compose.
fn editor_middle_click_paste(
    mouse: Res<ButtonInput<MouseButton>>,
    mut active: ResMut<ActiveEditor>,
    outbox: Res<EditorOutboxState>,
    mut clipboard: Option<ResMut<SystemClipboard>>,
) {
    if !mouse.just_pressed(MouseButton::Middle) {
        return;
    }
    let Some((session, command_line_open)) = active
        .session
        .as_ref()
        .map(|v| (v.session, v.state.command_line.is_some()))
    else {
        return;
    };
    let mut notice = None;
    paste_into(
        PasteSource::Primary,
        clipboard.as_deref_mut(),
        &outbox,
        session,
        command_line_open,
        &mut notice,
    );
    if notice.is_some() {
        active.notice = notice;
    }
}

/// Wires the editor's screen/landing foundation, its dedicated surface
/// renderer, and key forwarding.
pub struct EditorPlugin;

impl Plugin for EditorPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<EditorOpenRequested>()
            .init_resource::<ActiveEditor>()
            .init_resource::<EditorOutboxState>()
            .add_systems(
                Update,
                (
                    handle_editor_open,
                    handle_editor_events,
                    editor_sync_outbox,
                    editor_drain_landed,
                ),
            )
            .add_systems(OnEnter(Screen::Editor), render::spawn_editor_panel)
            .add_systems(OnExit(Screen::Editor), render::despawn_editor_panel)
            // Only dispatch and the paste are editor-screen-gated: the outbox
            // sync and the answer drain run ungated so keys queued right
            // before a Ctrl+Z suspend still deliver and report. The dispatch
            // runs after the central dispatcher — it consumes the GrabbedKey
            // stream it emits, and must see this frame's keys, not last
            // frame's.
            .add_systems(
                Update,
                (
                    editor_dispatch_keys,
                    editor_middle_click_paste,
                )
                    .run_if(in_state(Screen::Editor))
                    .after(crate::input::InputPhase::Dispatch),
            )
            // The surface build needs ComputedNode, so it runs post-layout; the
            // cursor sync reads the geometry the build just computed.
            .add_systems(
                PostUpdate,
                (
                    render::build_editor_surface.after(bevy::ui::UiSystems::Layout),
                    render::sync_editor_cursor.after(render::build_editor_surface),
                )
                    .run_if(in_state(Screen::Editor)),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::state::app::StatesPlugin;
    use kaijutsu_client::editor_outbox::{EditorSink, WRITER_STOPPED};
    use kaijutsu_kernel::editor::NO_SUCH_SESSION;
    use std::future::Future;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    fn lost(session: u64) -> String {
        format!("{NO_SUCH_SESSION} {session}")
    }

    #[test]
    fn no_such_session_is_session_lost() {
        // The kernel's verdict for a restarted-away session.
        assert!(is_session_lost_error(&lost(3)));
    }

    #[test]
    fn wire_wrapped_no_such_session_still_matches() {
        // Over the wire the message is wrapped by capnp/CallError; the
        // substring still identifies the verdict.
        assert!(is_session_lost_error(&format!(
            "RPC error: Cap'n Proto error: Failed: remote exception: Failed: {}",
            lost(3)
        )));
    }

    #[test]
    fn a_failed_answer_is_classified_by_its_text() {
        assert_eq!(landed_verdict(&lost(3)), LandedVerdict::SessionLost);
        assert_eq!(landed_verdict(WRITER_STOPPED), LandedVerdict::WriterStopped);
        assert_eq!(landed_verdict("connection closed"), LandedVerdict::Failed);
    }

    #[test]
    fn transient_errors_are_not_session_lost() {
        // A timeout or dropped connection must NOT evict a live editor — only
        // the kernel's explicit "no such session" does.
        assert!(!is_session_lost_error("connection closed"));
        assert!(!is_session_lost_error("editor_keys timed out"));
        assert!(!is_session_lost_error("editor: block not found in 019ef"));
    }

    #[test]
    fn a_paste_normalizes_newlines_and_skips_nothing() {
        assert_eq!(paste_payload("a\r\nb\rc\n", false), Ok(Some("a\nb\nc\n".to_string())));
        assert_eq!(paste_payload("", false), Ok(None));
    }

    #[test]
    fn a_paste_is_refused_while_the_command_line_is_open() {
        assert!(paste_payload("text", true).is_err());
        assert!(paste_payload("", true).is_err(), "refused before it is read as empty");
    }

    // ── the outbox seams, driven through the ECS ───────────────────────────

    #[derive(Clone)]
    enum Behavior {
        Ok,
        Fail(String),
        /// Wait for the gate, then answer.
        Gated(Arc<AtomicBool>),
    }

    #[derive(Clone)]
    struct FakeSink {
        behavior: Behavior,
        /// Calls that began.
        calls: Arc<AtomicUsize>,
    }

    impl FakeSink {
        fn new(behavior: Behavior) -> Self {
            Self { behavior, calls: Arc::default() }
        }

        fn answer(&self, session: u64) -> impl Future<Output = Result<EditorState, String>> + Send + use<> {
            let behavior = self.behavior.clone();
            self.calls.fetch_add(1, Ordering::SeqCst);
            async move {
                match behavior {
                    Behavior::Ok => Ok(state(session)),
                    Behavior::Fail(e) => Err(e),
                    Behavior::Gated(gate) => {
                        while !gate.load(Ordering::SeqCst) {
                            bevy::tasks::futures_lite::future::yield_now().await;
                        }
                        Ok(state(session))
                    }
                }
            }
        }
    }

    impl EditorSink for FakeSink {
        fn keys(&self, session: u64, _keys: &str) -> impl Future<Output = Result<EditorState, String>> {
            self.answer(session)
        }

        fn insert(&self, session: u64, _text: &str) -> impl Future<Output = Result<EditorState, String>> {
            self.answer(session)
        }
    }

    fn state(session: u64) -> EditorState {
        EditorState {
            session,
            text: "answered".to_string(),
            cursor: 0,
            mode: None,
            dirty: false,
            command_line: None,
            message: None,
            selection: None,
        }
    }

    fn install(app: &mut App, generation: u64, sink: FakeSink) {
        IoTaskPool::get_or_init(bevy::tasks::TaskPool::new);
        let (outbox, writer, landed) = editor_outbox::channel(sink);
        let writer = IoTaskPool::get().spawn(writer);
        app.world_mut()
            .resource_mut::<EditorOutboxState>()
            .install(generation, outbox, writer, landed);
    }

    fn test_app() -> App {
        let mut app = App::new();
        app.add_plugins(StatesPlugin)
            .init_state::<Screen>()
            .init_resource::<ActiveEditor>()
            .init_resource::<EditorOutboxState>()
            .add_systems(Update, editor_drain_landed);
        app
    }

    fn open_session(app: &mut App, session: u64, generation: u64) {
        app.world_mut().resource_mut::<ActiveEditor>().session = Some(EditorSessionView {
            session,
            path: "p".to_string(),
            generation,
            state: EditorState { text: "original".to_string(), ..state(session) },
        });
    }

    fn send(app: &mut App, session: u64, keys: &str) {
        app.world()
            .resource::<EditorOutboxState>()
            .keys(session, keys)
            .expect("queued");
    }

    /// Update until `done` holds, or fail after two seconds.
    fn wait_for(app: &mut App, what: &str, done: impl Fn(&App) -> bool) {
        let start = Instant::now();
        while !done(app) {
            assert!(start.elapsed() < Duration::from_secs(2), "timed out waiting for {what}");
            app.update();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn notice(app: &App) -> Option<String> {
        app.world().resource::<ActiveEditor>().notice.clone()
    }

    fn popped(app: &App) -> bool {
        matches!(
            app.world().resource::<NextState<Screen>>(),
            NextState::Pending(Screen::Conversation)
        )
    }

    #[test]
    fn a_failed_answer_lands_in_the_strip() {
        let mut app = test_app();
        open_session(&mut app, 7, 1);
        install(&mut app, 1, FakeSink::new(Behavior::Fail("boom".into())));
        send(&mut app, 7, "x");
        wait_for(&mut app, "a notice", |a| notice(a).is_some());
        assert!(notice(&app).unwrap().contains("boom"));
        assert!(!popped(&app), "a transient failure keeps the editor");
    }

    #[test]
    fn an_answer_applies_its_state_and_clears_the_notice() {
        let mut app = test_app();
        open_session(&mut app, 7, 1);
        app.world_mut().resource_mut::<ActiveEditor>().notice = Some("old".into());
        install(&mut app, 1, FakeSink::new(Behavior::Ok));
        send(&mut app, 7, "x");
        wait_for(&mut app, "the answer's state", |a| {
            a.world().resource::<ActiveEditor>().session.as_ref().is_some_and(|v| v.state.text == "answered")
        });
        assert_eq!(notice(&app), None);
    }

    #[test]
    fn session_lost_pops_only_the_matching_generation_and_session() {
        // A different generation: the failure is stale and must not pop.
        let mut app = test_app();
        open_session(&mut app, 7, 2);
        let sink = FakeSink::new(Behavior::Fail(lost(7)));
        let calls = sink.calls.clone();
        install(&mut app, 1, sink);
        send(&mut app, 7, "x");
        wait_for(&mut app, "the call", |_| calls.load(Ordering::SeqCst) == 1);
        for _ in 0..20 {
            app.update();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!popped(&app), "stale generation");
        assert!(app.world().resource::<ActiveEditor>().session.is_some());

        // A different session id: likewise.
        let mut app = test_app();
        open_session(&mut app, 8, 1);
        let sink = FakeSink::new(Behavior::Fail(lost(7)));
        let calls = sink.calls.clone();
        install(&mut app, 1, sink);
        send(&mut app, 7, "x");
        wait_for(&mut app, "the call", |_| calls.load(Ordering::SeqCst) == 1);
        for _ in 0..20 {
            app.update();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!popped(&app), "other session");

        // The match pops.
        let mut app = test_app();
        open_session(&mut app, 7, 1);
        install(&mut app, 1, FakeSink::new(Behavior::Fail(lost(7))));
        send(&mut app, 7, "x");
        wait_for(&mut app, "the pop", popped);
        assert!(app.world().resource::<ActiveEditor>().session.is_none());
    }

    #[test]
    fn replacing_the_outbox_cancels_the_old_writer_and_reports_what_it_lost() {
        let mut app = test_app();
        open_session(&mut app, 7, 1);
        let gate = Arc::new(AtomicBool::new(false));
        let old = FakeSink::new(Behavior::Gated(gate.clone()));
        let old_calls = old.calls.clone();
        install(&mut app, 1, old);
        send(&mut app, 7, "abc");
        wait_for(&mut app, "the first batch in flight", |_| old_calls.load(Ordering::SeqCst) == 1);
        send(&mut app, 7, "def"); // queued behind the in-flight batch

        let new = FakeSink::new(Behavior::Ok);
        install(&mut app, 2, new);
        gate.store(true, Ordering::SeqCst);

        wait_for(&mut app, "the loss notice", |a| {
            notice(a).is_some_and(|n| n.contains("lost across reconnect"))
        });
        assert!(notice(&app).unwrap().starts_with("2 editor call(s)"), "{:?}", notice(&app));
        for _ in 0..20 {
            app.update();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(old_calls.load(Ordering::SeqCst), 1, "the queued keys never reached the old handle");
        assert!(app.world().resource::<EditorOutboxState>().retiring.is_empty(), "the old receiver closed");
    }

    #[test]
    fn a_stopped_writer_is_replaced_by_the_next_sync() {
        let mut app = test_app();
        open_session(&mut app, 7, 1);
        install(&mut app, 1, FakeSink::new(Behavior::Fail(WRITER_STOPPED.into())));
        send(&mut app, 7, "x");
        wait_for(&mut app, "the writer to be dropped", |a| {
            !a.world().resource::<EditorOutboxState>().serves(1)
        });
    }
}
