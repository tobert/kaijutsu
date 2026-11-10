//! Taking, holding and giving back the terminal: the alternate screen, the
//! title and keyboard stacks, the cursor shape, and the lock that arbitrates
//! crossterm's reader.

use std::io::{self, Stdout};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::cursor::SetCursorStyle;
use crossterm::style::Print;
use crossterm::event::Event;
use crossterm::event::{

    DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    EndSynchronizedUpdate, SetTitle, disable_raw_mode, enable_raw_mode,
};
use parking_lot::Mutex;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::app::App;
use crate::compose::CursorShape;

use crate::copy;
use crate::editor::{self, ScreenMode};

/// How long the key reader blocks on one poll before releasing the terminal
/// lock. It bounds how long a redraw waits for the lock, so it is short.
const KEY_POLL: Duration = Duration::from_millis(20);

/// crossterm's internal event reader is one shared resource, and a blocking
/// read holds it. A reader mid-poll while raw mode goes away reads a cooked
/// terminal, and the keys typed at the shell prompt are lost. One mutex
/// arbitrates: the key reader takes it for the length of one [`KEY_POLL`],
/// and every terminal operation takes it too.
pub(super) type TermLock = Arc<Mutex<()>>;

/// Read keys on a blocking thread, one bounded poll at a time.
///
/// A dedicated thread rather than `crossterm::event::EventStream`: the stream
/// polls with no timeout, which holds crossterm's internal reader forever and
/// leaves no window for the exit path to stop it.
pub(super) fn spawn_key_reader(term_lock: TermLock, tx: mpsc::Sender<Event>) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    std::thread::spawn(move || {
        loop {
            // Checked under the lock: `leave_terminal` sets the flag and
            // then takes the lock, so a reader that was waiting for it sees
            // the flag before it can poll a cooked terminal.
            let ready = {
                let _guard = term_lock.lock();
                if stopped.load(Ordering::SeqCst) {
                    return;
                }
                crossterm::event::poll(KEY_POLL).unwrap_or(false)
            };
            if ready {
                let event = {
                    let _guard = term_lock.lock();
                    crossterm::event::read()
                };
                match event {
                    Ok(event) => {
                        if tx.blocking_send(event).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "terminal input ended");
                        return;
                    }
                }
            }
            // Unlocked, so a redraw waiting on the lock always gets a window.
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    stop
}

/// The cursor shape a frame of `app` wants: the draft's mode in the
/// conversation, the buffer's mode in the editor, and a block on the screens
/// that read rather than type (`docs/tui.md`, "Compose").
pub(super) fn wanted_cursor_shape(app: &App) -> CursorShape {
    match &app.screen {
        ScreenMode::Conversation => app.compose.cursor_shape(),
        ScreenMode::Editor(screen) => CursorShape::for_mode(screen.state.mode.as_deref()),
        ScreenMode::Diff(_) => CursorShape::Block,
    }
}

/// The window title a frame of `app` wants: the context on screen, named
/// the way the status line names it (`docs/tui.md`, "What owning the screen
/// lets us use"). `None` before a context is attached — the shell's own
/// title stands until there is something to say.
pub(super) fn wanted_title(app: &App) -> Option<String> {
    app.current.map(|id| format!("{} — kaijutsu", app.label_for(id)))
}

/// The title to write, and `sent` updated — `None` when the terminal
/// already carries it, or when there is nothing to name. Pure, so the rule
/// is tested without a terminal; [`set_title`] is the write.
fn title_to_send(sent: &mut Option<String>, want: Option<String>) -> Option<String> {
    let want = want?;
    if sent.as_deref() == Some(want.as_str()) {
        return None;
    }
    *sent = Some(want.clone());
    Some(want)
}

/// Set the window title when it differs from what was last sent (OSC 0).
/// The title the shell had is on xterm's title stack, pushed when the
/// screen was taken and popped by [`restore_terminal`].
pub(super) fn set_title(sent: &mut Option<String>, want: Option<String>) -> Result<()> {
    let Some(title) = title_to_send(sent, want) else {
        return Ok(());
    };
    crossterm::execute!(io::stdout(), SetTitle(title)).context("set the window title")
}

/// Send `want` to the terminal when it differs from what was last sent —
/// vim's `t_SI`/`t_EI`, as DECSCUSR. Steady shapes: blink is the terminal's
/// own preference and it keeps it for the default shape restored on exit.
pub(super) fn set_cursor_shape(sent: &mut Option<CursorShape>, want: CursorShape) -> Result<()> {
    if *sent == Some(want) {
        return Ok(());
    }
    let style = match want {
        CursorShape::Block => SetCursorStyle::SteadyBlock,
        CursorShape::Bar => SetCursorStyle::SteadyBar,
        CursorShape::Underline => SetCursorStyle::SteadyUnderScore,
    };
    crossterm::execute!(io::stdout(), style).context("set the cursor shape")?;
    *sent = Some(want);
    Ok(())
}

/// Take the screen for the session: raw mode, bracketed paste, the alternate
/// screen, and one full-screen `Terminal` over it.
///
/// One terminal for the whole session. A full-screen viewport is anchored to
/// nothing, so no frame asks the terminal where the cursor is and a slow hop
/// never stalls a redraw (`docs/tui.md`, "The owned screen").
pub(super) fn enter_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    // A paste arrives as one `Event::Paste` — text, not keystrokes — so a
    // newline inside it is a newline in the draft, not an Enter.
    crossterm::execute!(io::stdout(), EnableBracketedPaste)?;
    editor::take_screen()?;
    // Kitty keeps a separate keyboard-enhancement flag stack per screen
    // buffer, so the push has to land after `?1049h`, not before it
    // (`docs/tui.md`, "What owning the screen lets us use").
    push_kitty_keyboard();
    crossterm::execute!(io::stdout(), Print(ALTERNATE_SCROLL_ON), EnableFocusChange)?;
    push_title();
    Terminal::new(CrosstermBackend::new(io::stdout()))
}

