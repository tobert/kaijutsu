//! The shell surface (`Ctrl+Z`) — kaish through `shell_execute`, the gated
//! path a human's shell already takes (`docs/gate-and-shell-split.md`).
//!
//! ```text
//!   kaijutsu ▸ /v/ctx/7f/kaish-arith $ kj stage exclude 019c…#12 && kj fork
//! ```
//!
//! **Two cursors, never mixed.** `kaijutsu` is the acting context — what a
//! statement runs as — and the path is cwd, where you are looking. They move
//! independently, so the prompt renders both: the acting context must be
//! legible before you act (`docs/tui.md`, "Melted from the ssh shell
//! design"). A context the kernel has no recorded cwd for renders no path
//! segment rather than a guessed one.
//!
//! A command's output arrives as blocks through the context mirror and the
//! transcript prints it like any other block. Nothing here echoes output.
//!
//! `Ctrl+Z` once toggles the surface; `Ctrl+Z Ctrl+Z` inside
//! [`crate::app::DOUBLE_TAP`] undoes that toggle and suspends the process for
//! real (`crate::run` raises `SIGTSTP`).
//!
//! Pure: no RPC, no terminal, and the only clock is the `Instant` a caller
//! hands to [`Shell::press_ctrl_z`].

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, DOUBLE_TAP};
use crate::present::Palette;

/// Between the acting context and the cwd.
pub const CURSOR_SEPARATOR: &str = " ▸ ";

/// What a keystroke asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellAction {
    /// Nothing this surface owns.
    Ignored,
    /// The line changed; redraw and nothing else.
    Edited,
    /// `Enter` — run this through `shell_execute`.
    Run(String),
}

/// What `Ctrl+Z` meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtrlZ {
    /// The first press: the shell surface came up, or went away.
    Toggled,
    /// The second press inside the window: the toggle is undone and the
    /// process suspends.
    Suspend,
}

/// The shell surface's state.
#[derive(Debug, Default)]
pub struct Shell {
    /// Whether the shell surface holds the input region.
    pub active: bool,
    line: String,
    /// The insertion point, as a char offset into `line`.
    cursor: usize,
    /// Commands run this session, oldest first. Local to the process — the
    /// kernel's own history is a different surface.
    history: Vec<String>,
    /// Where `Up`/`Down` have walked to, as an index into `history`.
    browsing: Option<usize>,
    /// The line set aside when history browsing started, restored by walking
    /// back down past the newest entry.
    stashed: Option<String>,
    /// The acting context's cwd, as the kernel last reported it. `None` when
    /// the kernel has no cwd recorded for the context.
    cwd: Option<String>,
    last_ctrl_z: Option<Instant>,
}

impl Shell {
    pub fn new() -> Self {
        Self::default()
    }

    /// The line being typed.
    pub fn line(&self) -> &str {
        &self.line
    }

    /// The insertion point, as a char offset.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Take the kernel's answer for the acting context's cwd.
    pub fn set_cwd(&mut self, cwd: Option<String>) {
        self.cwd = cwd;
    }

    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// Commands run this session, oldest first.
    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// `Ctrl+Z`. The first press toggles the surface; a second press inside
    /// [`DOUBLE_TAP`] undoes that toggle and asks for a real suspend.
    pub fn press_ctrl_z(&mut self, now: Instant) -> CtrlZ {
        let doubled = self
            .last_ctrl_z
            .is_some_and(|prev| now.duration_since(prev) <= DOUBLE_TAP);
        self.active = !self.active;
        if doubled {
            self.last_ctrl_z = None;
            CtrlZ::Suspend
        } else {
            self.last_ctrl_z = Some(now);
            CtrlZ::Toggled
        }
    }

