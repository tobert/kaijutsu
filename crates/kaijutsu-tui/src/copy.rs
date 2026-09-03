//! Copy mode — the alternate screen's third occupant: `Ctrl+A [`, tmux's own
//! copy-mode chord (`.tmux.conf`'s `mode-keys vi`, `bind [ copy-mode`), and it
//! means the same thing here (`docs/tui.md`, guidance 7). The current
//! context's transcript becomes a buffer under vi motions — how a long tool
//! result is read whole, and how the conversation is scrolled from the
//! keyboard, since tool output no longer collapses by default
//! (`present::collapses_by_default`).
//!
//! **Freeze on open**, the same contract `diff.rs` keeps: the buffer is a
//! snapshot of `render_block` output taken the moment `Ctrl+A [` is pressed,
//! not a live view. A still-streaming block growing under the reader while
//! they scroll would move their place without saying so; the block-to-line
//! rendering that builds the snapshot lives at the edge
//! ([`crate::render::copy_buffer_lines`]), the same split diff keeps between
//! its screen and `kaijutsu-diff`'s model.
//!
//! Pure: a `Vec<Line<'static>>` in, cursor/scroll/search state out. No
//! terminal, no clock, no kernel — the way `picker.rs` and `compose.rs` stay
//! pure.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::present::Palette;

/// Body height assumed until the first frame is drawn, so a key that arrives
/// before one still steps a sane amount (`diff.rs`'s same convention).
const DEFAULT_BODY_LINES: usize = 20;

/// Rows the screen reserves for its own hint/search line.
const CHROME_LINES: u16 = 1;

/// One frozen transcript, and where the reader is inside it.
pub struct CopyScreen {
    /// The context's label, for the hint line.
    context_label: String,
    lines: Vec<Line<'static>>,
    /// The line the reader is on. Copy mode's motions are linewise only —
    /// there is no column cursor (`docs/tui.md`, guidance 7) — so a row index
    /// is the whole position.
    cursor: usize,
    /// First rendered row on screen.
    top: usize,
    /// Body height of the last frame drawn — a page step and both bottom
    /// stops are measured in it, and the key path has no terminal to ask.
    body_h: usize,
    /// `g` pressed once, waiting for a second `g` (vi's `gg`). Any other key
    /// cancels it.
    pending_g: bool,
    /// `v`'s linewise selection anchor — the other end is always the cursor.
    selection: Option<usize>,
    search: SearchState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchDirection {
    Forward,
    Backward,
}

impl SearchDirection {
    fn opposite(self) -> Self {
        match self {
            SearchDirection::Forward => SearchDirection::Backward,
            SearchDirection::Backward => SearchDirection::Forward,
        }
    }

