//! The editor screen — the alternate screen's vi occupant.
//!
//! The kernel owns the editor session; this module is a renderer and a key
//! forwarder, exactly as the Bevy app is (`docs/vi.md`, "App renderer"). Every
//! key travels to `editor_keys` in the kernel's vi notation, and the resulting
//! [`EditorState`] is what gets drawn. There is no local VimMachine, no mode
//! detection, and no quit detection: `ZZ`/`ZQ`/`:q` are ordinary keys, and the
//! kernel answers them with an `EditorClosed` push.
//!
//! The screen **takes the alternate screen** and returns the inline viewport
//! untouched when the session ends (`docs/tui.md`, ruling 1). The transcript in
//! the terminal's scrollback is never redrawn, so it cannot be disturbed.
//!
//! Pure but for the terminal handle in [`enter`]/[`leave`]: the frame builder
//! takes a state and a width and returns lines, so a `TestBackend` renders the
//! same frames a real terminal gets.

use std::io::{self, Stdout};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use kaijutsu_client::EditorState;
use ratatui::backend::CrosstermBackend;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Terminal, TerminalOptions, Viewport};

use crate::present::Palette;

/// What has displaced the inline viewport.
///
/// The closed set is ruling 1's: the conversation flows to scrollback, a grown
/// view grows the viewport, and the editor, the diff viewer and copy mode take
/// the alternate screen. A fourth *kind* of surface — one that is neither a
/// grown view nor a vim-shaped fullscreen occupant — is a design
/// conversation, not a patch.
#[derive(Default)]
pub enum ScreenMode {
    /// The inline viewport (`docs/tui.md`, "Conversation").
    #[default]
    Inline,
    /// A kernel-owned vi session.
    Editor(EditorScreen),
    /// A frozen diff.
    Diff(crate::diff::DiffScreen),
    /// `Ctrl+A [` — the frozen transcript under vi motions
    /// (`docs/tui.md`, "Copy mode").
    Copy(crate::copy::CopyScreen),
}

impl ScreenMode {
    /// Whether the alternate screen is up. The key path early-returns on this,
    /// which is what bypasses the `Ctrl+A` prefix and `Ctrl+C` while a vi
    /// surface is live.
    pub fn is_alternate(&self) -> bool {
        !matches!(self, ScreenMode::Inline)
    }

    /// The live editor session, when one is on screen.
    pub fn editor(&self) -> Option<&EditorScreen> {
        match self {
            ScreenMode::Editor(e) => Some(e),
            _ => None,
        }
    }

    pub fn editor_mut(&mut self) -> Option<&mut EditorScreen> {
        match self {
            ScreenMode::Editor(e) => Some(e),
            _ => None,
        }
    }
}

/// One kernel editor session, and where the viewport sits over its buffer.
pub struct EditorScreen {
    /// The kernel session handle — the currency of `editor_keys`.
    pub session: u64,
    /// The path being edited, for the mode line.
    pub path: String,
    /// The latest renderer-facing snapshot, seeded from the `open_editor`
    /// signal and kept fresh by the `subscribeEditor` push.
    pub state: EditorState,
    /// First buffer row drawn.
    top: usize,
    /// First buffer column drawn. There is no wrapping; a long line scrolls
    /// horizontally to keep the cursor on screen.
    left: usize,
}

impl EditorScreen {
    pub fn new(session: u64, path: impl Into<String>, state: EditorState) -> Self {
        Self {
            session,
            path: path.into(),
            state,
            top: 0,
            left: 0,
        }
    }
}

/// Rows the editor reserves below the buffer: the mode line and the `:`-line.
const CHROME_LINES: u16 = 2;

/// What one editor frame draws.
pub struct EditorFrame {
    pub lines: Vec<Line<'static>>,
    /// Where the terminal's cursor goes, in cells within the frame.
    pub cursor: (u16, u16),
}

