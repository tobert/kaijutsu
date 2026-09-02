//! The diff screen — the alternate screen's other occupant.
//!
//! **Nothing on the wire opens a diff view.** `kj diff` authors a block whose
//! `ContentType` is `Diff`, and a client decides for itself to open on it; the
//! Bevy app does that with `v` on a focused block. The TUI's gesture is
//! `Ctrl+A v`, which opens on the newest openable diff block in the current
//! context, and `--diff <a> [b]` runs `kj diff` at startup and opens on its
//! output.
//!
//! Two contracts from `kaijutsu-diff` shape this module, and both are the
//! crate's, not this renderer's:
//!
//! - **Freeze on open.** The model is parsed once and held for the screen's
//!   whole life. Hunk indices and the scroll position are positions in that
//!   model, and swapping it out would move the reader's place without saying
//!   so.
//! - **A declared diff that does not parse is a visible error**, never an
//!   empty viewer. Content and content type are independent fields, so a block
//!   can legitimately declare itself a diff and hold text that will not parse.

use kaijutsu_diff::model::{DiffModel, LineKind};
use kaijutsu_diff::{DiffOptions, parse_with};
use kaijutsu_types::ContentType;
use ratatui::text::{Line, Span};

use crate::present::Palette;

/// One frozen diff, and where the viewport sits over it.
pub struct DiffScreen {
    /// What the header names — a path, or the `kj diff` argv that produced it.
    pub title: String,
    /// The frozen render, built once at open.
    lines: Vec<Line<'static>>,
    /// First rendered row on screen.
    top: usize,
    /// Body height of the last frame drawn. A page step and the bottom stop
    /// are measured in it, and the key path has no terminal to ask.
    body_h: usize,
}

impl DiffScreen {
    /// Freeze `model` into rendered lines under `title`.
    pub fn new(title: impl Into<String>, model: &DiffModel, palette: &Palette) -> Self {
        Self {
            title: title.into(),
            lines: diff_lines(model, palette),
            top: 0,
            body_h: DEFAULT_BODY_LINES,
        }
    }

    /// A declared diff that would not parse. The screen still opens, and it
    /// says what went wrong over the text it could not read.
    pub fn unparsed(title: impl Into<String>, error: &str, text: &str, palette: &Palette) -> Self {
        let mut lines = vec![
            Line::from(Span::styled(
                format!("this block declares itself a diff and does not parse: {error}"),
                palette.alarm(),
            )),
            Line::from(String::new()),
        ];
        lines.extend(
            text.lines()
                .map(|l| Line::from(Span::styled(l.to_string(), palette.diff_context()))),
        );
        Self {
            title: title.into(),
            lines,
            top: 0,
            body_h: DEFAULT_BODY_LINES,
        }
    }

    /// Total rendered rows, for the header's position figure.
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

    /// Move the viewport by `delta` rows, stopping at both ends.
    pub fn scroll(&mut self, delta: isize, body_h: usize) {
        let max = self.lines.len().saturating_sub(body_h);
        let next = self.top as isize + delta;
        self.top = next.clamp(0, max as isize) as usize;
    }

    pub fn scroll_to_top(&mut self) {
        self.top = 0;
    }

    pub fn scroll_to_bottom(&mut self, body_h: usize) {
        self.top = self.lines.len().saturating_sub(body_h);
    }

    /// The rows to draw into a screen `height` tall: a header, the body, and a
    /// key line — the same "every grown view renders its own keys" rule the
    /// picker and the ledger follow (`docs/tui.md`, "Keys").
    pub fn frame(&mut self, height: u16, palette: &Palette) -> Vec<Line<'static>> {
        let body_h = height.saturating_sub(CHROME_LINES).max(1) as usize;
        self.body_h = body_h;
        // A shrunken screen can leave the window past the end.
        self.top = self.top.min(self.lines.len().saturating_sub(body_h));
        let mut out = Vec::with_capacity(height as usize);
        out.push(Line::from(vec![
            Span::styled(self.title.clone(), palette.diff_header()),
            Span::styled(
                format!(
                    "   {}-{} of {}",
                    (self.top + 1).min(self.lines.len().max(1)),
                    (self.top + body_h).min(self.lines.len()),
                    self.lines.len()
                ),
                palette.status(),
            ),
        ]));
        for i in 0..body_h {
            out.push(match self.lines.get(self.top + i) {
                Some(line) => line.clone(),
                None => Line::from(String::new()),
            });
        }
        out.push(Line::from(Span::styled(
            "j/k scroll  Ctrl+D/Ctrl+U page  g/G ends  q close".to_string(),
            palette.status(),
        )));
        out
    }
}