    fn glyph(self) -> char {
        match self {
            SearchDirection::Forward => '/',
            SearchDirection::Backward => '?',
        }
    }
}

/// The `/` or `?` bar, live while it is being typed.
struct SearchPrompt {
    direction: SearchDirection,
    text: String,
}

#[derive(Default)]
struct SearchState {
    prompt: Option<SearchPrompt>,
    /// Every matching line, ascending — rebuilt on each committed search
    /// (case-insensitive substring is enough, `docs/tui.md`, guidance 7).
    matches: Vec<usize>,
    direction: SearchDirection,
}

impl Default for SearchDirection {
    fn default() -> Self {
        SearchDirection::Forward
    }
}

impl CopyScreen {
    /// Open on `lines`, entering at the bottom — the newest block — the way
    /// tmux enters copy mode at the current screen.
    pub fn new(context_label: impl Into<String>, lines: Vec<Line<'static>>) -> Self {
        let cursor = lines.len().saturating_sub(1);
        let mut screen = Self {
            context_label: context_label.into(),
            lines,
            cursor,
            top: 0,
            body_h: DEFAULT_BODY_LINES,
            pending_g: false,
            selection: None,
            search: SearchState::default(),
        };
        screen.follow_cursor();
        screen
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// The body height of the last frame drawn.
    pub fn body_h(&self) -> usize {
        self.body_h
    }

    /// 0-indexed line the reader is on — exposed for tests; the hint line is
    /// what a player actually sees (`Self::frame`).
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    fn last_index(&self) -> usize {
        self.lines.len().saturating_sub(1)
    }

    fn set_cursor(&mut self, target: usize) {
        self.cursor = target.min(self.last_index());
        self.follow_cursor();
    }

    /// Move the window only when the cursor leaves it, the same rule
    /// `editor.rs`'s `editor_frame` follows for its own buffer.
    fn follow_cursor(&mut self) {
        if self.lines.is_empty() {
            self.top = 0;
            return;
        }
        if self.cursor < self.top {
            self.top = self.cursor;
        } else if self.cursor >= self.top + self.body_h {
            self.top = self.cursor + 1 - self.body_h;
        }
        self.top = self.top.min(self.lines.len().saturating_sub(self.body_h.min(self.lines.len())));
    }

    fn move_by(&mut self, delta: isize) {
        let next = (self.cursor as isize + delta).clamp(0, self.last_index() as isize) as usize;
        self.set_cursor(next);
    }

    fn move_to_top(&mut self) {
        self.set_cursor(0);
    }

    fn move_to_bottom(&mut self) {
        self.set_cursor(self.last_index());
    }

    // ── selection / yank (stretch) ──────────────────────────────────────

    fn toggle_selection(&mut self) {
        self.selection = if self.selection.is_some() {
            None
        } else {
            Some(self.cursor)
        };
    }

    fn selection_range(&self) -> Option<(usize, usize)> {
        self.selection.map(|anchor| (anchor.min(self.cursor), anchor.max(self.cursor)))
    }

    /// `y` — the selected lines' plain text, newline-joined, clearing the
    /// selection. `None` with no selection active: `y` alone does nothing,
    /// only `v` then `y` yanks (`docs/tui.md`, guidance 7 stretch).
    fn yank(&mut self) -> Option<String> {
        let (start, end) = self.selection_range()?;
        let text = (start..=end).map(|i| line_text(&self.lines[i])).collect::<Vec<_>>().join("\n");
        self.selection = None;
        Some(text)
    }

    // ── search ───────────────────────────────────────────────────────────

    fn open_prompt(&mut self, direction: SearchDirection) {
        self.search.prompt = Some(SearchPrompt { direction, text: String::new() });
    }

    fn commit_search(&mut self) {
        let Some(prompt) = self.search.prompt.take() else { return };
        if prompt.text.is_empty() {
            return;
        }
        let needle = prompt.text.to_lowercase();
        self.search.matches = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| line_text(l).to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
        self.search.direction = prompt.direction;
        self.jump_to_nearest_match();
    }

    fn jump_to_nearest_match(&mut self) {
        let Some(target) = (match self.search.direction {
            SearchDirection::Forward => self
                .search
                .matches
                .iter()
                .copied()
                .find(|&i| i >= self.cursor)
                .or_else(|| self.search.matches.first().copied()),
            SearchDirection::Backward => self
                .search
                .matches
                .iter()
                .rev()
                .copied()
                .find(|&i| i <= self.cursor)
                .or_else(|| self.search.matches.last().copied()),
        }) else {
            return;
        };
        self.set_cursor(target);
    }

    /// `n` (`same_direction = true`) or `N` (`false`) — step to the next
    /// match, wrapping around the whole buffer once it runs out. `false`
    /// when there is nothing to search for, so the caller can say "ignored"
    /// rather than "moved."
    fn step_match(&mut self, same_direction: bool) -> bool {
        if self.search.matches.is_empty() {
            return false;
        }
        let direction = if same_direction {
            self.search.direction
        } else {
            self.search.direction.opposite()
        };
        let target = match direction {
            SearchDirection::Forward => self
                .search
                .matches
                .iter()
                .copied()
                .find(|&i| i > self.cursor)
                .unwrap_or_else(|| self.search.matches[0]),
            SearchDirection::Backward => self
                .search
                .matches
                .iter()
                .rev()
                .copied()
                .find(|&i| i < self.cursor)
                .unwrap_or_else(|| *self.search.matches.last().expect("checked non-empty")),
        };
        self.set_cursor(target);
        true
    }

    // ── rendering ────────────────────────────────────────────────────────

    /// The rows to draw into a screen `height` tall: the body, and the hint
    /// line (or the search prompt, while one is being typed) — the same
    /// "every grown view renders its own keys" rule the picker and the
    /// ledger follow (`docs/tui.md`, "Keys").
    pub fn frame(&mut self, height: u16, palette: &Palette) -> Vec<Line<'static>> {
        let body_h = height.saturating_sub(CHROME_LINES).max(1) as usize;
        self.body_h = body_h;
        self.follow_cursor();

        let selection = self.selection_range();
        let mut out = Vec::with_capacity(height as usize);
        for i in 0..body_h {
            let idx = self.top + i;
            out.push(match self.lines.get(idx) {
                Some(line) if selection.is_some_and(|(s, e)| (s..=e).contains(&idx)) => {
                    overlay(line, palette.copy_selection())
                }
                Some(line) if idx == self.cursor && self.search.matches.binary_search(&idx).is_ok() => {
                    overlay(line, palette.copy_match())
                }
                Some(line) => line.clone(),
                None => Line::from(String::new()),
            });
        }

        out.push(match &self.search.prompt {
            Some(prompt) => Line::from(Span::styled(
                format!("{}{}", prompt.direction.glyph(), prompt.text),
                palette.status(),
            )),
            None => self.hint_line(palette),
        });
        out
    }