/// Translate a char offset into `text` to a `(row, column)` position.
///
/// Both are counted in characters, and an offset past the end lands at the end
/// — the kernel is the cursor's owner, so a disagreement about length must not
/// panic the renderer.
pub fn cursor_row_col(text: &str, offset: u64) -> (usize, usize) {
    let mut row = 0usize;
    let mut col = 0usize;
    for (seen, ch) in text.chars().enumerate() {
        if seen as u64 >= offset {
            return (row, col);
        }
        if ch == '\n' {
            row += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (row, col)
}

/// Build one editor frame: the buffer, the mode line, and the `:`-line.
///
/// Takes `&mut` because the scroll offsets are clamped here — the cursor is
/// kernel state and the viewport follows it, so the same call that draws the
/// cursor is the one that decides which rows are on screen.
pub fn editor_frame(
    screen: &mut EditorScreen,
    width: u16,
    height: u16,
    palette: &Palette,
) -> EditorFrame {
    let body_h = height.saturating_sub(CHROME_LINES).max(1) as usize;
    let cols = width.max(1) as usize;
    let (row, col) = cursor_row_col(&screen.state.text, screen.state.cursor);

    // Keep the cursor on screen without recentering on every keystroke: move
    // the window only when the cursor leaves it, the way vi does.
    if row < screen.top {
        screen.top = row;
    } else if row >= screen.top + body_h {
        screen.top = row + 1 - body_h;
    }
    if col < screen.left {
        screen.left = col;
    } else if col >= screen.left + cols {
        screen.left = col + 1 - cols;
    }

    let text_style = palette.editor_text();
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(height as usize);
    let buffer: Vec<&str> = screen.state.text.split('\n').collect();
    for i in 0..body_h {
        match buffer.get(screen.top + i) {
            Some(row_text) => {
                let visible: String = row_text
                    .chars()
                    .skip(screen.left)
                    .take(cols)
                    .collect();
                lines.push(Line::from(Span::styled(visible, text_style)));
            }
            // vi's empty-line marker, so "past the end" is never mistaken for a
            // run of blank lines in the file.
            None => lines.push(Line::from(Span::styled("~", palette.editor_filler()))),
        }
    }

    lines.push(mode_line(screen, row, col, cols, palette));
    let (strip, strip_cursor) = strip_line(&screen.state, palette);
    lines.push(strip);

    let cursor = match strip_cursor {
        Some(c) => (c.min(width.saturating_sub(1)), height.saturating_sub(1)),
        None => (
            (col - screen.left) as u16,
            (row - screen.top) as u16,
        ),
    };
    EditorFrame { lines, cursor }
}

/// `foo.kai [+]                          -- INSERT --            12,3`
fn mode_line(
    screen: &EditorScreen,
    row: usize,
    col: usize,
    cols: usize,
    palette: &Palette,
) -> Line<'static> {
    let mut left = screen.path.clone();
    if screen.state.dirty {
        left.push_str(" [+]");
    }
    let mode = screen.state.mode.clone().unwrap_or_default();
    let position = format!("{},{}", row + 1, col + 1);
    let right = if mode.is_empty() {
        position
    } else {
        format!("{mode}   {position}")
    };
    let gap = cols.saturating_sub(left.chars().count() + right.chars().count());
    Line::from(vec![
        Span::styled(left, palette.editor_status()),
        Span::styled(" ".repeat(gap), palette.editor_status()),
        Span::styled(right, palette.editor_status()),
    ])
}

/// The bottom row: the `:`-line while command mode is active, otherwise the
/// transient message, otherwise blank.
///
/// Returns the column the terminal cursor belongs in when the `:`-line owns it.
fn strip_line(state: &EditorState, palette: &Palette) -> (Line<'static>, Option<u16>) {
    if let Some(command) = &state.command_line {
        let col = command.chars().count() as u16;
        return (
            Line::from(Span::styled(command.clone(), palette.editor_command())),
            Some(col),
        );
    }
    match &state.message {
        Some(message) => (
            Line::from(Span::styled(message.clone(), palette.editor_message())),
            None,
        ),
        None => (Line::from(String::new()), None),
    }
}

/// Translate one crossterm key into the kernel's vi notation, or `None` for a
/// key the notation cannot express.
///
/// The vocabulary is `kaijutsu-editor`'s `parse_keys`: a literal char, `<Esc>`,
/// `<CR>`, `<BS>`, `<Tab>`, a bare space, and `<C-x>` chords. An arrow or a
/// function key has no token, and `parse_keys` silently drops an unknown one,
/// so it is refused here instead of being sent and swallowed. A literal `<`
/// would open a token, and `parse_keys` has no `<lt>` escape, so it is refused
/// too rather than corrupting the buffer.
pub fn key_notation(key: &KeyEvent) -> Option<String> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let named = match key.code {
        KeyCode::Esc => Some("<Esc>"),
        KeyCode::Enter => Some("<CR>"),
        KeyCode::Backspace => Some("<BS>"),
        KeyCode::Tab => Some("<Tab>"),
        _ => None,
    };
    if let Some(n) = named {
        return Some(n.to_string());
    }
    let KeyCode::Char(c) = key.code else {
        return None;
    };
    if ctrl {
        return Some(format!("<C-{}>", c.to_ascii_lowercase()));
    }
    if c == '<' {
        return None;
    }
    Some(c.to_string())
}

