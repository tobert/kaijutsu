//! Compose — a modalkit `VimMachine` over the kernel-owned input block.
//!
//! ```text
//!   ❯ and getattr? _                                                -- INSERT --
//! ```
//!
//! The draft is a shared block, not a local line: every keystroke becomes an
//! `edit_input` against the context's draft, and the text drawn here is what
//! the change feed says that block holds. A sibling typing into the same
//! draft shows up on this line (`docs/tui.md`, "Compose").
//!
//! The vi engine is [`kaijutsu_editor::EditorCore`] — the same pure modalkit
//! core the kernel's vi sessions run on. Its `EditOp`s are char-indexed,
//! which is exactly `edit_input`'s `(pos, insert, delete)` addressing, so
//! nothing translates between the two.
//!
//! Pure: no RPC, no terminal, and the only clock is the `Instant` a caller
//! hands to [`Compose::press`].

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use kaijutsu_editor::{EditOp, EditorCore};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, DOUBLE_TAP};
use crate::present::Palette;

/// The compose prompt. The shell surface has its own (`shell.rs`).
pub const PROMPT: &str = "❯ ";

/// What one keystroke asked the rest of the client to do.
///
/// A keystroke produces edits *or* a submit *or* an unfocus, never a mix: a
/// submit is `Enter` in normal mode, which the vi engine never sees.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ComposeAction {
    /// Edits to mirror onto the draft block, in order, through `edit_input`.
    pub ops: Vec<EditOp>,
    /// `Enter` in normal mode — submit the draft as a chat turn.
    pub submit: bool,
    /// `Esc Esc` in normal mode — compose releases the keyboard.
    pub unfocus: bool,
}

impl ComposeAction {
    /// Whether this keystroke asked for nothing at all.
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty() && !self.submit && !self.unfocus
    }
}

/// The compose surface.
pub struct Compose {
    editor: EditorCore,
    focused: bool,
    /// Consecutive `Esc` presses, for the dismiss gesture. Saturates at 2 and
    /// only the dismiss consumes it, so an `Esc Esc` that lands outside normal
    /// mode stays armed for the next tap (`docs/input.md`, "Escape").
    esc_taps: u8,
    last_esc: Option<Instant>,
    /// The highest context version this client's own `edit_input` calls have
    /// acknowledged. A mirror older than this does not yet carry our
    /// keystrokes, and reconciling against it would delete them.
    acked: u64,
}

impl Default for Compose {
    fn default() -> Self {
        Self::new()
    }
}

impl Compose {
    /// A fresh draft, in insert mode — the resting state the conversation
    /// figure draws (`-- INSERT --` at the right of the `❯` line).
    pub fn new() -> Self {
        Self::over("")
    }

    /// A draft already holding `text`, in insert mode with the cursor at the
    /// end. Used at startup, where the kernel may already hold a draft this
    /// or another client left behind.
    pub fn over(text: &str) -> Self {
        let mut editor = EditorCore::new(text);
        // `A` rather than `i`: land the cursor past the last character, which
        // is where someone resuming a draft expects to type.
        editor.apply_keys("A");
        Self {
            editor,
            focused: true,
            esc_taps: 0,
            last_esc: None,
            acked: 0,
        }
    }

    /// The draft's text.
    pub fn text(&self) -> String {
        self.editor.text()
    }

    /// The cursor's char offset into the draft.
    pub fn cursor(&mut self) -> usize {
        self.editor.cursor()
    }

    /// Whether compose holds the keyboard. `Esc Esc` in normal mode releases
    /// it; `i`, `a` or `o` take it back.
    pub fn focused(&self) -> bool {
        self.focused
    }

    /// The mode banner drawn at the right of the `❯` line: `-- INSERT --`,
    /// `-- VISUAL --`, and nothing in normal mode. An unfocused compose shows
    /// how to get back rather than a mode it is not in.
    pub fn mode_banner(&self) -> String {
        if !self.focused {
            return "i to type".to_string();
        }
        self.editor.mode().unwrap_or_default()
    }