    /// Where the terminal's cursor goes: after the typed text on the search
    /// prompt row, or hidden while the hint line owns the bottom row.
    pub fn frame_cursor(&self, height: u16) -> Option<(u16, u16)> {
        let prompt = self.search.prompt.as_ref()?;
        let col = 1 + prompt.text.chars().count() as u16;
        Some((col, height.saturating_sub(1)))
    }

    fn hint_line(&self, palette: &Palette) -> Line<'static> {
        let position = if self.lines.is_empty() {
            "line 0/0".to_string()
        } else {
            format!("line {}/{}", self.cursor + 1, self.lines.len())
        };
        // Kept under 80 columns beside a position and a short label, so
        // `q leave` is never the part that falls off the right edge.
        let keys = "j/k  ^D/^U  gg/G  / search  Space mark  Enter copy  q leave";
        Line::from(Span::styled(
            format!("{position}   {}   {keys}", self.context_label),
            palette.status(),
        ))
    }
}

fn line_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Layer `style` over every span in `line`, keeping the span's own style
/// underneath (`Style::patch`) — the same way `render_block` layers a
/// markdown tone over a block's own.
fn overlay(line: &Line<'static>, style: Style) -> Line<'static> {
    Line::from(
        line.spans
            .iter()
            .map(|s| Span::styled(s.content.clone(), s.style.patch(style)))
            .collect::<Vec<_>>(),
    )
}

/// What one key did to the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyOutcome {
    /// `q`, or `Esc` with no selection active — give the inline viewport
    /// back.
    Close,
    /// The cursor or the viewport moved.
    Moved,
    /// `/` or `?` opened the search prompt.
    PromptOpened,
    /// A character was typed or erased in the search prompt.
    PromptChanged,
    /// `Enter` committed the search prompt.
    SearchCommitted,
    /// `Esc` closed the search prompt without moving the cursor.
    SearchCancelled,
    /// `y` yanked the selected lines — the caller emits `.0` over OSC 52
    /// ([`osc52_sequence`]) and leaves copy mode, the way tmux does
    /// (`docs/tui.md`, guidance 7 stretch).
    Yanked(String),
    /// Nothing this screen answers.
    Ignored,
}