/// The kernel's `open_editor` peer signal, decoded.
///
/// **This is the only notification that a session opened.** The
/// `subscribeEditor` push channel carries `StateChanged` and `Closed` only —
/// `Kernel::editor_open_as` publishes neither — so a client that watches the
/// push stream alone never learns that `vi <path>` ran. The signal fans out to
/// the *submitter principal's* attached peers, which is why the TUI attaches
/// as one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorOpen {
    pub path: String,
    pub state: EditorState,
}

/// Decode an `open_editor` signal's JSON parameters.
///
/// The shape is the kernel's `EditorState::to_json` plus `path`, the same
/// payload the Bevy app's peer dispatcher reads. An open carries no transient
/// message — that field is set only when a later `:`-line errors, and it
/// arrives on the push channel.
pub fn parse_open_signal(params: &[u8]) -> Result<EditorOpen, String> {
    #[derive(serde::Deserialize)]
    struct Signal {
        session: u64,
        path: String,
        text: String,
        cursor: u64,
        mode: Option<String>,
        dirty: bool,
        #[serde(default)]
        command_line: Option<String>,
    }
    let signal: Signal =
        serde_json::from_slice(params).map_err(|e| format!("invalid open_editor params: {e}"))?;
    Ok(EditorOpen {
        path: signal.path,
        state: EditorState {
            session: signal.session,
            text: signal.text,
            cursor: signal.cursor,
            mode: signal.mode,
            dirty: signal.dirty,
            command_line: signal.command_line,
            message: None,
        },
    })
}

// ────────────────────────────────────────────────────────────────────────────
// Screen-mode transitions
// ────────────────────────────────────────────────────────────────────────────
//
// Every transition into and out of the alternate screen is a pure function on
// [`App`](crate::app::App) so the whole lifecycle — open, close, session lost,
// connection lost — is testable without a kernel and without a terminal. The
// event loop calls these; the terminal follows the state on the next frame.

/// Where a key goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyRoute {
    /// The surface holding the alternate screen takes it, whole. The `Ctrl+A`
    /// prefix and the `Ctrl+C` double-tap never see it — the editor is the
    /// sanctioned raw key reader (`docs/input.md`), so `Ctrl+A` there is vim's
    /// increment and `Ctrl+C` is vim's interrupt.
    AlternateScreen,
    /// The prefix machine and the compose line.
    Prefix,
}

/// Which way this client's next key goes.
pub fn route_key(app: &crate::app::App) -> KeyRoute {
    if app.screen.is_alternate() {
        KeyRoute::AlternateScreen
    } else {
        KeyRoute::Prefix
    }
}

/// Land an `open_editor` signal: the alternate screen's editor takes over.
pub fn enter_editor(app: &mut crate::app::App, open: EditorOpen) {
    app.screen = ScreenMode::Editor(EditorScreen::new(
        open.state.session,
        open.path,
        open.state,
    ));
}

/// Apply an editor push. Returns whether the frame changed.
///
/// `EditorClosed` is what `:q`, `ZZ` and `ZQ` produce: the kernel alone knows
/// the mode, so it alone decides a quit, and this is where the inline viewport
/// comes back. A push for another session is not ours — several sessions can be
/// open at once and the channel is kernel-wide.
pub fn apply_push(app: &mut crate::app::App, event: &kaijutsu_client::ServerEvent) -> bool {
    use kaijutsu_client::ServerEvent;
    match event {
        ServerEvent::EditorStateChanged { state } => {
            let Some(screen) = app.screen.editor_mut() else {
                return false;
            };
            if screen.session != state.session {
                return false;
            }
            screen.state = state.clone();
            true
        }
        ServerEvent::EditorClosed { session_id } => {
            if !app
                .screen
                .editor()
                .is_some_and(|s| s.session == *session_id)
            {
                return false;
            }
            app.screen = ScreenMode::Inline;
            true
        }
        _ => false,
    }
}