/// Rows the diff screen reserves around the body: the header and the key line.
const CHROME_LINES: u16 = 2;

/// Body height assumed until the first frame is drawn, so a key that arrives
/// before one still steps a sane amount.
const DEFAULT_BODY_LINES: usize = 20;

/// What a key did to the diff screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKey {
    /// `q` / `Esc` — give the inline viewport back.
    Close,
    /// The viewport moved.
    Scrolled,
    /// Nothing this screen answers.
    Ignored,
}

/// Interpret one key against the screen. `body_h` is the drawn body height,
/// which is what a page step and the bottom stop are measured in.
pub fn handle_key(
    screen: &mut DiffScreen,
    key: &crossterm::event::KeyEvent,
    body_h: usize,
) -> DiffKey {
    use crossterm::event::{KeyCode, KeyModifiers};
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let page = (body_h / 2).max(1) as isize;
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => DiffKey::Close,
        KeyCode::Char('j') | KeyCode::Down => {
            screen.scroll(1, body_h);
            DiffKey::Scrolled
        }
        KeyCode::Char('k') | KeyCode::Up => {
            screen.scroll(-1, body_h);
            DiffKey::Scrolled
        }
        KeyCode::Char('d') if ctrl => {
            screen.scroll(page, body_h);
            DiffKey::Scrolled
        }
        KeyCode::Char('u') if ctrl => {
            screen.scroll(-page, body_h);
            DiffKey::Scrolled
        }
        KeyCode::PageDown => {
            screen.scroll(body_h as isize, body_h);
            DiffKey::Scrolled
        }
        KeyCode::PageUp => {
            screen.scroll(-(body_h as isize), body_h);
            DiffKey::Scrolled
        }
        KeyCode::Char('g') | KeyCode::Home => {
            screen.scroll_to_top();
            DiffKey::Scrolled
        }
        KeyCode::Char('G') | KeyCode::End => {
            screen.scroll_to_bottom(body_h);
            DiffKey::Scrolled
        }
        _ => DiffKey::Ignored,
    }
}

/// Render a whole model: a header per file, a `@@` line per hunk, and one row
/// per diff line with its band color and its word spans lifted out of it.
pub fn diff_lines(model: &DiffModel, palette: &Palette) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    if let Some(truncation) = model.truncated {
        // A truncated model is not a patch, and must never look like one.
        out.push(Line::from(Span::styled(
            format!(
                "truncated: {} files, {} hunks, {} lines omitted",
                truncation.omitted_files, truncation.omitted_hunks, truncation.omitted_lines
            ),
            palette.warning(),
        )));
    }
    for file in &model.files {
        let heading = if file.old_path == file.new_path {
            file.new_path.clone()
        } else {
            format!("{} → {}", file.old_path, file.new_path)
        };
        out.push(Line::from(Span::styled(
            format!("{heading}  ({:?})", file.change),
            palette.diff_header(),
        )));
        for hunk in &file.hunks {
            let section = hunk
                .section
                .as_ref()
                .map(|s| format!(" {s}"))
                .unwrap_or_default();
            out.push(Line::from(Span::styled(
                format!(
                    "@@ -{},{} +{},{} @@{section}",
                    hunk.old_start,
                    hunk.old_count(),
                    hunk.new_start,
                    hunk.new_count()
                ),
                palette.diff_hunk(),
            )));
            for line in &hunk.lines {
                out.push(diff_line(
                    line.prefix(),
                    &line.text,
                    &line.words,
                    line.kind,
                    palette,
                ));
                if line.no_newline {
                    out.push(Line::from(Span::styled(
                        "\\ No newline at end of file".to_string(),
                        palette.diff_context(),
                    )));
                }
            }
        }
    }
    out
}