    /// Interpret one keystroke while the shell surface holds the input region.
    pub fn press(&mut self, key: KeyEvent) -> ShellAction {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                // Kill the line, readline's `Ctrl+U`.
                KeyCode::Char('u') => {
                    self.set_line(String::new());
                    ShellAction::Edited
                }
                _ => ShellAction::Ignored,
            };
        }
        match key.code {
            KeyCode::Enter => {
                let line = std::mem::take(&mut self.line);
                self.cursor = 0;
                self.browsing = None;
                self.stashed = None;
                if line.trim().is_empty() {
                    return ShellAction::Edited;
                }
                // Consecutive duplicates are one entry — walking history is
                // the point, and a repeated command makes it longer for
                // nothing.
                if self.history.last() != Some(&line) {
                    self.history.push(line.clone());
                }
                ShellAction::Run(line)
            }
            KeyCode::Char(c) => {
                let at = self.byte_offset(self.cursor);
                self.line.insert(at, c);
                self.cursor += 1;
                self.browsing = None;
                ShellAction::Edited
            }
            KeyCode::Backspace => {
                if self.cursor == 0 {
                    return ShellAction::Edited;
                }
                let at = self.byte_offset(self.cursor - 1);
                self.line.remove(at);
                self.cursor -= 1;
                self.browsing = None;
                ShellAction::Edited
            }
            KeyCode::Delete => {
                if self.cursor >= self.len() {
                    return ShellAction::Edited;
                }
                let at = self.byte_offset(self.cursor);
                self.line.remove(at);
                self.browsing = None;
                ShellAction::Edited
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                ShellAction::Edited
            }
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.len());
                ShellAction::Edited
            }
            KeyCode::Home => {
                self.cursor = 0;
                ShellAction::Edited
            }
            KeyCode::End => {
                self.cursor = self.len();
                ShellAction::Edited
            }
            KeyCode::Up => {
                self.walk_history_back();
                ShellAction::Edited
            }
            KeyCode::Down => {
                self.walk_history_forward();
                ShellAction::Edited
            }
            _ => ShellAction::Ignored,
        }
    }

    /// Step one entry older. The line being typed is stashed on the first
    /// step and comes back by walking forward past the newest entry.
    fn walk_history_back(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.browsing {
            None => {
                self.stashed = Some(self.line.clone());
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(n) => n - 1,
        };
        self.browsing = Some(next);
        self.set_line(self.history[next].clone());
    }

    /// Step one entry newer, past the newest entry back to the stashed line.
    fn walk_history_forward(&mut self) {
        let Some(n) = self.browsing else {
            return;
        };
        if n + 1 < self.history.len() {
            self.browsing = Some(n + 1);
            self.set_line(self.history[n + 1].clone());
        } else {
            self.browsing = None;
            let stashed = self.stashed.take().unwrap_or_default();
            self.set_line(stashed);
        }
    }

    /// Replace the line, cursor at the end.
    fn set_line(&mut self, line: String) {
        self.line = line;
        self.cursor = self.len();
    }

    fn len(&self) -> usize {
        self.line.chars().count()
    }

    /// Char offset → byte offset, for `String::insert` / `remove`.
    fn byte_offset(&self, chars: usize) -> usize {
        self.line
            .char_indices()
            .nth(chars)
            .map(|(i, _)| i)
            .unwrap_or(self.line.len())
    }
}

/// `kaijutsu ▸ /v/ctx/7f/kaish-arith $ ` — the acting context, then cwd.
///
/// The two cursors move independently, so both are rendered. A context with
/// no cwd recorded renders the label and `$` alone.
pub fn prompt(label: &str, cwd: Option<&str>) -> String {
    match cwd {
        Some(path) => format!("{label}{CURSOR_SEPARATOR}{path} $ "),
        None => format!("{label} $ "),
    }
}