/// Interpret one key against the screen. `body_h` is the drawn body height,
/// the same convention `diff::handle_key` follows.
pub fn handle_key(
    screen: &mut CopyScreen,
    key: &crossterm::event::KeyEvent,
    body_h: usize,
) -> CopyOutcome {
    use crossterm::event::{KeyCode, KeyModifiers};

    // The search prompt takes every key while it is open — the same
    // "the alternate screen takes it whole" rule the editor's own `:`-line
    // follows, one level down.
    if screen.search.prompt.is_some() {
        return match key.code {
            KeyCode::Esc => {
                screen.search.prompt = None;
                CopyOutcome::SearchCancelled
            }
            KeyCode::Enter => {
                screen.commit_search();
                CopyOutcome::SearchCommitted
            }
            KeyCode::Backspace => {
                screen.search.prompt.as_mut().expect("checked Some above").text.pop();
                CopyOutcome::PromptChanged
            }
            KeyCode::Char(c) => {
                screen.search.prompt.as_mut().expect("checked Some above").text.push(c);
                CopyOutcome::PromptChanged
            }
            _ => CopyOutcome::Ignored,
        };
    }

    if !matches!(key.code, KeyCode::Char('g')) {
        screen.pending_g = false;
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let half_page = (body_h / 2).max(1) as isize;
    let full_page = body_h as isize;

    match key.code {
        KeyCode::Char('q') => CopyOutcome::Close,
        KeyCode::Esc if screen.selection.is_some() => {
            screen.selection = None;
            CopyOutcome::Moved
        }
        KeyCode::Esc => CopyOutcome::Close,
        KeyCode::Char('j') | KeyCode::Down => {
            screen.move_by(1);
            CopyOutcome::Moved
        }
        KeyCode::Char('k') | KeyCode::Up => {
            screen.move_by(-1);
            CopyOutcome::Moved
        }
        KeyCode::Char('d') if ctrl => {
            screen.move_by(half_page);
            CopyOutcome::Moved
        }
        KeyCode::Char('u') if ctrl => {
            screen.move_by(-half_page);
            CopyOutcome::Moved
        }
        KeyCode::Char('f') if ctrl => {
            screen.move_by(full_page);
            CopyOutcome::Moved
        }
        KeyCode::Char('b') if ctrl => {
            screen.move_by(-full_page);
            CopyOutcome::Moved
        }
        KeyCode::PageDown => {
            screen.move_by(full_page);
            CopyOutcome::Moved
        }
        KeyCode::PageUp => {
            screen.move_by(-full_page);
            CopyOutcome::Moved
        }
        KeyCode::Char('g') => {
            if screen.pending_g {
                screen.pending_g = false;
                screen.move_to_top();
                CopyOutcome::Moved
            } else {
                screen.pending_g = true;
                CopyOutcome::Ignored
            }
        }
        KeyCode::Char('G') | KeyCode::End => {
            screen.move_to_bottom();
            CopyOutcome::Moved
        }
        KeyCode::Home => {
            screen.move_to_top();
            CopyOutcome::Moved
        }
        KeyCode::Char('/') => {
            screen.open_prompt(SearchDirection::Forward);
            CopyOutcome::PromptOpened
        }
        KeyCode::Char('?') => {
            screen.open_prompt(SearchDirection::Backward);
            CopyOutcome::PromptOpened
        }
        KeyCode::Char('n') => {
            if screen.step_match(true) {
                CopyOutcome::Moved
            } else {
                CopyOutcome::Ignored
            }
        }
        KeyCode::Char('N') => {
            if screen.step_match(false) {
                CopyOutcome::Moved
            } else {
                CopyOutcome::Ignored
            }
        }
        // `Space` marks and `Enter` copies: GNU screen's copy mode and
        // tmux's vi mode agree, and those are the hands this answers.
        // `v`/`y` are the vim spelling of the same two acts.
        KeyCode::Char('v') | KeyCode::Char(' ') => {
            screen.toggle_selection();
            CopyOutcome::Moved
        }
        KeyCode::Char('y') => match screen.yank() {
            Some(text) => CopyOutcome::Yanked(text),
            None => CopyOutcome::Ignored,
        },
        KeyCode::Enter => match screen.yank() {
            Some(text) => CopyOutcome::Yanked(text),
            None => CopyOutcome::Close,
        },
        _ => CopyOutcome::Ignored,
    }
}

/// The OSC 52 clipboard sequence for `text` — `ESC ] 52 ; c ; <base64> BEL`,
/// the only clipboard path over ssh; the targets are iTerm2 and the Linux
/// terminals (`docs/tui.md`, guidance 7). Pure: the caller writes the bytes
/// to the terminal, under the same lock every other write takes
/// (`run.rs`'s `term_lock`); this only encodes them.
pub fn osc52_sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64_encode(text.as_bytes()))
}