/// Alternate scroll, DECSET 1007. With mouse reporting off — which this
/// client never turns on — a terminal holding the alternate screen turns
/// each wheel tick into arrow-key presses, which is how the wheel scrolls
/// the transcript (`docs/tui.md`, "The mouse stays the terminal's"). xterm
/// needs the mode; kitty and foot do it by default. crossterm has no
/// constant for it, so the bytes go out as they are.
pub(super) const ALTERNATE_SCROLL_ON: &str = "\x1b[?1007h";
pub(super) const ALTERNATE_SCROLL_OFF: &str = "\x1b[?1007l";

/// xterm's title stack: push the shell's own title before the first
/// [`set_title`], pop it back on the way out. The stack is how the title is
/// given back exactly as it was found — an empty title on exit would leave
/// a wezterm tab blank instead. crossterm has no constant for either, so
/// the bytes go out as they are.
const TITLE_PUSH: &str = "\x1b[22;0t";
const TITLE_POP: &str = "\x1b[23;0t";

/// Whether this process has a title on the terminal's stack, the same shape
/// as `editor::ENTERED` and for the same reason: [`restore_terminal`] is
/// idempotent and runs from the exit path, the panic hook and the signal
/// path, while a pop that answers no push takes the *shell's* own saved
/// title off the stack.
static TITLE_PUSHED: AtomicBool = AtomicBool::new(false);

/// Claim a one-way flag: true when this call is the one that changed it.
/// The push and the pop are each done once, whoever gets there first.
fn claim(flag: &AtomicBool, want: bool) -> bool {
    flag.swap(want, Ordering::SeqCst) != want
}

/// Push the shell's own window title, once.
pub(super) fn push_title() {
    if claim(&TITLE_PUSHED, true) {
        let _ = crossterm::execute!(io::stdout(), Print(TITLE_PUSH));
    }
}

/// Pop the title this process pushed, if it pushed one; a no-op otherwise.
pub(super) fn pop_title() {
    if claim(&TITLE_PUSHED, false) {
        let _ = crossterm::execute!(io::stdout(), Print(TITLE_POP));
    }
}

/// The kitty keyboard protocol is on by request, never by probe: this
/// client's zero-terminal-query invariant
/// (`the_client_never_asks_where_the_cursor_is`) rules out
/// `supports_keyboard_enhancement`, which sends `CSI ? u` to ask. Set once,
/// before the screen is taken (`run::run`), from `--kitty-keyboard`.
pub(super) static KITTY_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Whether this process has a keyboard-enhancement flag stack frame pushed,
/// the same shape as `TITLE_PUSHED`: `restore_terminal` is idempotent and
/// runs from the exit path, the panic hook and the signal path, while a
/// blind pop when nothing was pushed lands on a terminal that ignores it
/// (unlike a title pop, which would take the shell's own saved title off
/// its stack).
static KITTY_PUSHED: AtomicBool = AtomicBool::new(false);

/// `DISAMBIGUATE_ESCAPE_CODES` only — not `REPORT_EVENT_TYPES`: `keys.rs`
/// already ignores `KeyEventKind::Release`, and there is no path here that
/// wants a key-up event.
const KEYBOARD_ENHANCEMENT_FLAGS: KeyboardEnhancementFlags =
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES;

/// Push one keyboard-enhancement flag stack frame, once, and only when
/// `--kitty-keyboard` asked for it.
pub(super) fn push_kitty_keyboard() {
    if KITTY_REQUESTED.load(Ordering::SeqCst) && claim(&KITTY_PUSHED, true) {
        let _ = crossterm::execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(KEYBOARD_ENHANCEMENT_FLAGS)
        );
    }
}

/// Pop the keyboard-enhancement flag stack frame this process pushed, if it
/// pushed one; a no-op otherwise.
pub(super) fn pop_kitty_keyboard() {
    if claim(&KITTY_PUSHED, false) {
        let _ = crossterm::execute!(io::stdout(), PopKeyboardEnhancementFlags);
    }
}