/// One diff row: the band style over the whole line, with the changed word
/// spans lifted onto the word style.
///
/// Spans index **bytes** and are sorted and non-overlapping, which is
/// `kaijutsu-diff`'s contract — a span that falls outside the text or out of
/// order would be a defect in that crate, so the slicing here clamps rather
/// than panicking and the row still renders.
fn diff_line(
    prefix: char,
    text: &str,
    words: &[kaijutsu_diff::model::WordSpan],
    kind: LineKind,
    palette: &Palette,
) -> Line<'static> {
    let band = match kind {
        LineKind::Insert => palette.diff_insert(),
        LineKind::Delete => palette.diff_delete(),
        LineKind::Context => palette.diff_context(),
    };
    let word = match kind {
        LineKind::Insert => palette.diff_word_insert(),
        LineKind::Delete => palette.diff_word_delete(),
        LineKind::Context => band,
    };
    let mut spans = vec![Span::styled(prefix.to_string(), band)];
    let mut at = 0usize;
    for span in words {
        let (start, end) = (span.start.min(text.len()), span.end.min(text.len()));
        if start < at || start >= end || !text.is_char_boundary(start) || !text.is_char_boundary(end)
        {
            continue;
        }
        if start > at {
            spans.push(Span::styled(text[at..start].to_string(), band));
        }
        spans.push(Span::styled(text[start..end].to_string(), word));
        at = end;
    }
    if at < text.len() {
        spans.push(Span::styled(text[at..].to_string(), band));
    }
    Line::from(spans)
}

/// How a block earns the diff viewer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenAs {
    /// `ContentType::Diff` — authoritative. It opens even when it will not
    /// parse; the block asked to be a diff and is wrong about it.
    Declared,
    /// `ContentType::Plain` whose text really is a diff, with the fence (if
    /// any) already stripped. Sniffing may enrich, never accuse.
    Sniffed { inner: String },
}

/// Does this block's content mean the viewer should open on it?
///
/// The same two mechanisms and the same two failure policies the app's
/// `openable_diff` carries, so the two clients never disagree about what is a
/// diff.
pub fn openable_diff(content_type: ContentType, text: &str) -> Option<OpenAs> {
    match content_type {
        ContentType::Diff => Some(OpenAs::Declared),
        ContentType::Plain => {
            let inner = leading_fenced_block(text, "diff").unwrap_or(text);
            let model = parse_with(inner, &DiffOptions::default()).ok()?;
            // `parse` accepts empty input; an empty parse is not a diff.
            (!model.files.is_empty()).then(|| OpenAs::Sniffed {
                inner: inner.to_string(),
            })
        }
        _ => None,
    }
}

/// The body of a ```` ```<lang> ```` fence that **leads** the text, or `None`.
///
/// The fence must lead — the same rule the app's sniff uses, so a diff quoted
/// halfway down a paragraph of prose is prose, not a diff block.
fn leading_fenced_block<'a>(text: &'a str, lang: &str) -> Option<&'a str> {
    let rest = text.trim_start();
    let rest = rest.strip_prefix("```")?;
    let (tag, body) = rest.split_once('\n')?;
    if tag.trim() != lang {
        return None;
    }
    match body.rfind("```") {
        Some(end) => Some(&body[..end]),
        None => Some(body),
    }
}