    /// Record the context version an `edit_input` acknowledged.
    pub fn record_ack(&mut self, version: u64) {
        self.acked = self.acked.max(version);
    }

    /// Reconcile the draft against the kernel block a sibling just edited.
    ///
    /// `version` is the mirror's context version, the same number `edit_input`
    /// acknowledges. A mirror older than our last ack has not seen our own
    /// keystrokes yet, so applying it would delete them — that is the echo
    /// race, and refusing early is what closes it. Returns whether the buffer
    /// moved.
    pub fn reconcile(&mut self, kernel_text: &str, version: u64) -> bool {
        if version < self.acked {
            return false;
        }
        self.editor.apply_remote_text(kernel_text)
    }

    /// Start over on an empty draft — what `submit_input` leaves behind, since
    /// the kernel snapshots the draft into a block and clears it.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Interpret one keystroke.
    pub fn press(&mut self, key: KeyEvent, now: Instant) -> ComposeAction {
        let normal = self.editor.mode().is_none();

        if !self.focused {
            // Unfocused, compose answers only the keys that mean "start
            // typing"; everything else is left for another surface to claim.
            if matches!(key.code, KeyCode::Char('i' | 'a' | 'o' | 'I' | 'A' | 'O'))
                && !key.modifiers.contains(KeyModifiers::CONTROL)
            {
                self.focused = true;
            } else {
                return ComposeAction::default();
            }
        }

        if key.code == KeyCode::Esc {
            if self.tap_escape(now) && normal {
                self.esc_taps = 0;
                self.last_esc = None;
                self.focused = false;
                return ComposeAction {
                    unfocus: true,
                    ..ComposeAction::default()
                };
            }
        } else {
            // `Esc x Esc` is not a double-tap.
            self.esc_taps = 0;
            self.last_esc = None;
        }

        if key.code == KeyCode::Enter && normal {
            return ComposeAction {
                submit: true,
                ..ComposeAction::default()
            };
        }

        ComposeAction {
            ops: self.editor.apply_key_event(key),
            ..ComposeAction::default()
        }
    }

    /// Bump the consecutive-`Esc` count and report whether it has reached the
    /// dismiss threshold. Saturates, so it stays armed until a press lands in
    /// normal mode.
    fn tap_escape(&mut self, now: Instant) -> bool {
        let consecutive = self
            .last_esc
            .is_some_and(|prev| now.duration_since(prev) <= DOUBLE_TAP);
        self.esc_taps = if consecutive {
            self.esc_taps.saturating_add(1).min(2)
        } else {
            1
        };
        self.last_esc = Some(now);
        self.esc_taps >= 2
    }
}

/// The input region of the live viewport: the shell prompt when the shell
/// surface is toggled on, the `❯` compose line otherwise.
///
/// Multi-line drafts grow this region — the caller gives the transcript
/// whatever rows are left (`crate::render::live_lines`).
pub fn input_lines(app: &App, width: u16, palette: &Palette) -> Vec<Line<'static>> {
    if app.shell.active {
        return crate::shell::prompt_lines(app, width, palette);
    }
    compose_lines(&app.compose, width, palette)
}