/// Leave the terminal the way we found it. Best-effort on every step: a
/// failure here must not mask the error that ended the loop.
///
/// Nothing is printed on the way out: leaving the alternate screen restores
/// the shell's own screen, and `:q` is quiet (`docs/tui.md`, "The owned
/// screen").
pub(super) fn leave_terminal(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    term_lock: &TermLock,
    reader_stop: &AtomicBool,
) {
    // The reader finishes the poll it is in, sees the flag, and stops; the
    // lock below waits out that poll before raw mode goes.
    reader_stop.store(true, Ordering::SeqCst);
    let _guard = term_lock.lock();
    let _ = terminal.flush();
    restore_terminal();
}

/// Put the terminal back the way the host shell had it: the main screen if
/// this process took the alternate one, the shell's own cursor shape, cooked
/// input.
/// Idempotent and best-effort. The normal exit, the panic hook, and the
/// signal path all come through here, so every way out agrees.
pub fn restore_terminal() {
    // A panic inside a frame unwinds past that frame's own
    // `EndSynchronizedUpdate`, and a terminal left inside an update paints
    // nothing. Ending one that was never begun is a no-op.
    let _ = crossterm::execute!(io::stdout(), EndSynchronizedUpdate);
    // Kitty keeps a separate keyboard-enhancement flag stack per screen
    // buffer, so the pop has to land before `?1049l`, not after it — the
    // reverse of the title-stack pop just below, which does not care.
    pop_kitty_keyboard();
    editor::abandon();
    pop_title();
    // `?1004l` and `?1007l` go out blind: resetting a mode the terminal
    // never had on is a no-op, unlike a title pop, which would take the
    // shell's own saved title off the stack.
    let _ = crossterm::execute!(
        io::stdout(),
        Print(ALTERNATE_SCROLL_OFF),
        DisableFocusChange,
        SetCursorStyle::DefaultUserShape,
        DisableBracketedPaste
    );
    let _ = disable_raw_mode();
}

/// Write the OSC 52 clipboard sequence (`copy::osc52_sequence`) directly to
/// stdout, under the same lock every other terminal write takes — a yank in
/// the scrolled transcript is the only caller (`docs/tui.md`, "Scrolling is
/// copy mode").
pub(super) fn write_osc52(term_lock: &TermLock, text: &str) -> io::Result<()> {
    use std::io::Write as _;
    let _guard = term_lock.lock();
    let mut stdout = io::stdout();
    stdout.write_all(copy::osc52_sequence(text).as_bytes())?;
    stdout.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::ContextId;
    use crate::run::feeds::tests::watched;

    // ────────────────────────────────────────────────────────────────────
    // The window title (docs/tui.md, "What owning the screen lets us use")
    // ────────────────────────────────────────────────────────────────────

    /// The title names the context on screen, so a wezterm tab reads like a
    /// tmux window. Nothing is named before a context is attached.
    #[test]
    fn the_window_title_names_the_context_on_screen() {
        let mut app = App::new("amy");
        assert_eq!(wanted_title(&app), None, "no context, nothing to name");

        let id = ContextId::new();
        watched(&mut app, id);
        app.switch_to(id);
        assert_eq!(
            wanted_title(&app),
            Some(format!("{} — kaijutsu", id.short())),
            "an unlabeled context is named by its short id, as the status line names it"
        );
    }

    /// The one place the title is sent: a repeat is not written again, and a
    /// switch to another context is.
    #[test]
    fn a_title_is_sent_once_per_change() {
        let mut sent: Option<String> = None;
        let mut written: Vec<String> = Vec::new();
        let mut send = |sent: &mut Option<String>, want: Option<String>| {
            if let Some(title) = title_to_send(sent, want) {
                written.push(title);
            }
        };
        send(&mut sent, Some("probe — kaijutsu".to_string()));
        send(&mut sent, Some("probe — kaijutsu".to_string()));
        send(&mut sent, Some("other — kaijutsu".to_string()));
        send(&mut sent, None);
        assert_eq!(written, vec!["probe — kaijutsu".to_string(), "other — kaijutsu".to_string()]);
    }

    /// The title stack is popped exactly as often as it is pushed.
    ///
    /// [`restore_terminal`] is idempotent and runs from the exit path, the
    /// panic hook and the signal path — `leave_terminal` then the hook is
    /// two calls — while a panic before the screen is taken runs it with no
    /// push behind it. A pop that answers no push takes the *shell's* own
    /// saved title off the stack.
    #[test]
    fn a_title_is_popped_once_and_only_after_a_push() {
        let flag = AtomicBool::new(false);
        assert!(!claim(&flag, false), "nothing to pop before a push");
        assert!(claim(&flag, true), "the first push is the one that writes");
        assert!(!claim(&flag, true), "a second push writes nothing");
        assert!(claim(&flag, false), "the first pop is the one that writes");
        assert!(!claim(&flag, false), "a second pop writes nothing");
    }
}