/// The shell surface's input region: one line, the prompt then what is typed.
pub fn prompt_lines(app: &App, width: u16, palette: &Palette) -> Vec<Line<'static>> {
    let label = app
        .current_info()
        .map(|c| c.label.clone())
        .filter(|l| !l.is_empty())
        .or_else(|| app.current.map(|id| id.short()))
        .unwrap_or_else(|| "no context".to_string());
    let head = prompt(&label, app.shell.cwd());
    let body = app.shell.line().to_string();
    let mut spans = vec![
        Span::styled(head.clone(), palette.status()),
        Span::styled(body.clone(), palette.compose()),
    ];
    // The double-tap hint sits where compose's mode banner does, because the
    // gesture is the one thing about this surface a figure cannot show.
    let hint = "Ctrl+Z Ctrl+Z suspends";
    let used = head.width() + body.width() + hint.width();
    if used < usize::from(width) {
        spans.push(Span::styled(
            " ".repeat(usize::from(width) - used),
            palette.compose(),
        ));
        spans.push(Span::styled(hint, palette.divider()));
    }
    vec![Line::from(spans)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn typed(shell: &mut Shell, text: &str) {
        for c in text.chars() {
            shell.press(press(KeyCode::Char(c)));
        }
    }

    fn run(shell: &mut Shell, text: &str) -> ShellAction {
        typed(shell, text);
        shell.press(press(KeyCode::Enter))
    }

    #[test]
    fn ctrl_z_once_toggles_the_shell_surface() {
        let mut shell = Shell::new();
        assert_eq!(shell.press_ctrl_z(Instant::now()), CtrlZ::Toggled);
        assert!(shell.active);
    }

    #[test]
    fn ctrl_z_twice_inside_the_window_undoes_the_toggle_and_suspends() {
        let mut shell = Shell::new();
        let t0 = Instant::now();
        assert_eq!(shell.press_ctrl_z(t0), CtrlZ::Toggled);
        assert_eq!(
            shell.press_ctrl_z(t0 + Duration::from_millis(300)),
            CtrlZ::Suspend
        );
        assert!(!shell.active, "the surface never came up");
    }

    #[test]
    fn ctrl_z_twice_outside_the_window_toggles_twice() {
        let mut shell = Shell::new();
        let t0 = Instant::now();
        assert_eq!(shell.press_ctrl_z(t0), CtrlZ::Toggled);
        assert_eq!(
            shell.press_ctrl_z(t0 + Duration::from_millis(900)),
            CtrlZ::Toggled
        );
        assert!(!shell.active, "the second press closed the surface");
    }

    /// The pair undoes its own toggle whichever surface was up, so `fg`
    /// brings back the screen that was there when you suspended.
    #[test]
    fn a_double_tap_from_an_open_shell_leaves_it_open() {
        let mut shell = Shell::new();
        let t0 = Instant::now();
        shell.press_ctrl_z(t0);
        assert!(shell.active);

        let t1 = t0 + Duration::from_millis(2_000);
        assert_eq!(shell.press_ctrl_z(t1), CtrlZ::Toggled);
        assert!(!shell.active);
        assert_eq!(
            shell.press_ctrl_z(t1 + Duration::from_millis(300)),
            CtrlZ::Suspend
        );
        assert!(shell.active, "the toggle was undone");
    }

    #[test]
    fn enter_runs_the_line() {
        let mut shell = Shell::new();
        assert_eq!(
            run(&mut shell, "kj fork"),
            ShellAction::Run("kj fork".to_string())
        );
        assert_eq!(shell.line(), "", "the line clears on run");
    }

    #[test]
    fn an_empty_line_runs_nothing() {
        let mut shell = Shell::new();
        assert_eq!(shell.press(press(KeyCode::Enter)), ShellAction::Edited);
        typed(&mut shell, "   ");
        assert_eq!(shell.press(press(KeyCode::Enter)), ShellAction::Edited);
        assert!(shell.history().is_empty());
    }

    #[test]
    fn up_and_down_walk_the_history() {
        let mut shell = Shell::new();
        run(&mut shell, "ls");
        run(&mut shell, "kj fork");
        shell.press(press(KeyCode::Up));
        assert_eq!(shell.line(), "kj fork");
        shell.press(press(KeyCode::Up));
        assert_eq!(shell.line(), "ls");
        shell.press(press(KeyCode::Up));
        assert_eq!(shell.line(), "ls", "the oldest entry is the end of the walk");
        shell.press(press(KeyCode::Down));
        assert_eq!(shell.line(), "kj fork");
        shell.press(press(KeyCode::Down));
        assert_eq!(shell.line(), "", "past the newest is the line you were typing");
    }

    #[test]
    fn walking_history_stashes_the_line_being_typed() {
        let mut shell = Shell::new();
        run(&mut shell, "ls");
        typed(&mut shell, "half typ");
        shell.press(press(KeyCode::Up));
        assert_eq!(shell.line(), "ls");
        shell.press(press(KeyCode::Down));
        assert_eq!(shell.line(), "half typ");
    }

    #[test]
    fn a_repeated_command_is_one_history_entry() {
        let mut shell = Shell::new();
        run(&mut shell, "ls");
        run(&mut shell, "ls");
        assert_eq!(shell.history(), ["ls"]);
    }

    #[test]
    fn editing_a_recalled_line_leaves_the_walk() {
        let mut shell = Shell::new();
        run(&mut shell, "ls");
        run(&mut shell, "kj fork");
        shell.press(press(KeyCode::Up));
        typed(&mut shell, "!");
        assert_eq!(shell.line(), "kj fork!");
        shell.press(press(KeyCode::Down));
        assert_eq!(shell.line(), "kj fork!", "a fresh line is not a history walk");
    }

    #[test]
    fn the_cursor_edits_in_chars_not_bytes() {
        let mut shell = Shell::new();
        typed(&mut shell, "café");
        shell.press(press(KeyCode::Left));
        typed(&mut shell, "X");
        assert_eq!(shell.line(), "cafXé");
        shell.press(press(KeyCode::Backspace));
        assert_eq!(shell.line(), "café");
        shell.press(press(KeyCode::Home));
        shell.press(press(KeyCode::Delete));
        assert_eq!(shell.line(), "afé");
    }

    #[test]
    fn ctrl_u_kills_the_line() {
        let mut shell = Shell::new();
        typed(&mut shell, "rm -rf /");
        shell.press(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(shell.line(), "");
        assert_eq!(shell.cursor(), 0);
    }

    /// Both cursors, never mixed: the acting context, then cwd.
    #[test]
    fn the_prompt_renders_both_cursors() {
        assert_eq!(
            prompt("kaijutsu", Some("/v/ctx/7f/kaish-arith")),
            "kaijutsu ▸ /v/ctx/7f/kaish-arith $ "
        );
    }

    #[test]
    fn a_context_with_no_recorded_cwd_renders_no_path() {
        assert_eq!(prompt("kaijutsu", None), "kaijutsu $ ");
    }
}