/// Parse the text a block or a `kj diff` run produced, honoring the declared /
/// sniffed split.
pub fn parse_open(how: &OpenAs, text: &str) -> Result<DiffModel, kaijutsu_diff::DiffError> {
    let source = match how {
        OpenAs::Declared => text,
        OpenAs::Sniffed { inner } => inner.as_str(),
    };
    parse_with(source, &DiffOptions::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_diff::{DiffOptions, FileSpec, diff_file};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::widgets::Paragraph;

    fn model() -> DiffModel {
        let file = diff_file(
            &FileSpec::modified("greet.rs", "fn main() {}\n", "fn main() { greet(); }\n"),
            &DiffOptions::default(),
        )
        .expect("diff");
        DiffModel::new(vec![file])
    }

    fn rows(lines: Vec<Line<'static>>, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|f| f.render_widget(Paragraph::new(lines), f.area()))
            .expect("draw");
        let buf = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn a_model_renders_its_file_header_hunk_and_bands() {
        let palette = Palette::builtin();
        let out = rows(diff_lines(&model(), &palette), 60, 6);
        assert!(out[0].starts_with("greet.rs"), "{:?}", out[0]);
        assert!(out[1].starts_with("@@ -1,1 +1,1 @@"), "{:?}", out[1]);
        assert_eq!(out[2], "-fn main() {}");
        assert_eq!(out[3], "+fn main() { greet(); }");
    }

    /// The `+`/`-` band is the line's style and the changed words carry their
    /// own — a reader must be able to see *what* in the line moved.
    #[test]
    fn the_word_spans_carry_a_style_of_their_own() {
        let palette = Palette::builtin();
        let lines = diff_lines(&model(), &palette);
        let insert = lines
            .iter()
            .find(|l| l.spans.first().is_some_and(|s| s.content == "+"))
            .expect("an insert row");
        let styles: std::collections::HashSet<_> = insert.spans.iter().map(|s| s.style).collect();
        assert!(
            styles.len() > 1,
            "the insert row should not be one flat style: {insert:?}"
        );
        assert!(styles.contains(&palette.diff_word_insert()));
    }

    #[test]
    fn a_truncated_model_says_so_before_its_first_file() {
        let palette = Palette::builtin();
        let mut m = model();
        m.truncated = Some(kaijutsu_diff::model::Truncation {
            omitted_files: 2,
            omitted_hunks: 3,
            omitted_lines: 40,
        });
        let out = rows(diff_lines(&m, &palette), 70, 3);
        assert!(out[0].starts_with("truncated: 2 files"), "{:?}", out[0]);
    }

    #[test]
    fn the_screen_frames_a_header_a_body_and_a_key_line() {
        let palette = Palette::builtin();
        let mut screen = DiffScreen::new("kj diff greet.rs", &model(), &palette);
        let out = rows(screen.frame(6, &palette), 60, 6);
        assert!(out[0].starts_with("kj diff greet.rs"), "{:?}", out[0]);
        assert!(out[5].starts_with("j/k scroll"), "{:?}", out[5]);
    }

    #[test]
    fn scrolling_stops_at_both_ends() {
        let palette = Palette::builtin();
        let mut screen = DiffScreen::new("t", &model(), &palette);
        let body = 2;
        screen.scroll(-5, body);
        assert_eq!(screen.top, 0);
        screen.scroll(500, body);
        assert_eq!(screen.top, screen.len() - body);
    }

    #[test]
    fn q_and_esc_close_the_screen() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let palette = Palette::builtin();
        let mut screen = DiffScreen::new("t", &model(), &palette);
        for code in [KeyCode::Char('q'), KeyCode::Esc] {
            assert_eq!(
                handle_key(&mut screen, &KeyEvent::new(code, KeyModifiers::NONE), 10),
                DiffKey::Close
            );
        }
        assert_eq!(
            handle_key(
                &mut screen,
                &KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
                10
            ),
            DiffKey::Scrolled
        );
    }

    // ── what opens ──────────────────────────────────────────────────────────

    #[test]
    fn a_declared_diff_opens_even_when_it_does_not_parse() {
        assert_eq!(
            openable_diff(ContentType::Diff, "not a diff at all\n"),
            Some(OpenAs::Declared)
        );
    }

    #[test]
    fn plain_text_opens_only_when_it_really_is_a_diff() {
        assert_eq!(openable_diff(ContentType::Plain, "hello\nworld\n"), None);
        assert_eq!(openable_diff(ContentType::Plain, ""), None);
    }

    #[test]
    fn a_leading_diff_fence_is_stripped_before_the_sniff() {
        let text = "```diff\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n```\n";
        let Some(OpenAs::Sniffed { inner }) = openable_diff(ContentType::Plain, text) else {
            panic!("a fenced diff should sniff");
        };
        assert!(inner.starts_with("--- a/x"), "{inner:?}");
    }

    /// The fence must lead the block, so prose that quotes a diff halfway down
    /// is prose.
    #[test]
    fn a_fence_that_does_not_lead_is_not_stripped() {
        assert_eq!(leading_fenced_block("words\n```diff\n-a\n+b\n```", "diff"), None);
    }

    #[test]
    fn an_unparsed_declared_diff_is_a_visible_error_not_an_empty_viewer() {
        let palette = Palette::builtin();
        let mut screen = DiffScreen::unparsed("bad.diff", "expected file header", "junk\n", &palette);
        let out = rows(screen.frame(5, &palette), 70, 5);
        assert!(out[1].contains("does not parse"), "{:?}", out[1]);
        assert!(out[3].contains("junk"), "{:?}", out[3]);
    }
}