/// A connection that will not come back cannot carry an editor session's
/// keystrokes. Give the inline viewport back with a notice rather than leaving
/// a buffer that echoes nothing. Returns whether the screen changed.
pub fn leave_on_disconnect(
    app: &mut crate::app::App,
    status: &kaijutsu_client::ConnectionStatus,
) -> bool {
    if !matches!(status, kaijutsu_client::ConnectionStatus::Terminal { .. })
        || !app.screen.is_alternate()
    {
        return false;
    }
    app.screen = ScreenMode::Inline;
    app.note("kernel connection ended; left the editor");
    true
}

/// The screen this client falls back to when the kernel disowns a session.
///
/// A kernel restart leaves the sessions gone while the persisted kernel id is
/// unchanged, so the reconnect looks ordinary and only the next `editor_keys`
/// call reports it. Returns whether the screen changed.
pub fn leave_on_session_lost(app: &mut crate::app::App, notice: &str) -> bool {
    if !app.screen.is_alternate() {
        return false;
    }
    app.screen = ScreenMode::Inline;
    app.note(notice.to_string());
    true
}

/// Does an `editor_keys` failure mean the kernel session is gone?
///
/// The kernel answers `editor: no such session N` when the id is not in its
/// registry, which is exactly what a kernel restart produces: the sessions are
/// in memory, the persisted kernel id is unchanged, and the reconnect looks
/// ordinary. Matching that verdict and nothing else keeps a momentary RPC
/// hiccup from evicting a live editor (`docs/vi.md`, restart staleness).
pub fn is_session_lost(error: &str) -> bool {
    error.contains("no such session")
}

/// The alternate screen, with a full-screen terminal over it.
///
/// A second `Terminal` rather than a mode switch on the inline one: the inline
/// viewport's height and its remembered cursor row are what put the transcript
/// in scrollback, and rebuilding them after every `:q` is how that gets lost.
/// This one is dropped in [`leave`] and the inline terminal draws its next
/// frame over a screen the terminal itself restored.
pub struct AltScreen {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl AltScreen {
    /// Draw `lines` full-screen. `cursor` places the terminal's cursor; `None`
    /// hides it, which is what a surface with no insertion point wants.
    pub fn draw(
        &mut self,
        lines: Vec<Line<'static>>,
        cursor: Option<(u16, u16)>,
    ) -> io::Result<()> {
        self.terminal.draw(|frame| {
            let area = frame.area();
            frame.render_widget(Paragraph::new(lines), area);
            if let Some(position) = cursor {
                frame.set_cursor_position(position);
            }
        })?;
        Ok(())
    }

    /// The screen's size, so a caller can build a frame that fits.
    pub fn size(&self) -> io::Result<ratatui::layout::Size> {
        self.terminal.size()
    }
}

/// Take the alternate screen. Raw mode is already on.
pub fn enter() -> io::Result<AltScreen> {
    crossterm::execute!(
        io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        crossterm::cursor::Show
    )?;
    let terminal = Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Fullscreen,
        },
    )?;
    Ok(AltScreen { terminal })
}