/// The `❯` line, one row per draft line, with the mode banner right-aligned
/// on the first row.
pub fn compose_lines(compose: &Compose, width: u16, palette: &Palette) -> Vec<Line<'static>> {
    let text = compose.text();
    let banner = compose.mode_banner();
    let mut out = Vec::new();
    for (n, body) in text.split('\n').enumerate() {
        // Continuation rows indent under the prompt so the draft reads as one
        // block of text rather than restarting at column zero.
        let lead = if n == 0 {
            PROMPT.to_string()
        } else {
            " ".repeat(PROMPT.width())
        };
        let mut spans = vec![
            Span::styled(lead.clone(), palette.status()),
            Span::styled(body.to_string(), palette.compose()),
        ];
        if n == 0 && !banner.is_empty() {
            let used = lead.width() + body.width() + banner.width();
            let pad = usize::from(width).saturating_sub(used).max(1);
            spans.push(Span::styled(" ".repeat(pad), palette.compose()));
            spans.push(Span::styled(banner.clone(), palette.divider()));
        }
        out.push(Line::from(spans));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn typed(compose: &mut Compose, text: &str, now: Instant) -> Vec<EditOp> {
        let mut ops = Vec::new();
        for c in text.chars() {
            ops.extend(compose.press(press(KeyCode::Char(c)), now).ops);
        }
        ops
    }

    #[test]
    fn a_fresh_draft_opens_in_insert_mode() {
        let compose = Compose::new();
        assert_eq!(compose.mode_banner(), "-- INSERT --");
        assert_eq!(compose.text(), "");
    }

    /// Every keystroke is an `edit_input`, char-indexed the way the kernel
    /// addresses block text.
    #[test]
    fn typing_produces_one_char_indexed_edit_per_keystroke() {
        let mut compose = Compose::new();
        let ops = typed(&mut compose, "hi", Instant::now());
        assert_eq!(
            ops,
            vec![
                EditOp { offset: 0, insert: "h".into(), delete: 0 },
                EditOp { offset: 1, insert: "i".into(), delete: 0 },
            ]
        );
        assert_eq!(compose.text(), "hi");
    }

    /// Offsets are char offsets, not byte offsets — the bug that eats a draft
    /// the moment anyone types non-ASCII.
    #[test]
    fn an_edit_after_a_multibyte_char_is_addressed_in_chars() {
        let mut compose = Compose::new();
        let ops = typed(&mut compose, "café!", Instant::now());
        assert_eq!(ops.last().expect("an op").offset, 4);
    }

    #[test]
    fn vi_motions_and_operators_reach_the_draft() {
        let now = Instant::now();
        let mut compose = Compose::new();
        typed(&mut compose, "one two", now);
        compose.press(press(KeyCode::Esc), now);
        assert_eq!(compose.mode_banner(), "", "Esc lands in normal mode");
        // `db` deletes back a word.
        let ops = typed(&mut compose, "db", now);
        assert_eq!(compose.text(), "one o");
        assert_eq!(ops, vec![EditOp { offset: 4, insert: String::new(), delete: 2 }]);
    }

    /// A vim-notation string cannot carry a literal `<`; a key event can.
    #[test]
    fn a_literal_less_than_reaches_the_draft() {
        let mut compose = Compose::new();
        typed(&mut compose, "a < b", Instant::now());
        assert_eq!(compose.text(), "a < b");
    }

    #[test]
    fn enter_in_normal_mode_submits() {
        let now = Instant::now();
        let mut compose = Compose::new();
        typed(&mut compose, "and getattr?", now);
        assert!(!compose.press(press(KeyCode::Enter), now).submit, "insert mode types");
        compose.press(press(KeyCode::Esc), now);
        assert!(compose.press(press(KeyCode::Enter), now).submit);
    }

    /// Enter in insert mode is a newline: that is how a multi-line draft is
    /// written, and the region grows to hold it.
    #[test]
    fn enter_in_insert_mode_grows_the_draft() {
        let now = Instant::now();
        let mut compose = Compose::new();
        typed(&mut compose, "one", now);
        let action = compose.press(press(KeyCode::Enter), now);
        assert!(!action.submit);
        typed(&mut compose, "two", now);
        assert_eq!(compose.text(), "one\ntwo");
        let lines = compose_lines(&compose, 40, &Palette::builtin());
        assert_eq!(lines.len(), 2, "the region grew a row");
    }

    #[test]
    fn esc_esc_in_normal_mode_clears_focus() {
        let now = Instant::now();
        let mut compose = Compose::new();
        typed(&mut compose, "hi", now);
        // First Esc: insert → normal. Second: the dismiss.
        assert!(!compose.press(press(KeyCode::Esc), now).unfocus);
        let action = compose.press(press(KeyCode::Esc), now + Duration::from_millis(200));
        assert!(action.unfocus);
        assert!(!compose.focused());
        assert_eq!(compose.text(), "hi", "unfocus never discards the draft");
    }

    #[test]
    fn two_escapes_outside_the_window_do_not_clear_focus() {
        let now = Instant::now();
        let mut compose = Compose::new();
        compose.press(press(KeyCode::Esc), now);
        let late = compose.press(press(KeyCode::Esc), now + Duration::from_millis(900));
        assert!(!late.unfocus);
        assert!(compose.focused());
    }

    #[test]
    fn a_key_between_escapes_breaks_the_gesture() {
        let now = Instant::now();
        let mut compose = Compose::new();
        compose.press(press(KeyCode::Esc), now);
        typed(&mut compose, "x", now);
        assert!(!compose.press(press(KeyCode::Esc), now).unfocus);
    }

    #[test]
    fn an_unfocused_compose_ignores_typing_until_i_takes_the_keyboard_back() {
        let now = Instant::now();
        let mut compose = Compose::new();
        compose.press(press(KeyCode::Esc), now);
        compose.press(press(KeyCode::Esc), now);
        assert!(!compose.focused());
        assert_eq!(compose.mode_banner(), "i to type");

        assert!(compose.press(press(KeyCode::Char('x')), now).is_empty());
        assert_eq!(compose.text(), "");

        compose.press(press(KeyCode::Char('i')), now);
        assert!(compose.focused());
        typed(&mut compose, "back", now);
        assert_eq!(compose.text(), "back");
    }

    /// The draft is a shared block: a sibling's typing shows.
    #[test]
    fn a_siblings_edit_reaches_the_draft() {
        let mut compose = Compose::new();
        compose.record_ack(4);
        assert!(compose.reconcile("a sibling typed this", 5));
        assert_eq!(compose.text(), "a sibling typed this");
    }

    /// The echo race: our own `edit_input` comes back through the feed one
    /// keystroke behind. A mirror older than our last ack is stale, and
    /// applying it would delete what we just typed.
    #[test]
    fn a_mirror_behind_our_own_acks_never_rewinds_the_draft() {
        let now = Instant::now();
        let mut compose = Compose::new();
        typed(&mut compose, "ab", now);
        compose.record_ack(9);
        assert!(!compose.reconcile("a", 8), "a stale mirror is refused");
        assert_eq!(compose.text(), "ab");
    }

    #[test]
    fn our_own_echo_is_not_a_change() {
        let mut compose = Compose::new();
        typed(&mut compose, "ab", Instant::now());
        compose.record_ack(3);
        assert!(!compose.reconcile("ab", 3), "identical text is no change at all");
    }

    #[test]
    fn a_submitted_draft_resets_to_an_empty_insert_mode_line() {
        let mut compose = Compose::new();
        typed(&mut compose, "sent", Instant::now());
        compose.reset();
        assert_eq!(compose.text(), "");
        assert_eq!(compose.mode_banner(), "-- INSERT --");
    }

    #[test]
    fn resuming_a_draft_puts_the_cursor_past_its_last_char() {
        let mut compose = Compose::over("half a thought");
        assert_eq!(compose.cursor(), 14);
        typed(&mut compose, "!", Instant::now());
        assert_eq!(compose.text(), "half a thought!");
    }

    /// The figure: `❯ and getattr? _` with `-- INSERT --` at the right.
    #[test]
    fn the_compose_line_carries_the_mode_banner_at_the_right() {
        let mut compose = Compose::new();
        typed(&mut compose, "and getattr?", Instant::now());
        let lines = compose_lines(&compose, 40, &Palette::builtin());
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.starts_with("❯ and getattr?"), "got {text:?}");
        assert!(text.ends_with("-- INSERT --"), "got {text:?}");
        assert_eq!(text.width(), 40, "the banner sits at the right edge");
    }

    /// Nothing you would paste is inside a box.
    #[test]
    fn the_compose_region_carries_no_border_glyphs() {
        let mut compose = Compose::new();
        typed(&mut compose, "cargo test -p kaijutsu-tui", Instant::now());
        for line in compose_lines(&compose, 40, &Palette::builtin()) {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            for glyph in ['│', '╭', '╮', '╰', '╯'] {
                assert!(!text.contains(glyph), "{text:?} carries {glyph}");
            }
        }
    }
}