/// Standard base64, padded — hand-rolled rather than a new dependency: the
/// alphabet is fixed and OSC 52 is the one caller.
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 { ALPHABET[((n >> 6) & 0x3F) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[(n & 0x3F) as usize] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn lines(n: usize) -> Vec<Line<'static>> {
        (0..n).map(|i| Line::from(format!("line {i}"))).collect()
    }

    fn fixture(n: usize) -> CopyScreen {
        CopyScreen::new("kaijutsu", lines(n))
    }

    fn plain(lines: &[Line<'static>]) -> Vec<String> {
        lines.iter().map(line_text).collect()
    }

    /// Typed keystrokes for one search: the glyph, the pattern, then Enter.
    fn search(screen: &mut CopyScreen, glyph: char, pattern: &str) {
        assert_eq!(handle_key(screen, &press(KeyCode::Char(glyph)), 10), CopyOutcome::PromptOpened);
        for c in pattern.chars() {
            handle_key(screen, &press(KeyCode::Char(c)), 10);
        }
        assert_eq!(handle_key(screen, &press(KeyCode::Enter), 10), CopyOutcome::SearchCommitted);
    }

    // ── entry position ──────────────────────────────────────────────────

    #[test]
    fn it_enters_at_the_bottom_of_the_buffer() {
        let screen = fixture(50);
        assert_eq!(screen.cursor(), 49, "the newest block, the way tmux enters at the current screen");
    }

    #[test]
    fn an_empty_buffer_has_no_cursor_to_move() {
        let screen = fixture(0);
        assert_eq!(screen.cursor(), 0);
        assert!(screen.is_empty());
    }

    // ── motions ──────────────────────────────────────────────────────────

    #[test]
    fn j_and_k_move_one_line() {
        let mut screen = fixture(10);
        screen.set_cursor(5);
        assert_eq!(handle_key(&mut screen, &press(KeyCode::Char('j')), 10), CopyOutcome::Moved);
        assert_eq!(screen.cursor(), 6);
        assert_eq!(handle_key(&mut screen, &press(KeyCode::Up), 10), CopyOutcome::Moved);
        assert_eq!(screen.cursor(), 5);
    }

    #[test]
    fn j_stops_at_the_last_line_rather_than_overflowing() {
        let mut screen = fixture(3);
        screen.set_cursor(2);
        handle_key(&mut screen, &press(KeyCode::Char('j')), 10);
        assert_eq!(screen.cursor(), 2, "no line past the end to move to");
    }

    #[test]
    fn k_stops_at_the_first_line_rather_than_underflowing() {
        let mut screen = fixture(3);
        screen.set_cursor(0);
        handle_key(&mut screen, &press(KeyCode::Char('k')), 10);
        assert_eq!(screen.cursor(), 0);
    }

    #[test]
    fn ctrl_d_and_ctrl_u_move_half_a_page() {
        let mut screen = fixture(100);
        screen.set_cursor(0);
        handle_key(&mut screen, &ctrl('d'), 20);
        assert_eq!(screen.cursor(), 10, "half of a 20-row body");
        handle_key(&mut screen, &ctrl('u'), 20);
        assert_eq!(screen.cursor(), 0);
    }

    #[test]
    fn ctrl_f_and_ctrl_b_and_the_page_keys_move_a_full_page() {
        let mut screen = fixture(100);
        screen.set_cursor(0);
        handle_key(&mut screen, &ctrl('f'), 20);
        assert_eq!(screen.cursor(), 20);
        handle_key(&mut screen, &ctrl('b'), 20);
        assert_eq!(screen.cursor(), 0);
        handle_key(&mut screen, &press(KeyCode::PageDown), 20);
        assert_eq!(screen.cursor(), 20);
        handle_key(&mut screen, &press(KeyCode::PageUp), 20);
        assert_eq!(screen.cursor(), 0);
    }

    #[test]
    fn a_half_page_step_is_never_less_than_one_line() {
        // A one-row body: `body_h / 2 == 0`, and a page step that moved
        // nothing would strand the reader.
        let mut screen = fixture(10);
        screen.set_cursor(0);
        handle_key(&mut screen, &ctrl('d'), 1);
        assert_eq!(screen.cursor(), 1, "the page step floors at one line");
    }

    // ── gg / G ───────────────────────────────────────────────────────────

    #[test]
    fn gg_jumps_to_the_top() {
        let mut screen = fixture(50);
        assert_eq!(handle_key(&mut screen, &press(KeyCode::Char('g')), 10), CopyOutcome::Ignored, "the first g waits for a second");
        assert_eq!(handle_key(&mut screen, &press(KeyCode::Char('g')), 10), CopyOutcome::Moved);
        assert_eq!(screen.cursor(), 0);
    }

    #[test]
    fn a_single_g_is_cancelled_by_any_other_key() {
        let mut screen = fixture(50);
        handle_key(&mut screen, &press(KeyCode::Char('g')), 10);
        handle_key(&mut screen, &press(KeyCode::Char('j')), 10);
        // The cancelled `g` must not fire on a later lone `g`.
        assert_eq!(handle_key(&mut screen, &press(KeyCode::Char('g')), 10), CopyOutcome::Ignored);
        assert_ne!(screen.cursor(), 0);
    }

    #[test]
    fn capital_g_jumps_to_the_bottom() {
        let mut screen = fixture(50);
        screen.set_cursor(0);
        assert_eq!(handle_key(&mut screen, &press(KeyCode::Char('G')), 10), CopyOutcome::Moved);
        assert_eq!(screen.cursor(), 49);
    }

    // ── search ───────────────────────────────────────────────────────────

    #[test]
    fn search_is_a_case_insensitive_substring() {
        let mut screen = CopyScreen::new(
            "kaijutsu",
            vec![Line::from("one"), Line::from("Contains ALPHA here"), Line::from("three")],
        );
        screen.set_cursor(0);
        search(&mut screen, '/', "alpha");
        assert_eq!(screen.cursor(), 1);
    }

    #[test]
    fn n_wraps_around_to_the_first_match_past_the_last() {
        let mut screen = CopyScreen::new(
            "kaijutsu",
            vec![
                Line::from("alpha one"),
                Line::from("nope"),
                Line::from("alpha two"),
                Line::from("nope"),
                Line::from("nope"),
            ],
        );
        screen.set_cursor(0);
        search(&mut screen, '/', "alpha");
        assert_eq!(screen.cursor(), 0);
        assert!(handle_key(&mut screen, &press(KeyCode::Char('n')), 10) == CopyOutcome::Moved);
        assert_eq!(screen.cursor(), 2, "the second match");
        assert!(handle_key(&mut screen, &press(KeyCode::Char('n')), 10) == CopyOutcome::Moved);
        assert_eq!(screen.cursor(), 0, "n past the last match wraps to the first");
    }

    #[test]
    fn capital_n_wraps_backward_to_the_last_match_before_the_first() {
        let mut screen = CopyScreen::new(
            "kaijutsu",
            vec![Line::from("alpha one"), Line::from("nope"), Line::from("alpha two"), Line::from("nope")],
        );
        screen.set_cursor(0);
        search(&mut screen, '/', "alpha");
        assert_eq!(screen.cursor(), 0);
        assert_eq!(handle_key(&mut screen, &press(KeyCode::Char('N')), 10), CopyOutcome::Moved);
        assert_eq!(screen.cursor(), 2, "N before the first match wraps to the last");
    }

    #[test]
    fn a_search_prompt_with_no_matches_leaves_the_cursor_put() {
        let mut screen = fixture(10);
        screen.set_cursor(4);
        search(&mut screen, '/', "nowhere to be found");
        assert_eq!(screen.cursor(), 4);
    }

    #[test]
    fn n_and_capital_n_with_no_active_search_are_ignored() {
        let mut screen = fixture(10);
        screen.set_cursor(4);
        assert_eq!(handle_key(&mut screen, &press(KeyCode::Char('n')), 10), CopyOutcome::Ignored);
        assert_eq!(handle_key(&mut screen, &press(KeyCode::Char('N')), 10), CopyOutcome::Ignored);
        assert_eq!(screen.cursor(), 4);
    }

    #[test]
    fn esc_cancels_the_prompt_and_backspace_edits_it() {
        let mut screen = fixture(10);
        handle_key(&mut screen, &press(KeyCode::Char('/')), 10);
        handle_key(&mut screen, &press(KeyCode::Char('a')), 10);
        handle_key(&mut screen, &press(KeyCode::Char('b')), 10);
        assert_eq!(handle_key(&mut screen, &press(KeyCode::Backspace), 10), CopyOutcome::PromptChanged);
        assert_eq!(handle_key(&mut screen, &press(KeyCode::Esc), 10), CopyOutcome::SearchCancelled);
        assert!(screen.search.prompt.is_none());
    }

    /// While the prompt is open every key belongs to it — `q` types a
    /// literal `q` into the pattern rather than closing copy mode.
    #[test]
    fn q_types_into_an_open_search_prompt_rather_than_closing() {
        let mut screen = fixture(10);
        handle_key(&mut screen, &press(KeyCode::Char('/')), 10);
        assert_eq!(handle_key(&mut screen, &press(KeyCode::Char('q')), 10), CopyOutcome::PromptChanged);
        assert_eq!(screen.search.prompt.as_ref().unwrap().text, "q");
    }

    // ── leaving ──────────────────────────────────────────────────────────

    #[test]
    fn q_and_esc_close_without_mutating_the_frozen_buffer() {
        let mut screen = fixture(20);
        let before = plain(&screen.lines);
        assert_eq!(handle_key(&mut screen, &press(KeyCode::Char('q')), 10), CopyOutcome::Close);
        assert_eq!(handle_key(&mut screen, &press(KeyCode::Esc), 10), CopyOutcome::Close);
        assert_eq!(plain(&screen.lines), before, "closing must not touch the frozen buffer");
    }

    #[test]
    fn esc_with_a_selection_active_cancels_the_selection_instead_of_closing() {
        let mut screen = fixture(10);
        handle_key(&mut screen, &press(KeyCode::Char('v')), 10);
        assert!(screen.selection.is_some());
        assert_eq!(handle_key(&mut screen, &press(KeyCode::Esc), 10), CopyOutcome::Moved);
        assert!(screen.selection.is_none(), "the selection cleared");
        // Copy mode itself is still open — a second Esc is what leaves it.
        assert_eq!(handle_key(&mut screen, &press(KeyCode::Esc), 10), CopyOutcome::Close);
    }

    // ── selection / yank (stretch) ──────────────────────────────────────

    #[test]
    fn v_then_y_yanks_the_selected_lines() {
        let mut screen = fixture(10);
        screen.set_cursor(2);
        handle_key(&mut screen, &press(KeyCode::Char('v')), 10);
        handle_key(&mut screen, &press(KeyCode::Char('j')), 10);
        handle_key(&mut screen, &press(KeyCode::Char('j')), 10);
        assert_eq!(screen.cursor(), 4);
        let outcome = handle_key(&mut screen, &press(KeyCode::Char('y')), 10);
        assert_eq!(outcome, CopyOutcome::Yanked("line 2\nline 3\nline 4".to_string()));
        assert!(screen.selection.is_none(), "yanking clears the selection");
    }

    /// GNU screen and tmux's vi mode both mark with `Space` and copy with
    /// `Enter` — the keys Amy's hands know. `v`/`y` stay as the vim
    /// spelling of the same two acts.
    #[test]
    fn space_marks_and_enter_copies_and_leaves() {
        let mut screen = fixture(10);
        screen.set_cursor(2);
        handle_key(&mut screen, &press(KeyCode::Char(' ')), 10);
        assert!(screen.selection.is_some(), "Space starts the selection");
        handle_key(&mut screen, &press(KeyCode::Char('j')), 10);
        let outcome = handle_key(&mut screen, &press(KeyCode::Enter), 10);
        assert_eq!(outcome, CopyOutcome::Yanked("line 2\nline 3".to_string()));
    }

    /// `Enter` with nothing marked leaves copy mode, as tmux's
    /// copy-selection-and-cancel does with an empty selection.
    #[test]
    fn enter_with_no_selection_leaves() {
        let mut screen = fixture(10);
        assert_eq!(handle_key(&mut screen, &press(KeyCode::Enter), 10), CopyOutcome::Close);
    }

    #[test]
    fn y_with_no_selection_is_ignored() {
        let mut screen = fixture(10);
        assert_eq!(handle_key(&mut screen, &press(KeyCode::Char('y')), 10), CopyOutcome::Ignored);
    }

    // ── the hint line ────────────────────────────────────────────────────

    #[test]
    fn the_hint_line_carries_the_position_and_the_label() {
        let mut screen = fixture(50);
        screen.set_cursor(10);
        let frame = screen.frame(6, &Palette::builtin());
        let hint: String = frame.last().unwrap().spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(hint.contains("line 11/50"), "got {hint:?}");
        assert!(hint.contains("kaijutsu"), "got {hint:?}");
        assert!(hint.contains("q leave"), "got {hint:?}");
    }

    #[test]
    fn the_search_prompt_replaces_the_hint_line_while_typing() {
        let mut screen = fixture(10);
        handle_key(&mut screen, &press(KeyCode::Char('/')), 10);
        handle_key(&mut screen, &press(KeyCode::Char('a')), 10);
        let frame = screen.frame(6, &Palette::builtin());
        let bottom: String = frame.last().unwrap().spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(bottom, "/a");
    }

    // ── osc 52 ───────────────────────────────────────────────────────────

    #[test]
    fn osc52_encodes_the_known_base64_of_a_short_string() {
        assert_eq!(osc52_sequence("hi"), "\x1b]52;c;aGk=\x07");
    }
}