/// Give the alternate screen back. Best-effort on every step: a failure here
/// must not mask the error that ended the session.
pub fn leave(screen: AltScreen) {
    drop(screen);
    let _ = crossterm::execute!(io::stdout(), crossterm::terminal::LeaveAlternateScreen);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn state(text: &str, cursor: u64) -> EditorState {
        EditorState {
            session: 7,
            text: text.to_string(),
            cursor,
            mode: None,
            dirty: false,
            command_line: None,
            message: None,
        }
    }

    fn screen(text: &str, cursor: u64) -> EditorScreen {
        EditorScreen::new(7, "notes.kai", state(text, cursor))
    }

    /// Render a frame through a real ratatui backend and return its rows, so
    /// the test sees what a terminal would.
    fn rows(screen: &mut EditorScreen, width: u16, height: u16) -> Vec<String> {
        let palette = Palette::builtin();
        let frame = editor_frame(screen, width, height, &palette);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|f| f.render_widget(Paragraph::new(frame.lines), f.area()))
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn the_buffer_draws_with_the_mode_line_under_it() {
        let mut s = screen("alpha\nbeta\n", 0);
        let out = rows(&mut s, 24, 5);
        assert_eq!(out[0], "alpha");
        assert_eq!(out[1], "beta");
        assert_eq!(out[2], "");
        assert!(out[3].starts_with("notes.kai"), "mode line: {:?}", out[3]);
        assert!(out[3].ends_with("1,1"), "mode line: {:?}", out[3]);
    }

    #[test]
    fn rows_past_the_end_of_the_buffer_are_tildes() {
        let mut s = screen("only\n", 0);
        let out = rows(&mut s, 24, 6);
        // "only", the empty line after the terminator, then fillers.
        assert_eq!(out[2], "~");
        assert_eq!(out[3], "~");
    }

    #[test]
    fn the_cursor_lands_on_its_row_and_column() {
        // Offset 8 is 'e' on the second line of "alpha\nbeta".
        let mut s = screen("alpha\nbeta", 8);
        let palette = Palette::builtin();
        let frame = editor_frame(&mut s, 24, 6, &palette);
        assert_eq!(frame.cursor, (2, 1));
    }

    #[test]
    fn a_cursor_past_the_end_lands_at_the_end() {
        let s = screen("ab", 99);
        let (row, col) = cursor_row_col(&s.state.text, s.state.cursor);
        assert_eq!((row, col), (0, 2));
    }

    #[test]
    fn the_mode_label_and_the_dirty_marker_reach_the_mode_line() {
        let mut s = screen("x", 0);
        s.state.mode = Some("-- INSERT --".to_string());
        s.state.dirty = true;
        let out = rows(&mut s, 40, 4);
        assert!(out[2].starts_with("notes.kai [+]"), "{:?}", out[2]);
        assert!(out[2].contains("-- INSERT --"), "{:?}", out[2]);
    }

    #[test]
    fn the_command_line_owns_the_bottom_row_and_the_cursor() {
        let mut s = screen("x", 0);
        s.state.command_line = Some(":wq".to_string());
        let palette = Palette::builtin();
        let frame = editor_frame(&mut s, 40, 4, &palette);
        assert_eq!(frame.cursor, (3, 3), "cursor sits after the typed command");
        let out = rows(&mut s, 40, 4);
        assert_eq!(out[3], ":wq");
    }

    #[test]
    fn a_message_draws_when_no_command_is_active() {
        let mut s = screen("x", 0);
        s.state.message = Some("E492: Not an editor command: frobnicate".to_string());
        let out = rows(&mut s, 60, 4);
        assert!(out[3].starts_with("E492"), "{:?}", out[3]);
    }

    /// The `:`-line wins the bottom row: a stale message must not shadow the
    /// command the user is typing.
    #[test]
    fn the_command_line_beats_a_message() {
        let mut s = screen("x", 0);
        s.state.message = Some("E37".to_string());
        s.state.command_line = Some(":q".to_string());
        let out = rows(&mut s, 40, 4);
        assert_eq!(out[3], ":q");
    }

    #[test]
    fn the_window_follows_the_cursor_down_and_back_up() {
        let text = (0..40).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
        let mut s = screen(&text, 0);
        let palette = Palette::builtin();
        // Row 0 with a 6-row body: no scroll.
        let frame = editor_frame(&mut s, 20, 8, &palette);
        assert_eq!(frame.cursor.1, 0);
        assert_eq!(s.top, 0);

        // Put the cursor on row 20 and redraw.
        s.state.cursor = text
            .chars()
            .enumerate()
            .filter(|(_, c)| *c == '\n')
            .nth(19)
            .map(|(i, _)| i as u64 + 1)
            .expect("20 newlines");
        editor_frame(&mut s, 20, 8, &palette);
        assert_eq!(s.top, 15, "the window scrolled to hold row 20");

        // Back to row 0.
        s.state.cursor = 0;
        editor_frame(&mut s, 20, 8, &palette);
        assert_eq!(s.top, 0);
    }

    #[test]
    fn a_long_line_scrolls_horizontally_to_keep_the_cursor_visible() {
        let text = "x".repeat(200);
        let mut s = screen(&text, 150);
        let palette = Palette::builtin();
        let frame = editor_frame(&mut s, 20, 4, &palette);
        assert_eq!(s.left, 131);
        assert_eq!(frame.cursor.0, 19);
    }

    // ── key notation ────────────────────────────────────────────────────────

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn a_literal_char_forwards_as_itself() {
        assert_eq!(key_notation(&press(KeyCode::Char('i'))).as_deref(), Some("i"));
        assert_eq!(key_notation(&press(KeyCode::Char('Z'))).as_deref(), Some("Z"));
        assert_eq!(key_notation(&press(KeyCode::Char(' '))).as_deref(), Some(" "));
    }

    #[test]
    fn named_keys_take_their_tokens() {
        for (code, want) in [
            (KeyCode::Esc, "<Esc>"),
            (KeyCode::Enter, "<CR>"),
            (KeyCode::Backspace, "<BS>"),
            (KeyCode::Tab, "<Tab>"),
        ] {
            assert_eq!(key_notation(&press(code)).as_deref(), Some(want));
        }
    }

    #[test]
    fn a_control_chord_lowercases() {
        let key = KeyEvent::new(KeyCode::Char('W'), KeyModifiers::CONTROL);
        assert_eq!(key_notation(&key).as_deref(), Some("<C-w>"));
    }

    /// `parse_keys` drops an unknown `<...>` token silently, so a key with no
    /// token is refused here rather than sent and swallowed.
    #[test]
    fn keys_the_notation_cannot_express_are_refused() {
        assert_eq!(key_notation(&press(KeyCode::Left)), None);
        assert_eq!(key_notation(&press(KeyCode::F(3))), None);
        assert_eq!(key_notation(&press(KeyCode::Char('<'))), None);
    }

    // ── session-lost verdict ────────────────────────────────────────────────

    #[test]
    fn no_such_session_is_the_restart_verdict() {
        assert!(is_session_lost("editor: no such session 3"));
        assert!(is_session_lost(
            "RPC error: Cap'n Proto error: Failed: remote exception: \
             Failed: editor: no such session 3"
        ));
    }

    #[test]
    fn a_transient_failure_never_evicts_a_live_editor() {
        assert!(!is_session_lost("connection closed"));
        assert!(!is_session_lost("editor_keys timed out"));
        assert!(!is_session_lost("editor: block not found in 019ef"));
    }

    // ── the open signal ─────────────────────────────────────────────────────

    #[test]
    fn an_open_signal_decodes_into_a_session_and_a_state() {
        let params = br#"{"session":4,"path":"/config/rc/coder/create/S00-stance.kai",
            "text":"hello\n","cursor":0,"mode":null,"dirty":false}"#;
        let open = parse_open_signal(params).expect("decodes");
        assert_eq!(open.path, "/config/rc/coder/create/S00-stance.kai");
        assert_eq!(open.state.session, 4);
        assert_eq!(open.state.text, "hello\n");
        assert_eq!(open.state.message, None, "an open carries no message");
    }

    #[test]
    fn a_malformed_open_signal_fails_loudly() {
        assert!(parse_open_signal(b"{\"session\":4}").is_err());
        assert!(parse_open_signal(b"not json").is_err());
    }

    // ── screen mode ─────────────────────────────────────────────────────────

    #[test]
    fn the_inline_viewport_is_not_the_alternate_screen() {
        assert!(!ScreenMode::Inline.is_alternate());
        assert!(ScreenMode::Editor(screen("x", 0)).is_alternate());
    }

    // ── transitions ─────────────────────────────────────────────────────────

    use kaijutsu_client::{ConnectionStatus, ServerEvent};

    fn app_with_editor() -> crate::app::App {
        let mut app = crate::app::App::new("amy");
        enter_editor(
            &mut app,
            EditorOpen {
                path: "notes.kai".to_string(),
                state: state("hello\n", 0),
            },
        );
        app
    }

    #[test]
    fn an_open_signal_takes_the_alternate_screen() {
        let app = app_with_editor();
        assert!(app.screen.is_alternate());
        let live = app.screen.editor().expect("an editor screen");
        assert_eq!(live.session, 7);
        assert_eq!(live.path, "notes.kai");
    }

    #[test]
    fn a_state_push_for_this_session_updates_the_buffer() {
        let mut app = app_with_editor();
        let mut next = state("hello world\n", 5);
        next.dirty = true;
        assert!(apply_push(&mut app, &ServerEvent::EditorStateChanged { state: next }));
        let live = app.screen.editor().expect("still open");
        assert_eq!(live.state.text, "hello world\n");
        assert!(live.state.dirty);
    }

    /// The push channel is kernel-wide, so a sibling's session arrives here
    /// too. It must not overwrite the buffer on screen.
    #[test]
    fn a_state_push_for_another_session_is_not_ours() {
        let mut app = app_with_editor();
        let mut other = state("someone else\n", 0);
        other.session = 99;
        assert!(!apply_push(&mut app, &ServerEvent::EditorStateChanged { state: other }));
        assert_eq!(
            app.screen.editor().expect("still open").state.text,
            "hello\n"
        );
    }

    /// `:q` / `ZZ` / `ZQ` are ordinary keys; the kernel answers them with a
    /// close push, and that is what gives the inline viewport back.
    #[test]
    fn a_close_push_gives_the_inline_viewport_back() {
        let mut app = app_with_editor();
        assert!(apply_push(&mut app, &ServerEvent::EditorClosed { session_id: 7 }));
        assert!(!app.screen.is_alternate());
    }

    #[test]
    fn a_close_push_for_another_session_leaves_ours_up() {
        let mut app = app_with_editor();
        assert!(!apply_push(&mut app, &ServerEvent::EditorClosed { session_id: 99 }));
        assert!(app.screen.is_alternate());
    }

    #[test]
    fn a_terminal_connection_leaves_the_editor_with_a_notice() {
        let mut app = app_with_editor();
        assert!(leave_on_disconnect(
            &mut app,
            &ConnectionStatus::Terminal {
                reason: "kernel gone".to_string()
            }
        ));
        assert!(!app.screen.is_alternate());
        assert_eq!(app.notice(), Some("kernel connection ended; left the editor"));
    }

    /// A reconnecting connection is not a lost one — the actor retries
    /// forever, and evicting a live editor on a hiccup is the bug.
    #[test]
    fn a_transient_connection_state_keeps_the_editor_up() {
        let mut app = app_with_editor();
        assert!(!leave_on_disconnect(&mut app, &ConnectionStatus::Connecting { attempt: 2 }));
        assert!(app.screen.is_alternate());
    }

    #[test]
    fn a_lost_session_drops_back_to_the_inline_viewport() {
        let mut app = app_with_editor();
        assert!(leave_on_session_lost(&mut app, "editor session lost"));
        assert!(!app.screen.is_alternate());
        assert_eq!(app.notice(), Some("editor session lost"));
    }

    // ── the key bypass ──────────────────────────────────────────────────────

    #[test]
    fn the_alternate_screen_takes_every_key_before_the_prefix() {
        let app = app_with_editor();
        assert_eq!(route_key(&app), KeyRoute::AlternateScreen);
        let mut inline = crate::app::App::new("amy");
        assert_eq!(route_key(&inline), KeyRoute::Prefix);
        inline.screen = ScreenMode::Editor(screen("x", 0));
        assert_eq!(route_key(&inline), KeyRoute::AlternateScreen);
    }

    /// `Ctrl+A` in the editor is vim's increment, not the screen prefix, and
    /// `Ctrl+C` is vim's interrupt, not the quit double-tap. Both reach the
    /// kernel as notation because the route above bypasses the prefix machine.
    #[test]
    fn ctrl_a_and_ctrl_c_reach_the_kernel_while_the_editor_is_live() {
        let app = app_with_editor();
        assert_eq!(route_key(&app), KeyRoute::AlternateScreen);
        for (c, want) in [('a', "<C-a>"), ('c', "<C-c>")] {
            let key = KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
            assert_eq!(key_notation(&key).as_deref(), Some(want));
        }
    }
}
