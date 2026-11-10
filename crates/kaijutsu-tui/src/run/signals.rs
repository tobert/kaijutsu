//! Stop signals and `Ctrl+Z`: the loop ends on `SIGTERM` and `SIGHUP`, and
//! hands the terminal to the host shell on a suspend.

use std::io::{self, Stdout};

use anyhow::Result;
use crossterm::cursor::SetCursorStyle;
use crossterm::style::Print;
use crossterm::event::{

    DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::editor::{self};

use super::terminal::{ALTERNATE_SCROLL_OFF, ALTERNATE_SCROLL_ON, TermLock, pop_kitty_keyboard, pop_title, push_kitty_keyboard, push_title};

/// The signals that mean "stop": `SIGTERM` from a runner or `kill`, and
/// `SIGHUP` when the terminal goes away. Either ends the event loop the way
/// `:q` does. Off unix there are none, and `recv` never resolves.
#[cfg(unix)]
pub(super) struct StopSignals {
    term: tokio::signal::unix::Signal,
    hup: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl StopSignals {
    pub(super) fn listen() -> io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self { term: signal(SignalKind::terminate())?, hup: signal(SignalKind::hangup())? })
    }

    pub(super) async fn recv(&mut self) -> &'static str {
        tokio::select! {
            _ = self.term.recv() => "SIGTERM",
            _ = self.hup.recv() => "SIGHUP",
        }
    }
}

#[cfg(not(unix))]
pub(super) struct StopSignals;

#[cfg(not(unix))]
impl StopSignals {
    pub(super) fn listen() -> io::Result<Self> {
        Ok(Self)
    }

    pub(super) async fn recv(&mut self) -> &'static str {
        std::future::pending().await
    }
}

/// Hand the terminal back to the host shell: give the screen back, leave raw
/// mode, stop ourselves the way a shell job does, and take the screen again
/// when `SIGCONT` brings us back — vim's `stoptermcap`/`starttermcap` order
/// (`docs/tui.md`, "The `:` line": `Ctrl+Z` is a single suspend, not a
/// toggle).
pub(super) fn suspend(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    term_lock: &TermLock,
) -> Result<()> {
    // Held across the stop so the key reader cannot read in cooked mode.
    let _guard = term_lock.lock();
    let _ = terminal.flush();
    pop_title();
    // Before `?1049l`, the same order as `restore_terminal`.
    pop_kitty_keyboard();
    let _ = crossterm::execute!(
        io::stdout(),
        Print(ALTERNATE_SCROLL_OFF),
        DisableFocusChange,
        SetCursorStyle::DefaultUserShape,
        DisableBracketedPaste
    );
    editor::abandon();
    let _ = disable_raw_mode();

    raise_stop();

    // Back from SIGCONT, onto a fresh alternate screen: the terminal cleared
    // it on the way out, so the next frame has to paint every cell rather
    // than diff against what ratatui last drew.
    enable_raw_mode()?;
    crossterm::execute!(io::stdout(), EnableBracketedPaste)?;
    editor::take_screen()?;
    // After `?1049h`, the same order as `enter_terminal`.
    push_kitty_keyboard();
    crossterm::execute!(io::stdout(), Print(ALTERNATE_SCROLL_ON), EnableFocusChange)?;
    push_title();
    // A resize at the current size, not `Terminal::clear`: clear asks the
    // terminal where the cursor is, and this client never does. Both clear
    // the screen and reset the back buffer, which is what a freshly retaken
    // screen needs before the next frame diffs against it.
    let size = terminal.size()?;
    terminal.resize(ratatui::layout::Rect::new(0, 0, size.width, size.height))?;
    Ok(())
}

/// Stop this process with `SIGTSTP` — the unix suspend, not an emulation of
/// it, so the shell's job control sees a stopped job and `fg` resumes it.
#[cfg(unix)]
fn raise_stop() {
    // SAFETY: `raise` takes an integer and is async-signal-safe.
    unsafe {
        libc::raise(libc::SIGTSTP);
    }
}

/// There is no `SIGTSTP` off unix; the toggle still works and the second tap
/// says why nothing happened.
#[cfg(not(unix))]
fn raise_stop() {
    tracing::warn!("suspend is a unix gesture; nothing to raise here");
}
