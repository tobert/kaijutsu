//! Inline diff previews — the collapsed conversation view of a `Diff` block.
//!
//! `docs/diff.md` Decision 5: a diff block reads in the conversation as a
//! **diffstat header plus the first N lines**, collapsed by default; expanding
//! it into a full vi-motion surface is `Screen::Diff` (slice 5).
//!
//! Two halves, and the line between them matters. The **projection** half
//! (everything down to `cursor_byte`) is deliberately pure: text in, a
//! [`DiffPreview`] out, no Bevy, no Parley, no theme — it only decides what
//! the preview says and what each of its lines means. The **geometry** half
//! below it turns a laid-out preview into quads, so it works in layout space
//! and does touch Parley; it is here rather than in the renderer because both
//! the inline preview and the full viewer draw the same shapes, and two
//! copies of "what a diff looks like" would drift.
//!
//! Three contracts from the crate rustdoc (`kaijutsu-diff`) live here:
//!
//! - **Color is semantic** (Decision 8). The preview text carries no escape
//!   codes; a line's meaning travels as a [`DiffLineClass`] and the theme
//!   picks the color. That is what keeps a diff re-diffable at word level,
//!   foldable, and clean when a model hydrates it.
//! - **Truncate before layout.** `truncate_to_bytes(MAX_RENDER_BYTES)` runs
//!   before a single glyph is shaped: the parse ceiling is 16 MiB and Parley
//!   is on the main thread. Truncation is a *projection* — the block keeps the
//!   whole diff — and it cuts on whole-hunk boundaries, so a preview never
//!   reads as a complete-but-smaller patch.
//! - **A declared diff that does not parse is a visible error**, never an
//!   empty cell and never a crash. Content and content-type are separate LWW
//!   registers in the CRDT, so "declares itself a diff, holds something else"
//!   is a legitimate state, not a bug to assume away.

use kaijutsu_diff::{
    DiffModel, DiffStat, FileChange, LineKind,
    limits::{DEFAULT_PREVIEW_LINES, MAX_RENDER_BYTES},
    parse_with,
};
use kaijutsu_types::ContentType;

use crate::text::msdf::geometry::{GeometryVertex, stroke_line_quad};

/// Longest single preview line, in chars, before it is elided with `…`.
///
/// `truncate_to_bytes` bounds the *whole* preview but says nothing about the
/// shape of what survives: twenty lines of a minified bundle is a megabyte of
/// text inside the line budget, and Parley would wrap every byte of it. A
/// preview line past this width has stopped being readable anyway.
pub const MAX_PREVIEW_LINE_CHARS: usize = 500;

/// What one preview line *means*. The renderer turns this into a color and,
/// for the tinted kinds, a background band.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineClass {
    /// The diffstat header (`3 files, +42 −17`).
    Stat,
    /// A per-file path header.
    FileHeader,
    /// A `@@ … @@` hunk header.
    HunkHeader,
    /// An added line. Tinted.
    Insert,
    /// A removed line. Tinted.
    Delete,
    /// An unchanged context line.
    Context,
    /// An omission note — what the preview is not showing.
    Meta,
    /// The banner of a block that declares itself a diff and does not parse.
    /// Tinted.
    Error,
}

/// One logical line of a preview: where it lives in
/// [`DiffPreview::plain_text`] and what it means.
///
/// `start..end` is **contiguous with its neighbours** — `end` is the start of
/// the next line, so the newline separating them belongs to the earlier line
/// and every byte of `plain_text` maps to exactly one `PreviewLine`. That is
/// what lets both the brush lookup (which is given a shaping run's start byte)
/// and the band lookup (which is given a wrapped visual row's start byte)
/// answer with a plain containment test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewLine {
    /// Byte offset of the line's first byte in `plain_text`.
    pub start: usize,
    /// Byte offset one past the line's newline (= the next line's `start`).
    pub end: usize,
    /// Byte offset where the diff line's own text begins — one past `start`
    /// for a body line (skipping the ` `/`+`/`-` prefix, always one ASCII
    /// byte), equal to `start` for every other class.
    ///
    /// This is the translation slice 6 needs: `kaijutsu_diff::WordSpan`
    /// indexes **bytes of `DiffLine::text`**, so a word span becomes a range
    /// in `plain_text` by adding `text_start`. No char/grapheme conversion is
    /// involved anywhere on this path.
    pub text_start: usize,
    /// Semantic class.
    pub class: DiffLineClass,
}

/// A byte range of [`DiffPreview::plain_text`] that the word-level refinement
/// marked as *changed within its line* — the intra-line half of the diff.
///
/// Separate from [`PreviewLine`] rather than a field on it, for two reasons:
/// `PreviewLine` stays `Copy` (the band and class lookups index it per visual
/// row, per frame), and the renderer wants one flat, sorted, non-overlapping
/// list to turn into brush spans anyway.
///
/// Ranges are absolute in `plain_text`: [`kaijutsu_diff::WordSpan`] indexes
/// bytes of `DiffLine::text`, and the translation is `+ text_start`
/// ([`PreviewLine::text_start`]) — plus the elision clip, see
/// [`elision_cut`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WordHighlight {
    /// Inclusive start byte offset in `plain_text`.
    pub start: usize,
    /// Exclusive end byte offset in `plain_text`.
    pub end: usize,
    /// The class of the line the span belongs to — [`DiffLineClass::Insert`]
    /// or [`DiffLineClass::Delete`]. The renderer picks the emphasis color
    /// from it.
    pub class: DiffLineClass,
}

/// A parsed, budgeted, class-annotated inline preview of a diff block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffPreview {
    /// The exact text to lay out — stat header, then the previewed lines.
    /// Has no trailing newline (a trailing newline would cost a blank
    /// laid-out row and an extra line of block height).
    pub plain_text: String,
    /// One entry per line of `plain_text`, in order, contiguous.
    pub lines: Vec<PreviewLine>,
    /// Intra-line changed ranges, ascending and non-overlapping. Empty for an
    /// error preview and for any line the refinement had no counterpart for.
    pub words: Vec<WordHighlight>,
    /// The stat of the **whole** diff, not of the previewed slice — the stat
    /// describes the change, the body is what fits.
    pub stat: DiffStat,
    /// True when the content declared itself a diff and did not parse.
    pub is_error: bool,
}

impl DiffPreview {
    /// The class covering byte `offset`, if any.
    ///
    /// `None` only past the end of the text (Parley reports a trailing empty
    /// line for text that ends in a newline; we do not produce one, but a
    /// caller must not be able to panic on the question).
    pub fn class_at(&self, offset: usize) -> Option<DiffLineClass> {
        self.lines
            .iter()
            .find(|l| offset >= l.start && offset < l.end)
            .map(|l| l.class)
    }
}

/// Map a block's declared [`ContentType`] onto a diff tokenizer profile.
///
/// Deliberately **exhaustive**: `kaijutsu-diff` keeps its own `DiffProfile`
/// vocabulary precisely so MIME growth never reaches the algorithm crate, and
/// this is the edge where the two meet. A new `ContentType` variant must be a
/// compile error here — a wildcard would silently hand new content to the
/// default tokenizer, which is exactly the "quietly wrong" failure the
/// pre-merge review flagged.
///
/// Today only `Diff` (a declared diff block) and `Plain` (a sniffed
/// ` ```diff ` fence) actually arrive. The remaining arms are where per-content
/// profiles land once a diff block records *what it diffed*: markdown wants
/// the deferred `Paragraph` profile (prose rewraps, so line identity is weak)
/// and ABC wants `AbcBars` (hunks on measures, not text lines). Both are
/// documented-not-implemented in `kaijutsu_diff::profile`, so mapping them to
/// `LineWord` today is the honest answer, not a fallback.
pub fn diff_profile_for(content_type: ContentType) -> kaijutsu_diff::DiffProfile {
    use kaijutsu_diff::DiffProfile;
    match content_type {
        ContentType::Diff | ContentType::Plain => DiffProfile::LineWord,
        ContentType::Markdown => DiffProfile::LineWord,
        ContentType::Abc => DiffProfile::LineWord,
        ContentType::Svg | ContentType::Image => DiffProfile::LineWord,
    }
}

/// Parse `text` as a unified diff and build its inline preview.
///
/// Never fails and never returns an empty preview: unparseable content becomes
/// an error preview that names the problem and still shows the source.
pub fn build_diff_preview(text: &str, content_type: ContentType) -> DiffPreview {
    let options = kaijutsu_diff::DiffOptions {
        profile: diff_profile_for(content_type),
        ..Default::default()
    };
    match parse_with(text, &options) {
        Ok(model) => preview_from_model(&model),
        Err(e) => error_preview(text, &e.to_string()),
    }
}

/// Parse `text` as a diff, or report why not.
///
/// The sniffing path wants this: an undeclared ` ```diff ` fence that turns out
/// not to be a diff must fall through to the ordinary text/markdown rendering,
/// never hijack the block into a diff error banner.
pub fn try_build_diff_preview(text: &str, content_type: ContentType) -> Option<DiffPreview> {
    let options = kaijutsu_diff::DiffOptions {
        profile: diff_profile_for(content_type),
        ..Default::default()
    };
    let model = parse_with(text, &options).ok()?;
    // An empty parse is not a diff. `parse` accepts empty input as an empty
    // model, so without this every fenced block with nothing diff-shaped in it
    // would sniff as a (blank) diff.
    if model.files.is_empty() {
        return None;
    }
    Some(preview_from_model(&model))
}

/// Where display elision cut `text`, as a byte offset into it.
///
/// [`crate::text::truncate_chars`] keeps `max - 1` chars and appends `…`, so
/// this is the byte length of the part that *survived unchanged* — the point
/// past which a [`kaijutsu_diff::WordSpan`] no longer describes anything on
/// screen. A span past it would index into the ellipsis or off the end of the
/// line entirely (the failure the slice-4 post-ship review flagged), and one
/// straddling it is clipped here rather than drawn over the `…`.
fn elision_cut(text: &str, max_chars: usize) -> usize {
    if max_chars == 0 {
        return 0;
    }
    if text.chars().count() <= max_chars {
        return text.len();
    }
    text.char_indices()
        .nth(max_chars - 1)
        .map(|(i, _)| i)
        .unwrap_or(text.len())
}

/// Accumulates lines, their byte ranges, and their word spans as the preview
/// is written.
struct PreviewBuilder {
    text: String,
    lines: Vec<PreviewLine>,
    words: Vec<WordHighlight>,
}

impl PreviewBuilder {
    fn new() -> Self {
        Self {
            text: String::new(),
            lines: Vec::new(),
            words: Vec::new(),
        }
    }

    /// Append one line. `prefix_bytes` is how many leading ASCII bytes are the
    /// diff prefix rather than the line's own text (1 for a body line, 0
    /// otherwise) — see [`PreviewLine::text_start`].
    fn push(&mut self, class: DiffLineClass, prefix_bytes: usize, content: &str) {
        let start = self.text.len();
        self.text.push_str(content);
        self.text.push('\n');
        self.lines.push(PreviewLine {
            start,
            end: self.text.len(),
            text_start: start + prefix_bytes,
            class,
        });
    }

    /// Append a body line together with its word-level spans.
    ///
    /// `words` index bytes of the line's **own** text (`DiffLine::text`), and
    /// `cut` is how many of those bytes survived display elision — see
    /// [`elision_cut`]. Spans are sorted ascending, so the first one that
    /// starts past the cut ends the walk.
    fn push_words(
        &mut self,
        class: DiffLineClass,
        prefix_bytes: usize,
        content: &str,
        words: &[kaijutsu_diff::WordSpan],
        cut: usize,
    ) {
        self.push(class, prefix_bytes, content);
        let text_start = self.lines.last().expect("just pushed").text_start;
        for span in words {
            if span.start >= cut {
                break;
            }
            let end = span.end.min(cut);
            if end <= span.start {
                continue;
            }
            self.words.push(WordHighlight {
                start: text_start + span.start,
                end: text_start + end,
                class,
            });
        }
    }

    fn finish(mut self, stat: DiffStat, is_error: bool) -> DiffPreview {
        // Drop the trailing newline and shrink the last line's range with it,
        // so `end` stays exactly `plain_text.len()` and the contiguity
        // invariant survives.
        if self.text.ends_with('\n') {
            self.text.pop();
            if let Some(last) = self.lines.last_mut() {
                last.end = self.text.len();
            }
        }
        DiffPreview {
            plain_text: self.text,
            lines: self.lines,
            words: self.words,
            stat,
            is_error,
        }
    }
}

/// Build the preview body from a parsed model.
fn preview_from_model(model: &DiffModel) -> DiffPreview {
    // The stat describes the WHOLE diff; the body is what fits. Taken before
    // any projection, exactly as the kernel's hydration projection does.
    let stat = model.stat();

    // Budget in two steps, both before any layout happens:
    //   1. bytes — the render ceiling, because the parse ceiling is 16 MiB and
    //      Parley runs on the main thread;
    //   2. lines — the preview shape from Decision 5.
    // Both cut on whole-hunk boundaries and both record what they dropped.
    let bounded = kaijutsu_diff::truncate_to_bytes(model, MAX_RENDER_BYTES);
    let shown = kaijutsu_diff::truncate_to_lines(&bounded, DEFAULT_PREVIEW_LINES);

    let mut b = PreviewBuilder::new();

    let mut header = stat.to_string();
    if model.truncated.is_some() {
        // The block itself already arrived truncated (the producer spent a
        // budget). Say so — the stat below it is the stat of what we have,
        // which is not the stat of the change.
        header.push_str("  (partial diff)");
    }
    b.push(DiffLineClass::Stat, 0, &header);

    if model.files.is_empty() {
        b.push(DiffLineClass::Meta, 0, "(no file sections)");
        return b.finish(stat, false);
    }

    for file in &shown.files {
        b.push(DiffLineClass::FileHeader, 0, &file_header(file));
        for hunk in &file.hunks {
            b.push(DiffLineClass::HunkHeader, 0, &hunk_header(hunk));
            for line in &hunk.lines {
                let class = match line.kind {
                    LineKind::Insert => DiffLineClass::Insert,
                    LineKind::Delete => DiffLineClass::Delete,
                    LineKind::Context => DiffLineClass::Context,
                };
                let cut = elision_cut(&line.text, MAX_PREVIEW_LINE_CHARS);
                let text = crate::text::truncate_chars(&line.text, MAX_PREVIEW_LINE_CHARS);
                b.push_words(
                    class,
                    1,
                    &format!("{}{}", line.prefix(), text),
                    &line.words,
                    cut,
                );
            }
        }
    }

    if let Some(note) = omission_note(model, &shown) {
        b.push(DiffLineClass::Meta, 0, &note);
    }

    b.finish(stat, false)
}

/// The one-line header for a file section.
fn file_header(file: &kaijutsu_diff::FileDiff) -> String {
    match file.change {
        FileChange::Added => format!("{} (new file)", file.new_path),
        FileChange::Deleted => format!("{} (deleted)", file.old_path),
        FileChange::Renamed => format!("{} \u{2192} {}", file.old_path, file.new_path),
        FileChange::Modified => file.new_path.clone(),
    }
}

/// A `@@ … @@` header for the preview.
///
/// Close to canonical unified form but not a promise of it — preview text is
/// for reading, never for applying. Yank (slice 5) reads the frozen model and
/// re-`format`s it; it must never scrape these strings.
fn hunk_header(hunk: &kaijutsu_diff::Hunk) -> String {
    let mut s = format!(
        "@@ -{},{} +{},{} @@",
        hunk.old_start,
        hunk.old_count(),
        hunk.new_start,
        hunk.new_count()
    );
    if let Some(section) = &hunk.section {
        s.push(' ');
        s.push_str(section);
    }
    s
}

/// What the preview is not showing, phrased as a count the reader can trust.
///
/// Compares the shown projection against the model it came from rather than
/// reading `shown.truncated` directly, because that field also carries any
/// truncation the *producer* already applied — which the "(partial diff)" note
/// on the header already covers.
fn omission_note(model: &DiffModel, shown: &DiffModel) -> Option<String> {
    let hidden_lines = model.line_count().saturating_sub(shown.line_count());
    let hidden_files = model.files.len().saturating_sub(shown.files.len());
    if hidden_lines == 0 && hidden_files == 0 {
        return None;
    }
    let mut note = format!("\u{2026} {hidden_lines} more line");
    if hidden_lines != 1 {
        note.push('s');
    }
    if hidden_files > 0 {
        note.push_str(&format!(
            " in {hidden_files} more file{}",
            if hidden_files == 1 { "" } else { "s" }
        ));
    }
    Some(note)
}

/// The visible error treatment for a block that declares itself a diff and
/// holds something else.
///
/// Shows the failure *and* the source: an error banner alone would leave the
/// reader unable to see what the block actually contains, and the source alone
/// would leave them wondering why it isn't colored. The lines are classed
/// `Context` rather than guessed at — nothing here parsed, so claiming a line
/// is an insertion would be a fabrication.
fn error_preview(text: &str, reason: &str) -> DiffPreview {
    error_preview_with(text, reason, DEFAULT_PREVIEW_LINES)
}

/// [`error_preview`] with an explicit source-line budget — the full viewer
/// shows far more of the offending text than an inline row can.
fn error_preview_with(text: &str, reason: &str, max_lines: usize) -> DiffPreview {
    let mut b = PreviewBuilder::new();
    b.push(
        DiffLineClass::Error,
        0,
        &format!("[declared as a diff but does not parse] {reason}"),
    );

    let mut shown = 0usize;
    let total = text.lines().count();
    for line in text.lines().take(max_lines) {
        b.push(
            DiffLineClass::Context,
            0,
            &crate::text::truncate_chars(line, MAX_PREVIEW_LINE_CHARS),
        );
        shown += 1;
    }
    if total > shown {
        let hidden = total - shown;
        b.push(
            DiffLineClass::Meta,
            0,
            &format!(
                "\u{2026} {hidden} more line{}",
                if hidden == 1 { "" } else { "s" }
            ),
        );
    }

    b.finish(DiffStat::default(), true)
}

// ════════════════════════════════════════════════════════════════════════════
// The full viewer's projection
// ════════════════════════════════════════════════════════════════════════════

/// Longest single line the FULL viewer lays out before eliding it.
///
/// Wider than the inline preview's budget — the full screen is where you go to
/// actually read a line — but still bounded, for the same reason: one minified
/// bundle line inside the byte budget is a screenful of wrapped text and a
/// Parley stall. Elision is *display only*; yank reads `DiffCore`'s canonical
/// text, which is never elided.
pub const MAX_VIEW_LINE_CHARS: usize = 2_000;

/// What a [`kaijutsu_diff::RowKind`] means to the renderer.
///
/// Exhaustive on purpose, like [`diff_profile_for`]: a new row kind in
/// `kaijutsu-diff` must be a compile error here rather than silently painting
/// itself context-colored.
pub fn class_for_row(kind: kaijutsu_diff::RowKind) -> DiffLineClass {
    use kaijutsu_diff::RowKind;
    match kind {
        RowKind::Marker => DiffLineClass::Meta,
        RowKind::FileHeader => DiffLineClass::FileHeader,
        RowKind::HunkHeader => DiffLineClass::HunkHeader,
        RowKind::Body(LineKind::Insert) => DiffLineClass::Insert,
        RowKind::Body(LineKind::Delete) => DiffLineClass::Delete,
        RowKind::Body(LineKind::Context) => DiffLineClass::Context,
        RowKind::NoNewline => DiffLineClass::Meta,
    }
}

/// Project a frozen [`DiffCore`] into the classed, laid-out form the renderer
/// already knows how to draw.
///
/// The same [`DiffPreview`] shape the inline preview produces, so the full
/// screen reuses `build_diff_span_brushes` and [`build_diff_band_geometry`]
/// verbatim instead of growing a parallel rendering path. Two differences from
/// the inline preview, both deliberate:
///
/// - **No stat header.** The text is byte-for-byte `DiffCore::text()` — the
///   canonical diff — so a row index means the same thing to the renderer and
///   to the cursor. The stat is drawn in its own header strip instead.
/// - **No line budget.** The whole diff is shown; only absurd single lines are
///   elided ([`MAX_VIEW_LINE_CHARS`]). The byte budget was already spent when
///   the core was built.
///
/// **`rows` indexes `DiffCore::visible_rows`, not the canonical rows**, and
/// `preview.lines[i]` is the row `visible_rows()[rows.start + i]` names — one
/// for one, in order. Folding is a projection (`docs/diff.md` slice 6): a
/// folded hunk's body is simply not in that list, its header stays and picks
/// up a `⋯ N lines folded` indicator here. The cursor and the selection are
/// published in the same visible coordinates (`cursor_visible_row`,
/// `selection_visible_rows`), so everything the viewer addresses agrees.
/// Yank is unaffected either way: it reads the core's canonical text, which
/// folding never touches.
///
/// It takes a **window** rather than the whole diff because a diff bounded
/// only by `MAX_RENDER_BYTES` is up to a megabyte of text, and Parley-shaping
/// a megabyte to draw forty visible lines is a frame no GPU saves you from.
/// The viewer lays out a band of rows around the cursor and rebuilds it when
/// the cursor leaves; the caller owns `rows.start`.
pub fn view_rows(core: &kaijutsu_diff::DiffCore, rows: std::ops::Range<usize>) -> DiffPreview {
    let mut b = PreviewBuilder::new();
    let text = core.text();
    let visible = core.visible_rows();
    let end = rows.end.min(visible.len());
    let start = rows.start.min(end);

    for &index in &visible[start..end] {
        let row = &core.rows()[index];
        let raw = text[row.start..row.end].trim_end_matches('\n');
        let cut = elision_cut(raw, MAX_VIEW_LINE_CHARS);
        let mut shown = crate::text::truncate_chars(raw, MAX_VIEW_LINE_CHARS);
        // A folded hunk draws as its header alone, so the header has to say
        // what is missing — a fold with no indicator is a diff that quietly
        // lies about its own size. Appended after elision, so the indicator
        // survives an absurd header line.
        if let Some(fold) = core.fold_summary(index) {
            shown.push_str(&format!(
                "  \u{22ef} {} line{} folded",
                fold.hidden_rows,
                if fold.hidden_rows == 1 { "" } else { "s" },
            ));
        }
        // The prefix byte only exists if it survived elision (it always does —
        // the budget is thousands of chars — but derive it rather than assume).
        let prefix_bytes = (row.text_start - row.start).min(shown.len());
        // `cut` is measured on the row (prefix included); word spans index the
        // line's own text, so the cut moves with them.
        b.push_words(
            class_for_row(row.kind),
            prefix_bytes,
            &shown,
            row_words(core.model(), row),
            cut.saturating_sub(prefix_bytes),
        );
    }

    b.finish(core.model().stat(), false)
}

/// The word-level spans of the model line a row came from, or nothing.
///
/// A row's provenance (`file`/`hunk`/`line`) is the only link back to the
/// model: the canonical text carries no word information, because word spans
/// are *derived* — recomputed by `parse`, never serialized — which is what
/// keeps the format/parse roundtrip an identity.
fn row_words<'a>(
    model: &'a kaijutsu_diff::DiffModel,
    row: &kaijutsu_diff::DiffRow,
) -> &'a [kaijutsu_diff::WordSpan] {
    const NONE: &[kaijutsu_diff::WordSpan] = &[];
    // A `\ No newline` row points at the body line above it, so it has the
    // same provenance and none of the same text — asking the kind is what
    // keeps its marker text from being highlighted with that line's spans.
    if !matches!(row.kind, kaijutsu_diff::RowKind::Body(_)) {
        return NONE;
    }
    let (Some(fi), Some(hi), Some(li)) = (row.file, row.hunk, row.line) else {
        return NONE;
    };
    model
        .files
        .get(fi)
        .and_then(|f| f.hunks.get(hi))
        .and_then(|h| h.lines.get(li))
        .map(|l| l.words.as_slice())
        .unwrap_or(NONE)
}

/// The full viewer's error state: a block that declares itself a diff and
/// holds something that does not parse.
///
/// The crate contract is explicit that this is a *visible* state and never an
/// empty viewer. Shows the banner and a generous window on the source.
pub fn error_view(text: &str, reason: &str) -> DiffPreview {
    error_preview_with(text, reason, 500)
}

/// The byte offset in [`DiffPreview::plain_text`] of `column` on preview line
/// `line`, clamped to the **end** of that line's text.
///
/// `column` is counted in **chars** — modalkit's unit, since its buffer is a
/// char-indexed rope (`DiffCore::cursor_col`) — and everything downstream of
/// here is bytes, so this is the one conversion on the path. The clamp stops
/// at the line's text, never past its newline, so a range built from two of
/// these can never leak into the next line.
///
/// This is the **exclusive-end-friendly** form: use it for the end of a
/// selection, where "one past the last selected char" is exactly what is
/// wanted. For placing a cursor *on* a character, use [`cursor_byte`].
///
/// `None` only for a line index the preview does not have.
pub fn column_byte(preview: &DiffPreview, line: usize, column: usize) -> Option<usize> {
    let l = preview.lines.get(line)?;
    let text = preview.plain_text[l.start..l.end].trim_end_matches('\n');
    let offset = text
        .char_indices()
        .nth(column)
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    Some(l.start + offset)
}

/// Where to *draw* the cursor for `column` on preview line `line`: the byte
/// offset of a real character, never of the line's newline.
///
/// The distinction is not pedantry. `text.len()` is the offset of the
/// terminating `\n`, and Parley's `Cursor::from_byte_index(.., Downstream)`
/// resolves that onto the **following** visual row — so a goal column past the
/// end of a short line (vim's `$` then `j`) drew the cursor one line below
/// where the viewer thought it was. The core clamps the column too
/// (`DiffCore::cursor_col`); this is the other half, and it holds even if a
/// future caller hands over an unclamped column.
///
/// A line with no characters at all has no byte that is not its newline; it
/// returns the line start, and the caller draws a zero-width cursor there.
/// (The diff viewer has no such line — every row carries at least a prefix or
/// a header — but the error view's source lines can be empty.)
pub fn cursor_byte(preview: &DiffPreview, line: usize, column: usize) -> Option<usize> {
    let l = preview.lines.get(line)?;
    let text = preview.plain_text[l.start..l.end].trim_end_matches('\n');
    let offset = match text.char_indices().nth(column) {
        Some((i, _)) => i,
        None => text.char_indices().next_back().map_or(0, |(i, _)| i),
    };
    Some(l.start + offset)
}

// ════════════════════════════════════════════════════════════════════════════
// Background bands
// ════════════════════════════════════════════════════════════════════════════

/// One laid-out visual row, as Parley wrapped it.
///
/// A logical preview line that wraps produces several of these, all pointing
/// into the same [`PreviewLine`] — which is why the class lookup is by byte
/// offset rather than by index: a wrapped `+` line must be tinted across every
/// row it occupies, not just its first.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PreviewRow {
    /// Byte offset in `plain_text` where this visual row starts.
    pub text_offset: usize,
    /// Row top, block-local LOGICAL px.
    pub top: f32,
    /// Row bottom, block-local LOGICAL px.
    pub bottom: f32,
}

/// RGBA8 band colors, resolved from the theme by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffBandColors {
    /// Behind an added line.
    pub insert: [u8; 4],
    /// Behind a removed line.
    pub delete: [u8; 4],
    /// Behind the parse-error banner.
    pub error: [u8; 4],
}

/// Which band color a class gets, if any. Only the three tinted classes
/// produce geometry — headers and context lines are carried by text color
/// alone, which keeps the quad count proportional to *changed* lines.
fn band_color(class: DiffLineClass, colors: &DiffBandColors) -> Option<[u8; 4]> {
    match class {
        DiffLineClass::Insert => Some(colors.insert),
        DiffLineClass::Delete => Some(colors.delete),
        DiffLineClass::Error => Some(colors.error),
        DiffLineClass::Stat
        | DiffLineClass::FileHeader
        | DiffLineClass::HunkHeader
        | DiffLineClass::Context
        | DiffLineClass::Meta => None,
    }
}

/// Build the per-line background bands as flat-colored triangles.
///
/// Pure math in block-local LOGICAL pixels — the same space as
/// `PositionedGlyph::x/y`, converted to physical NDC by the render-world
/// vertex builder. Reuses [`stroke_line_quad`] rather than growing a second
/// rectangle primitive: a band *is* a butt-capped horizontal stroke whose
/// width is the row height.
pub fn build_diff_band_geometry(
    preview: &DiffPreview,
    rows: &[PreviewRow],
    width: f32,
    offset: (f32, f32),
    colors: &DiffBandColors,
) -> Vec<GeometryVertex> {
    if width <= 0.0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for row in rows {
        let height = row.bottom - row.top;
        if height <= 0.0 {
            continue;
        }
        let Some(class) = preview.class_at(row.text_offset) else {
            continue;
        };
        let Some(color) = band_color(class, colors) else {
            continue;
        };
        let cy = (row.top + row.bottom) * 0.5 + offset.1;
        out.extend_from_slice(&stroke_line_quad(
            offset.0 as f64,
            cy as f64,
            (offset.0 + width) as f64,
            cy as f64,
            height as f64,
            color,
        ));
    }
    out
}

/// Build the visual-line selection highlight as full-width bands.
///
/// Line bands are what the renderer can draw *today* — which is exactly why
/// the viewer's visual mode is line-wise only until the multi-rect
/// `BlockFxMaterial` extension lands (`docs/diff.md` slices 5/6). `rows` is the
/// same wrapped-visual-row list the diff bands use, so a selected line that
/// wraps is highlighted across every row it occupies.
///
/// `selected` is an inclusive range of **logical line indices** into
/// `preview.lines` — the viewer's row range, straight from
/// `DiffCore::selection_rows`.
pub fn build_selection_band_geometry(
    preview: &DiffPreview,
    rows: &[PreviewRow],
    selected: (usize, usize),
    width: f32,
    offset: (f32, f32),
    color: [u8; 4],
) -> Vec<GeometryVertex> {
    let (first, last) = selected;
    if width <= 0.0 || first > last || first >= preview.lines.len() {
        return Vec::new();
    }
    let last = last.min(preview.lines.len() - 1);
    let (start, end) = (preview.lines[first].start, preview.lines[last].end);

    let mut out = Vec::new();
    for row in rows {
        if row.text_offset < start || row.text_offset >= end {
            continue;
        }
        let height = row.bottom - row.top;
        if height <= 0.0 {
            continue;
        }
        let cy = (row.top + row.bottom) * 0.5 + offset.1;
        out.extend_from_slice(&stroke_line_quad(
            offset.0 as f64,
            cy as f64,
            (offset.0 + width) as f64,
            cy as f64,
            height as f64,
            color,
        ));
    }
    out
}

/// The byte range of [`DiffPreview::plain_text`] a **character-wise**
/// selection covers, given its extent in window-local line indices.
///
/// The one translation between `DiffCore`'s coordinates (row + char column)
/// and the byte range Parley's `Selection` wants. Two clamps, both deliberate:
///
/// - Rows outside the laid-out window are clipped to it. A selection can start
///   above the window or end below it; the part on screen is still the part
///   that gets drawn.
/// - `end_col` is **inclusive** ([`kaijutsu_diff::CharSpan`]), so the range
///   runs one column past it — and [`column_byte`] clamps that to the end of
///   the line's text rather than running into the newline. (Not
///   [`cursor_byte`]: that one clamps *onto* the last character, which as an
///   exclusive end would drop the last selected glyph.)
///
/// `None` when the span covers nothing the window laid out, or when it is
/// degenerate — a caller must not turn either into a rectangle.
pub fn char_selection_bytes(
    preview: &DiffPreview,
    span: kaijutsu_diff::CharSpan,
) -> Option<std::ops::Range<usize>> {
    if preview.lines.is_empty() || span.start_row >= preview.lines.len() {
        return None;
    }
    let last = preview.lines.len() - 1;
    let (end_row, end_col) = if span.end_row > last {
        // The selection runs off the bottom of the window: take the last row
        // whole rather than dropping it.
        (last, usize::MAX)
    } else {
        (span.end_row, span.end_col)
    };
    if end_row < span.start_row {
        return None;
    }
    let start = column_byte(preview, span.start_row, span.start_col)?;
    let end = column_byte(preview, end_row, end_col.saturating_add(1))?;
    (end > start).then_some(start..end)
}

/// Turn selection rectangles into flat-colored quads for `MsdfBlockGeometry`.
///
/// The diff viewer draws its selection **behind** the glyphs and on top of the
/// `+`/`-` bands, which is why it goes through geometry rather than the
/// material's shader composite (`shaders::selection` explains the split). The
/// rects arrive in Parley's layout-local space; `offset` moves them to
/// block-local logical pixels, the space every other geometry producer works
/// in.
pub fn selection_rect_geometry(
    rects: &[crate::shaders::SelectionRect],
    offset: (f32, f32),
    color: [u8; 4],
) -> Vec<GeometryVertex> {
    let mut out = Vec::with_capacity(rects.len() * 6);
    for rect in rects.iter().filter(|r| r.is_visible()) {
        out.extend_from_slice(&crate::text::msdf::geometry::rect_quad(
            (rect.x + offset.0) as f64,
            (rect.y + offset.1) as f64,
            rect.width as f64,
            rect.height as f64,
            color,
        ));
    }
    out
}

/// Where a byte range of laid-out text actually sits: one rect per visual row
/// it crosses.
///
/// Parley's `Selection::geometry` is the off-the-shelf answer `docs/vi.md`
/// names, and it is the *only* honest one — it walks clusters, so it is right
/// on a wrapped line and on a run whose glyphs are not uniform width, which
/// cell-width arithmetic is not. It can report several boxes for one visual
/// row (bidi); they are unioned per row so every consumer downstream sees the
/// one-rect-per-row shape it expects.
///
/// This is the single parley query behind both the character-wise selection
/// and the word-level background washes — the "walk clusters in the cached
/// layout" step `docs/diff.md` recorded as the cost of word backgrounds turns
/// out to be a library call we already had.
pub fn layout_rects(
    layout: &parley::Layout<peniko::Brush>,
    bytes: std::ops::Range<usize>,
) -> Vec<crate::shaders::SelectionRect> {
    use crate::shaders::SelectionRect;
    use parley::editing::{Cursor, Selection};
    use parley::layout::Affinity;

    if bytes.end <= bytes.start {
        return Vec::new();
    }
    let selection = Selection::new(
        Cursor::from_byte_index(layout, bytes.start, Affinity::Downstream),
        Cursor::from_byte_index(layout, bytes.end, Affinity::Upstream),
    );
    let mut out: Vec<(SelectionRect, usize)> = Vec::new();
    selection.geometry_with(layout, |rect, line| {
        let next =
            SelectionRect::from_edges(rect.x0 as f32, rect.y0 as f32, rect.x1 as f32, rect.y1 as f32);
        match out.last_mut() {
            Some((prev, prev_line)) if *prev_line == line => {
                *prev = SelectionRect::from_edges(
                    prev.x.min(next.x),
                    prev.y.min(next.y),
                    prev.right().max(next.right()),
                    prev.bottom().max(next.bottom()),
                );
            }
            _ => out.push((next, line)),
        }
    });
    out.into_iter().map(|(rect, _)| rect).collect()
}

/// Background washes behind the words the refinement marked changed.
///
/// `docs/diff.md` "Future / adjacent" wanted these and priced them at a
/// cluster walk; [`layout_rects`] is that walk, already written by parley.
/// They ride the **layout** tier beside the `+`/`-` bands — word spans change
/// only when the content does — and draw over them, under the glyphs, so a
/// changed word reads as a brighter patch inside its line's band.
///
/// Only the two changed classes wash. A span on any other class would be a
/// claim the model never makes (`DiffLine::words` is empty on context lines),
/// exactly as with the foreground emphasis.
pub fn build_word_wash_geometry(
    preview: &DiffPreview,
    layout: &parley::Layout<peniko::Brush>,
    offset: (f32, f32),
    colors: &DiffWordColors,
) -> Vec<GeometryVertex> {
    let mut out = Vec::new();
    for word in &preview.words {
        let color = match word.class {
            DiffLineClass::Insert => colors.insert,
            DiffLineClass::Delete => colors.delete,
            _ => continue,
        };
        // Deliberately NOT coalesced: coalescing squares a multi-row shape off
        // to full width, which is right for a selection and wrong for a word —
        // a word that wraps must stay two word-shaped rects.
        let rects = layout_rects(layout, word.start..word.end);
        out.extend(selection_rect_geometry(&rects, offset, color));
    }
    out
}

/// RGBA8 word-wash colors, resolved from the theme by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffWordColors {
    /// Behind a changed word in an added line.
    pub insert: [u8; 4],
    /// Behind a changed word in a removed line.
    pub delete: [u8; 4],
}

// ════════════════════════════════════════════════════════════════════════════
// The minimap strip
// ════════════════════════════════════════════════════════════════════════════

/// RGBA8 colors for the minimap strip, resolved from the theme by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MinimapColors {
    /// The rail behind everything — how long the document is.
    pub rail: [u8; 4],
    /// Insertion density, drawn right of centre.
    pub insert: [u8; 4],
    /// Deletion density, drawn left of centre.
    pub delete: [u8; 4],
    /// Hunk/file header ticks.
    pub structure: [u8; 4],
    /// The band showing what part of the document is on screen.
    pub viewport: [u8; 4],
    /// The line showing where the cursor is.
    pub cursor: [u8; 4],
}

/// The smallest bar a non-zero count may draw, in logical px.
///
/// **Presence survives compression.** One deleted line inside a thousand-row
/// diff is a fraction of a pixel wide; rounding it to nothing would erase
/// exactly what a reader scans a minimap for. Proportion is lost below this
/// width — deliberately, and only below it.
const MIN_MINIMAP_BAR: f32 = 2.0;

/// The document layer of the strip: rail, per-bucket density bars, hunk ticks.
///
/// `rect` is the strip's box in block-local logical px. Insertions grow right
/// from the centre line and deletions grow left — a diverging bar chart, so
/// "this region is mostly additions" is a *shape*, not a color the reader has
/// to decode.
///
/// O(buckets), not O(rows), which is what lets it be cached against a coarse
/// key and memcpy'd into the geometry buffer on every cheap pass.
pub fn build_minimap_geometry(
    buckets: &[kaijutsu_diff::MinimapBucket],
    rect: crate::shaders::SelectionRect,
    colors: &MinimapColors,
) -> Vec<GeometryVertex> {
    use crate::text::msdf::geometry::rect_quad;
    if buckets.is_empty() || rect.width <= 0.0 || rect.height <= 0.0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(buckets.len() * 18 + 6);
    out.extend_from_slice(&rect_quad(
        rect.x as f64,
        rect.y as f64,
        rect.width as f64,
        rect.height as f64,
        colors.rail,
    ));

    let n = buckets.len() as f32;
    let slot_h = (rect.height / n).max(1.0);
    let half = rect.width * 0.5;
    let centre = rect.x + half;
    for (i, bucket) in buckets.iter().enumerate() {
        if bucket.is_empty() {
            continue;
        }
        let top = rect.y + rect.height * i as f32 / n;
        let rows = bucket.rows() as f32;
        let bar = |count: u32| -> f32 {
            if count == 0 {
                0.0
            } else {
                (half * count as f32 / rows).clamp(MIN_MINIMAP_BAR.min(half), half)
            }
        };

        let del = bar(bucket.deletes);
        if del > 0.0 {
            out.extend_from_slice(&rect_quad(
                (centre - del) as f64,
                top as f64,
                del as f64,
                slot_h as f64,
                colors.delete,
            ));
        }
        let ins = bar(bucket.inserts);
        if ins > 0.0 {
            out.extend_from_slice(&rect_quad(
                centre as f64,
                top as f64,
                ins as f64,
                slot_h as f64,
                colors.insert,
            ));
        }
        // Structure is a full-width tick at the slot's top edge: where the
        // hunks are, readable even where the density bars are empty.
        if bucket.structure > 0 {
            out.extend_from_slice(&rect_quad(
                rect.x as f64,
                top as f64,
                rect.width as f64,
                slot_h.min(2.0) as f64,
                colors.structure,
            ));
        }
    }
    out
}

/// The position layer: the band of document currently on screen, and the
/// cursor's line within it.
///
/// Separate from [`build_minimap_geometry`] because it changes on every
/// keystroke while the document layer changes only when the diff or its folds
/// do — the same two-tier split as the surface's `LayoutKey`/`CursorKey`, one
/// level finer.
pub fn build_minimap_position_geometry(
    rows: usize,
    buckets: usize,
    window: std::ops::Range<usize>,
    cursor: usize,
    rect: crate::shaders::SelectionRect,
    colors: &MinimapColors,
) -> Vec<GeometryVertex> {
    use crate::text::msdf::geometry::rect_quad;
    use kaijutsu_diff::minimap::bucket_of;
    if rows == 0 || buckets == 0 || rect.width <= 0.0 || rect.height <= 0.0 {
        return Vec::new();
    }
    let slot_h = rect.height / buckets as f32;
    let y_of = |bucket: usize| rect.y + rect.height * bucket as f32 / buckets as f32;

    let mut out = Vec::with_capacity(12);
    // The viewport band. `window.end` is exclusive; the band has to cover the
    // last visible row, so it is measured from `end - 1`.
    if window.end > window.start {
        let top = y_of(bucket_of(window.start, rows, buckets));
        let bottom = y_of(bucket_of(window.end - 1, rows, buckets)) + slot_h;
        out.extend_from_slice(&rect_quad(
            rect.x as f64,
            top as f64,
            rect.width as f64,
            (bottom - top).max(slot_h) as f64,
            colors.viewport,
        ));
    }
    let cursor_top = y_of(bucket_of(cursor, rows, buckets));
    out.extend_from_slice(&rect_quad(
        rect.x as f64,
        cursor_top as f64,
        rect.width as f64,
        slot_h.max(2.0) as f64,
        colors.cursor,
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_diff::fixtures;

    fn preview(relative: &str) -> DiffPreview {
        build_diff_preview(&fixtures::read(relative), ContentType::Diff)
    }

    /// The invariant every other lookup depends on: line ranges tile
    /// `plain_text` with no gap and no overlap, so any byte Parley hands back
    /// — a run start, a wrapped row start — resolves to exactly one class.
    fn assert_contiguous(p: &DiffPreview) {
        let mut expected = 0usize;
        for line in &p.lines {
            assert_eq!(line.start, expected, "gap/overlap in preview line ranges");
            assert!(line.end > line.start);
            assert!(line.text_start >= line.start && line.text_start <= line.end);
            expected = line.end;
        }
        assert_eq!(
            expected,
            p.plain_text.len(),
            "line ranges must cover the whole text"
        );
    }

    #[test]
    fn preview_leads_with_the_diffstat_header() {
        let p = preview("canonical/multi_file.diff");
        assert_eq!(p.lines[0].class, DiffLineClass::Stat);
        assert!(
            p.plain_text.starts_with("3 files, +3 \u{2212}3"),
            "stat header missing or wrong: {:?}",
            p.plain_text.lines().next(),
        );
        assert_eq!(p.stat.files_changed, 3);
        assert!(!p.is_error);
        assert_contiguous(&p);
    }

    #[test]
    fn body_lines_carry_their_diff_prefix_and_semantic_class() {
        let p = preview("canonical/single_file_modify.diff");
        let classed: Vec<(DiffLineClass, &str)> = p
            .lines
            .iter()
            .map(|l| {
                let text = &p.plain_text[l.start..l.end.min(p.plain_text.len())];
                (l.class, text.trim_end_matches('\n'))
            })
            .collect();

        assert!(
            classed
                .iter()
                .any(|(c, t)| *c == DiffLineClass::Insert && t.starts_with('+')),
            "no classified insertion in {classed:?}",
        );
        assert!(
            classed
                .iter()
                .any(|(c, t)| *c == DiffLineClass::Delete && t.starts_with('-')),
            "no classified deletion in {classed:?}",
        );
        assert!(
            classed
                .iter()
                .any(|(c, t)| *c == DiffLineClass::HunkHeader && t.starts_with("@@")),
            "no classified hunk header in {classed:?}",
        );
        assert!(
            classed
                .iter()
                .any(|(c, t)| *c == DiffLineClass::FileHeader && t.contains("src/greet.rs")),
            "no classified file header in {classed:?}",
        );
    }

    /// The prefix is one ASCII byte, so a `WordSpan` — which indexes bytes of
    /// `DiffLine::text` — lands correctly by adding `text_start`. Pinned with
    /// multibyte content because that is the case a char/byte confusion
    /// silently corrupts (slice 6 depends on this).
    #[test]
    fn text_start_skips_exactly_the_prefix_even_with_multibyte_text() {
        let raw = "--- a/日本.txt\n+++ b/日本.txt\n@@ -1 +1 @@\n-こんにちは\n+こんばんは\n";
        let p = build_diff_preview(raw, ContentType::Diff);
        let insert = p
            .lines
            .iter()
            .find(|l| l.class == DiffLineClass::Insert)
            .expect("an insertion");
        assert_eq!(insert.text_start, insert.start + 1);
        assert_eq!(&p.plain_text[insert.start..insert.text_start], "+");
        assert!(p.plain_text[insert.text_start..].starts_with("こんばんは"));
        assert_contiguous(&p);
    }

    // ── word-level highlight ────────────────────────────────────────────────

    /// The text each word highlight covers, with the class it belongs to.
    fn highlighted(p: &DiffPreview) -> Vec<(DiffLineClass, &str)> {
        p.words
            .iter()
            .map(|w| (w.class, &p.plain_text[w.start..w.end]))
            .collect()
    }

    /// Word spans must land on the *changed words* of a changed line, not on
    /// the line and not on the diff prefix. `text_start` is the whole
    /// translation: a `WordSpan` indexes bytes of `DiffLine::text`.
    #[test]
    fn word_spans_cover_the_changed_words_inside_a_changed_line() {
        let p = build_diff_preview(
            "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-the quick brown fox\n+the quick red fox\n",
            ContentType::Diff,
        );
        assert_eq!(
            highlighted(&p),
            vec![
                (DiffLineClass::Delete, "brown"),
                (DiffLineClass::Insert, "red"),
            ],
        );
        // ...and never over the `+`/`-` prefix byte.
        for w in &p.words {
            let line = p
                .lines
                .iter()
                .find(|l| w.start >= l.start && w.end <= l.end)
                .expect("every word span lives inside one line");
            assert!(w.start >= line.text_start, "a span covered the diff prefix");
        }
    }

    /// A line with no counterpart (a pure insertion) has nothing to refine
    /// against — highlighting all of it would be a lie dressed as detail.
    #[test]
    fn a_pure_insertion_has_no_word_spans() {
        let p = build_diff_preview(
            "--- a/f\n+++ b/f\n@@ -0,0 +1 @@\n+brand new line\n",
            ContentType::Diff,
        );
        assert!(highlighted(&p).is_empty(), "{:?}", highlighted(&p));
    }

    /// Byte-indexed, all the way through: the spans must slice multibyte text
    /// on char boundaries (a panic here is what a char/byte confusion looks
    /// like) and land on the changed word.
    #[test]
    fn word_spans_are_byte_exact_on_multibyte_text() {
        let raw = "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-日本語 は たのしい\n+日本語 は むずかしい\n";
        let p = build_diff_preview(raw, ContentType::Diff);
        assert_eq!(
            highlighted(&p),
            vec![
                (DiffLineClass::Delete, "たのしい"),
                (DiffLineClass::Insert, "むずかしい"),
            ],
        );
    }

    /// **The slice-4 post-ship finding**: per-line elision cuts the text after
    /// `text_start` is fixed, so a span past the cut would index the ellipsis
    /// or run off the end of the line. Spans are filtered against the cut.
    #[test]
    fn word_spans_past_the_line_elision_are_dropped() {
        let head = "x".repeat(MAX_PREVIEW_LINE_CHARS + 100);
        let raw = format!(
            "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-{head} alpha\n+{head} omega\n",
        );
        let p = build_diff_preview(&raw, ContentType::Diff);
        for w in &p.words {
            assert!(w.end <= p.plain_text.len(), "span ran off the end");
            // Every span must still slice on a char boundary of the *shown*
            // text — the check that fails loudly if the cut is ignored.
            assert!(p.plain_text.is_char_boundary(w.start));
            assert!(p.plain_text.is_char_boundary(w.end));
            let line = p
                .lines
                .iter()
                .find(|l| w.start >= l.start && w.start < l.end)
                .expect("a span outside every line");
            assert!(w.end <= line.end, "a span crossed into the next line");
        }
        // The changed words live past the cut, so nothing survives to draw.
        assert!(
            highlighted(&p).is_empty(),
            "changes past the elision cut must not be highlighted: {:?}",
            highlighted(&p),
        );
    }

    /// A span that *straddles* the cut is clipped to it rather than dropped —
    /// the visible half of the change is still the change.
    #[test]
    fn a_word_span_straddling_the_elision_cut_is_clipped() {
        // `cut` lands inside the long token, so the span starts before it and
        // ends after it.
        let before = format!("head {}", "a".repeat(MAX_PREVIEW_LINE_CHARS));
        let after = format!("head {}", "b".repeat(MAX_PREVIEW_LINE_CHARS));
        let raw = format!("--- a/f\n+++ b/f\n@@ -1 +1 @@\n-{before}\n+{after}\n");
        let p = build_diff_preview(&raw, ContentType::Diff);
        for w in &p.words {
            let line = p
                .lines
                .iter()
                .find(|l| w.start >= l.start && w.start < l.end)
                .expect("a span outside every line");
            assert!(w.end <= line.end, "clip failed: {w:?} vs line {line:?}");
            assert!(
                !p.plain_text[w.start..w.end].contains('\u{2026}'),
                "a span was drawn over the ellipsis",
            );
        }
        assert!(
            !p.words.is_empty(),
            "the visible half of a straddling change must survive",
        );
    }

    #[test]
    fn file_headers_name_what_happened_to_the_file() {
        let p = preview("canonical/multi_file.diff");
        assert!(p.plain_text.contains("src/new.rs (new file)"), "{}", p.plain_text);
        assert!(p.plain_text.contains("src/old.rs (deleted)"), "{}", p.plain_text);

        let renamed = preview("canonical/rename_pure.diff");
        assert!(
            renamed.plain_text.contains('\u{2192}'),
            "a rename must show both paths: {}",
            renamed.plain_text,
        );
    }

    /// Decision 5: the preview is the *first* N lines, and it says what it is
    /// not showing. A truncated preview that looked complete would be the
    /// exact failure the crate's truncation contract exists to prevent.
    #[test]
    fn preview_stops_at_the_line_budget_and_says_how_much_is_hidden() {
        let before: String = (1..=200).map(|i| format!("line {i}\n")).collect();
        let after = before.replace("line 7\n", "SEVEN\n");
        let file = kaijutsu_diff::diff_file(
            &kaijutsu_diff::FileSpec::modified("big.txt", &before, &after),
            &kaijutsu_diff::DiffOptions::default(),
        )
        .unwrap();
        let mut files = vec![file];
        for i in 0..30 {
            let a = format!("a{i}\n");
            let b = format!("b{i}\n");
            files.push(
                kaijutsu_diff::diff_file(
                    &kaijutsu_diff::FileSpec::modified(&format!("f{i}.txt"), &a, &b),
                    &kaijutsu_diff::DiffOptions::default(),
                )
                .unwrap(),
            );
        }
        let model = DiffModel::new(files);
        let text = kaijutsu_diff::format(&model);
        let p = build_diff_preview(&text, ContentType::Diff);

        let body = p
            .lines
            .iter()
            .filter(|l| {
                matches!(
                    l.class,
                    DiffLineClass::Insert | DiffLineClass::Delete | DiffLineClass::Context
                )
            })
            .count();
        assert!(
            body <= DEFAULT_PREVIEW_LINES,
            "preview showed {body} body lines, budget is {DEFAULT_PREVIEW_LINES}",
        );
        assert_eq!(
            p.lines.last().map(|l| l.class),
            Some(DiffLineClass::Meta),
            "a truncated preview must end with an omission note",
        );
        assert!(
            p.plain_text.contains("more line"),
            "omission note missing: {}",
            p.plain_text,
        );
        // The stat is the whole change, not the shown slice.
        assert_eq!(p.stat.files_changed, 31);
        assert_contiguous(&p);
    }

    /// A single absurd line inside the line budget must not reach Parley
    /// whole. `truncate_to_bytes` bounds the total; this bounds the shape.
    #[test]
    fn an_enormous_single_line_is_elided() {
        let long = "x".repeat(50_000);
        let raw = format!("--- a/f\n+++ b/f\n@@ -1 +1 @@\n-short\n+{long}\n");
        let p = build_diff_preview(&raw, ContentType::Diff);
        assert!(
            p.plain_text.len() < 5_000,
            "a 50k-char line reached the layout text ({} bytes)",
            p.plain_text.len(),
        );
        assert!(p.plain_text.contains('\u{2026}'));
        assert_contiguous(&p);
    }

    /// The declared-Diff-that-doesn't-parse contract: visible, named, and the
    /// source still readable. Never empty, never a panic.
    #[test]
    fn unparseable_content_declared_as_a_diff_renders_as_a_visible_error() {
        let p = build_diff_preview("just some prose, honestly\nnot a diff\n", ContentType::Diff);
        assert!(p.is_error);
        assert_eq!(p.lines[0].class, DiffLineClass::Error);
        assert!(
            p.plain_text.starts_with("[declared as a diff but does not parse]"),
            "{}",
            p.plain_text,
        );
        assert!(
            p.plain_text.contains("just some prose"),
            "the source must stay visible: {}",
            p.plain_text,
        );
        assert_contiguous(&p);
    }

    /// Every rejection in the shared corpus produces a usable error preview —
    /// binary patches included, which is the case most likely to be huge.
    #[test]
    fn every_invalid_fixture_previews_as_an_error_never_empty() {
        for (relative, _variant) in fixtures::INVALID {
            let p = build_diff_preview(&fixtures::read(relative), ContentType::Diff);
            assert!(p.is_error, "{relative} should be an error preview");
            assert!(!p.plain_text.is_empty(), "{relative} previewed as nothing");
            assert_contiguous(&p);
        }
    }

    /// The sniffing path must not hijack a fenced block that isn't a diff.
    #[test]
    fn try_build_returns_none_for_content_that_is_not_a_diff() {
        assert!(try_build_diff_preview("hello\nworld\n", ContentType::Plain).is_none());
        assert!(try_build_diff_preview("", ContentType::Plain).is_none());
        assert!(
            try_build_diff_preview(
                &fixtures::read("canonical/single_file_modify.diff"),
                ContentType::Plain
            )
            .is_some()
        );
    }

    /// A diff that arrives already truncated says so — the marker parses back
    /// into the model, and the header must not present a partial stat as the
    /// whole story.
    #[test]
    fn an_already_truncated_block_is_labelled_partial() {
        let p = preview("canonical/truncated.diff");
        assert!(
            p.plain_text.lines().next().unwrap().contains("partial diff"),
            "{}",
            p.plain_text,
        );
    }

    // ── the full viewer's projection ────────────────────────────────────────

    fn core(relative: &str) -> kaijutsu_diff::DiffCore {
        let model = kaijutsu_diff::parse(&fixtures::read(relative)).expect("fixture parses");
        kaijutsu_diff::DiffCore::new(model)
    }

    /// The whole document as one window — what every assertion below wants.
    fn whole(c: &kaijutsu_diff::DiffCore) -> DiffPreview {
        view_rows(c, 0..c.rows().len())
    }

    /// The projection's load-bearing property: one laid-out line per core row,
    /// in order. The cursor row, the selection range, and the band lookup are
    /// all that index — get this wrong and the cursor draws on the wrong line.
    #[test]
    fn the_full_view_is_one_line_per_core_row_in_order() {
        let c = core("canonical/multi_file.diff");
        let v = whole(&c);
        assert_eq!(v.lines.len(), c.rows().len());
        assert_eq!(v.plain_text, c.text().trim_end_matches('\n'));
        assert_contiguous(&v);

        for (i, (line, row)) in v.lines.iter().zip(c.rows()).enumerate() {
            assert_eq!(line.class, class_for_row(row.kind), "row {i}");
            assert_eq!(line.start, row.start, "row {i} starts elsewhere");
        }
    }

    /// Unlike the inline preview, the full view has no line budget and no stat
    /// header — its text IS the canonical diff, so a row index means the same
    /// thing to the renderer and to the cursor.
    #[test]
    fn the_full_view_shows_every_line_and_leads_with_the_diff_itself() {
        // Change every tenth line, so the diff is genuinely long rather than
        // one small hunk in a long file.
        let before: String = (1..=200).map(|i| format!("line {i}\n")).collect();
        let after: String = (1..=200)
            .map(|i| {
                if i % 10 == 0 {
                    format!("CHANGED {i}\n")
                } else {
                    format!("line {i}\n")
                }
            })
            .collect();
        let file = kaijutsu_diff::diff_file(
            &kaijutsu_diff::FileSpec::modified("big.txt", &before, &after),
            &kaijutsu_diff::DiffOptions::default(),
        )
        .unwrap();
        let c = kaijutsu_diff::DiffCore::new(DiffModel::new(vec![file]));
        let v = whole(&c);

        assert!(
            v.lines.len() > DEFAULT_PREVIEW_LINES,
            "the full view must not stop at the preview budget",
        );
        assert_eq!(v.lines[0].class, DiffLineClass::FileHeader);
        assert!(v.plain_text.starts_with("diff --git"), "{}", v.plain_text);
        assert!(v.lines.iter().all(|l| l.class != DiffLineClass::Stat));
    }

    // ── folds ───────────────────────────────────────────────────────────────

    /// A folded hunk draws as its header alone, carrying an indicator that
    /// says how much is hidden. The rows are gone from the *projection* only —
    /// `core.rows()` and the canonical text are untouched, which is what keeps
    /// yank honest.
    #[test]
    fn a_folded_hunk_projects_as_one_annotated_header_row() {
        let mut c = core("canonical/single_file_modify.diff");
        c.apply_notation("]czc");
        let header = c.cursor_row();
        let hidden = c.fold_summary(header).expect("the hunk is folded");

        let v = whole(&c);
        assert_eq!(v.lines.len(), c.visible_rows().len());
        assert_eq!(
            v.lines.len(),
            c.rows().len() - hidden.hidden_rows,
            "the fold's body must not be laid out",
        );
        assert!(
            v.plain_text
                .contains(&format!("\u{22ef} {} lines folded", hidden.hidden_rows)),
            "the header must say what it is hiding: {}",
            v.plain_text,
        );
        assert_contiguous(&v);
    }

    /// Unfolding restores the projection exactly — a fold is view state, so a
    /// round trip through it must leave nothing behind.
    #[test]
    fn folding_and_unfolding_round_trips_the_projection() {
        let mut c = core("canonical/single_file_modify.diff");
        let before = whole(&c);
        c.apply_notation("]czc");
        assert_ne!(whole(&c).plain_text, before.plain_text);
        c.apply_notation("zo");
        assert_eq!(whole(&c), before);
    }

    /// The window is in visible coordinates too: a range past a fold projects
    /// the rows that survived it, not the ones the canonical indices name.
    #[test]
    fn a_window_over_a_folded_document_follows_the_visible_rows() {
        let mut c = core("canonical/multi_file.diff");
        c.apply_notation("]czc");
        let v = view_rows(&c, 1..4);
        assert_eq!(v.lines.len(), 3);
        for (i, line) in v.lines.iter().enumerate() {
            let row = c.rows()[c.visible_rows()[1 + i]];
            assert_eq!(line.class, class_for_row(row.kind), "line {i}");
        }
    }

    /// The full view highlights words too, and reaches them the only way it
    /// can: through each row's provenance back into the model. Word spans are
    /// derived, never serialized, so they are not in the canonical text.
    #[test]
    fn the_full_view_carries_word_spans_from_the_model() {
        let raw = "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-the quick brown fox\n+the quick red fox\n";
        let model = kaijutsu_diff::parse(raw).expect("parses");
        let c = kaijutsu_diff::DiffCore::new(model);
        let v = whole(&c);
        assert_eq!(
            highlighted(&v),
            vec![
                (DiffLineClass::Delete, "brown"),
                (DiffLineClass::Insert, "red"),
            ],
        );
    }

    /// A `\ No newline at end of file` row carries the provenance of the body
    /// line above it and none of its text — highlighting it with that line's
    /// spans would paint the marker.
    #[test]
    fn a_no_newline_marker_row_gets_no_word_spans() {
        let file = kaijutsu_diff::diff_file(
            &kaijutsu_diff::FileSpec::modified("a", "alpha beta\n", "alpha gamma"),
            &kaijutsu_diff::DiffOptions::default(),
        )
        .unwrap();
        let c = kaijutsu_diff::DiffCore::new(DiffModel::new(vec![file]));
        let v = whole(&c);
        let marker = v
            .lines
            .iter()
            .zip(c.rows())
            .find(|(_, r)| r.kind == kaijutsu_diff::RowKind::NoNewline)
            .map(|(l, _)| *l)
            .expect("a no-newline row");
        assert!(
            !v.words
                .iter()
                .any(|w| w.start >= marker.start && w.start < marker.end),
            "the no-newline marker must carry no word spans",
        );
    }

    /// A truncated model's marker leads the canonical text, so it leads the
    /// view — and reads as meta, not as content.
    #[test]
    fn a_truncated_model_shows_its_marker_first() {
        let c = core("canonical/truncated.diff");
        let v = whole(&c);
        assert_eq!(v.lines[0].class, DiffLineClass::Meta);
        assert!(
            v.plain_text
                .starts_with(kaijutsu_diff::TRUNCATION_MARKER_PREFIX)
        );
    }

    /// One absurd line must not reach Parley whole, even in the full view —
    /// and eliding it must not disturb the one-line-per-row correspondence.
    #[test]
    fn an_enormous_line_is_elided_without_losing_a_row() {
        let long = "x".repeat(50_000);
        let raw = format!("--- a/f\n+++ b/f\n@@ -1 +1 @@\n-short\n+{long}\n");
        let model = kaijutsu_diff::parse(&raw).expect("parses");
        let c = kaijutsu_diff::DiffCore::new(model);
        let v = whole(&c);
        assert_eq!(v.lines.len(), c.rows().len(), "elision dropped a row");
        assert!(v.plain_text.len() < 20_000, "{} bytes", v.plain_text.len());
        assert!(v.plain_text.contains('\u{2026}'));
        assert_contiguous(&v);
    }

    /// The declared-Diff-that-doesn't-parse contract, at full-screen scale:
    /// named, visible, source still readable — never an empty viewer.
    #[test]
    fn the_error_view_names_the_problem_and_keeps_the_source() {
        let source: String = (1..=200).map(|i| format!("not a diff {i}\n")).collect();
        let v = error_view(&source, "expected a file header");
        assert!(v.is_error);
        assert_eq!(v.lines[0].class, DiffLineClass::Error);
        assert!(v.plain_text.contains("expected a file header"));
        assert!(
            v.plain_text.contains("not a diff 150"),
            "shows more than a preview would"
        );
        assert_contiguous(&v);
    }

    // ── the cursor's column ─────────────────────────────────────────────────

    /// The column is counted in chars and the answer is a byte offset — the
    /// one conversion on the cursor path, and the one a multibyte line would
    /// silently corrupt.
    #[test]
    fn cursor_byte_converts_a_char_column_to_a_byte_offset() {
        let raw = "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-日本語\n+にほんご\n";
        let c = kaijutsu_diff::DiffCore::new(kaijutsu_diff::parse(raw).expect("parses"));
        let v = whole(&c);
        let insert = v
            .lines
            .iter()
            .position(|l| l.class == DiffLineClass::Insert)
            .expect("an insertion");
        let start = v.lines[insert].start;

        assert_eq!(cursor_byte(&v, insert, 0), Some(start), "column 0 is the +");
        // One char in is one ASCII byte; the next three chars are 3 bytes each.
        assert_eq!(cursor_byte(&v, insert, 1), Some(start + 1));
        assert_eq!(cursor_byte(&v, insert, 3), Some(start + 1 + 6));
    }

    /// A column past the end of a short row clamps to *that* row. The two
    /// clamps differ on purpose, and the difference is a real bug either way
    /// round: `column_byte` stops at the end of the text (the exclusive end a
    /// selection wants), `cursor_byte` stops **on the last character**.
    ///
    /// `text.len()` is the offset of the line's `\n`, and Parley resolves that
    /// downstream — onto the NEXT visual row. `$` then `j` onto a shorter line
    /// drew the cursor a line low until this clamp existed.
    #[test]
    fn the_two_column_clamps_stop_in_different_places() {
        let c = core("canonical/single_file_modify.diff");
        let v = whole(&c);
        let line = v.lines[1];
        let text = &v.plain_text[line.start..line.end];
        let newline = line.end - 1; // the line's own text ends here
        assert_eq!(&text[text.len() - 1..], "\n", "premise: it has a newline");

        assert_eq!(
            column_byte(&v, 1, 9_999),
            Some(newline),
            "an exclusive end stops at the end of the text",
        );
        assert_eq!(
            cursor_byte(&v, 1, 9_999),
            Some(newline - 1),
            "a drawn cursor stops ON the last character, never on the newline",
        );
        assert!(cursor_byte(&v, 9_999, 0).is_none(), "no such line");
        assert!(column_byte(&v, 9_999, 0).is_none());
    }

    /// An empty line has no character to sit on. Both clamps land on the line
    /// start (which *is* its newline) rather than panicking or walking
    /// backwards into the line above.
    #[test]
    fn an_empty_line_clamps_to_its_own_start() {
        // The error view is the one path that lays out arbitrary source text,
        // blank lines included.
        let v = error_view("not a diff\n\nstill not\n", "no file header");
        let blank = v
            .lines
            .iter()
            .position(|l| v.plain_text[l.start..l.end].trim_end_matches('\n').is_empty())
            .expect("a blank source line");
        let start = v.lines[blank].start;
        assert_eq!(cursor_byte(&v, blank, 0), Some(start));
        assert_eq!(cursor_byte(&v, blank, 40), Some(start));
        assert_eq!(column_byte(&v, blank, 40), Some(start));
    }

    /// The elision cut is a **byte** offset derived from a **char** count.
    /// Both of its uses (the clip in `push_words`, the slice the renderer
    /// takes) assume it is a char boundary, and a multibyte character sitting
    /// exactly at the cut is where that assumption would break.
    #[test]
    fn the_elision_cut_always_lands_on_a_char_boundary() {
        let text = "aaa\u{65e5}bbb";
        for max in 0..=text.chars().count() + 2 {
            let cut = elision_cut(text, max);
            assert!(
                text.is_char_boundary(cut),
                "cut {cut} for max {max} split a character",
            );
        }
        // The interesting one, spelled out: the 4th char IS the multibyte one.
        assert_eq!(elision_cut(text, 4), 3);
        assert_eq!(elision_cut(text, 5), 6, "past it, not into it");
    }

    /// ...and the same, end to end: a word span straddling a cut that falls
    /// inside a run of multibyte characters must come out sliceable.
    #[test]
    fn word_spans_survive_a_cut_that_lands_mid_multibyte_run() {
        use kaijutsu_diff::{DiffModel, DiffOptions, FileSpec, diff_file};
        // The changed run starts a couple of chars before the cut and ends
        // well past it, and every character in it is three bytes wide.
        let head = "\u{65e5}".repeat(MAX_PREVIEW_LINE_CHARS - 3);
        let before = format!("{head}\u{3042}\u{3044}\u{3046}\u{3048}\u{304a}\n");
        let after = format!("{head}\u{304b}\u{304d}\u{304f}\u{3051}\u{3053}\n");
        let file = diff_file(
            &FileSpec::modified("wide.txt", &before, &after),
            &DiffOptions::default(),
        )
        .expect("diffs");
        let text = kaijutsu_diff::format(&DiffModel::new(vec![file]));

        let p = build_diff_preview(&text, ContentType::Diff);
        assert!(!p.words.is_empty(), "premise: the refinement found words");
        for w in &p.words {
            assert!(w.end > w.start, "an empty span survived the clip");
            assert!(
                p.plain_text.is_char_boundary(w.start) && p.plain_text.is_char_boundary(w.end),
                "span {}..{} splits a character",
                w.start,
                w.end,
            );
            // What the renderer actually does with it.
            let _ = &p.plain_text[w.start..w.end];
        }
        assert_contiguous(&p);
    }

    // ── character-wise selection extents ────────────────────────────────────

    fn span(
        start_row: usize,
        start_col: usize,
        end_row: usize,
        end_col: usize,
    ) -> kaijutsu_diff::CharSpan {
        kaijutsu_diff::CharSpan {
            start_row,
            start_col,
            end_row,
            end_col,
        }
    }

    /// The inclusive end really is inclusive: a one-character selection covers
    /// one character, not zero.
    #[test]
    fn a_one_character_selection_covers_one_character() {
        let c = core("canonical/single_file_modify.diff");
        let v = whole(&c);
        let bytes = char_selection_bytes(&v, span(1, 0, 1, 0)).expect("a range");
        assert_eq!(bytes.len(), 1);
        assert_eq!(bytes.start, v.lines[1].start);
    }

    /// A selection running off the bottom of the laid-out window takes the
    /// last row whole rather than disappearing.
    #[test]
    fn a_selection_past_the_window_clamps_to_its_last_row() {
        let c = core("canonical/single_file_modify.diff");
        let v = whole(&c);
        let last = v.lines.len() - 1;
        let bytes = char_selection_bytes(&v, span(0, 0, 9_999, 9_999)).expect("a range");
        assert_eq!(bytes.start, 0);
        assert_eq!(
            bytes.end,
            v.lines[last].end,
            "the last row must be taken whole",
        );
        // ...and it never runs past the text Parley was given.
        assert!(bytes.end <= v.plain_text.len());
    }

    /// Degenerate spans produce no range at all — a caller must not turn one
    /// into a zero-area rectangle.
    #[test]
    fn a_degenerate_character_span_has_no_range() {
        let c = core("canonical/single_file_modify.diff");
        let v = whole(&c);
        assert!(char_selection_bytes(&v, span(9_999, 0, 9_999, 4)).is_none());
        let empty = DiffPreview {
            plain_text: String::new(),
            lines: Vec::new(),
            words: Vec::new(),
            stat: DiffStat::default(),
            is_error: false,
        };
        assert!(char_selection_bytes(&empty, span(0, 0, 0, 0)).is_none());
    }

    /// A multi-row span covers every byte between its ends, so the rects built
    /// from it cannot skip a row.
    #[test]
    fn a_multi_row_span_is_contiguous() {
        let c = core("canonical/single_file_modify.diff");
        let v = whole(&c);
        let bytes = char_selection_bytes(&v, span(1, 2, 3, 1)).expect("a range");
        assert_eq!(bytes.start, v.lines[1].start + 2);
        assert!(bytes.end > v.lines[3].start);
        assert!(bytes.end <= v.lines[3].end);
    }

    // ── rects from a real layout ────────────────────────────────────────────

    /// The shipped mono font, registered exactly as the asset loader does —
    /// the rect math is *parley's*, so shaping for real is the point.
    fn mono() -> crate::text::shaping::VelloFont {
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../assets/fonts/NotoMono-Regular.ttf"
        ))
        .expect("shipped test font must be present");
        crate::text::shaping::load_into_font_context(bytes)
    }

    fn laid_out(text: &str, width: Option<f32>) -> parley::Layout<peniko::Brush> {
        use crate::text::shaping::{VelloTextAlign, VelloTextStyle};
        mono().layout(
            text,
            &VelloTextStyle {
                font_size: 16.0,
                ..Default::default()
            },
            VelloTextAlign::Left,
            width,
        )
    }

    /// One rect per visual row, and a range inside one row is one rect that
    /// starts where the text does — not at the left margin.
    #[test]
    fn layout_rects_are_one_per_visual_row() {
        let layout = laid_out("aaaa\nbbbb\ncccc", None);
        let inside = layout_rects(&layout, 1..3);
        assert_eq!(inside.len(), 1, "a range inside one row is one rect");
        assert!(inside[0].x > 0.0, "it starts at the second character");
        assert!(inside[0].is_visible());

        let across = layout_rects(&layout, 1..12);
        assert_eq!(across.len(), 3, "one rect per row it crosses");
        // Rows descend the page in order.
        assert!(across[0].y < across[1].y && across[1].y < across[2].y);
    }

    /// A range that wraps produces a rect per *visual* row, which is the whole
    /// reason this goes through Parley instead of cell arithmetic.
    #[test]
    fn layout_rects_follow_a_wrap() {
        // Narrow enough that one logical line must wrap.
        let layout = laid_out("aaaa bbbb cccc dddd eeee ffff", Some(60.0));
        assert!(layout.lines().count() > 1, "premise: it wrapped");
        let rects = layout_rects(&layout, 0..29);
        assert_eq!(rects.len(), layout.lines().count());
    }

    #[test]
    fn an_empty_range_has_no_rects() {
        let layout = laid_out("aaaa", None);
        assert!(layout_rects(&layout, 2..2).is_empty());
        assert!(layout_rects(&layout, 3..1).is_empty());
    }

    /// Word washes draw one quad per rect, for the changed classes only —
    /// a context line has no word spans to wash and no claim to make.
    #[test]
    fn word_washes_draw_only_the_changed_classes() {
        let colors = DiffWordColors {
            insert: [1, 2, 3, 4],
            delete: [5, 6, 7, 8],
        };
        let mut p = preview("canonical/single_file_modify.diff");
        let layout = laid_out(&p.plain_text, None);

        let with_words = build_word_wash_geometry(&p, &layout, (0.0, 0.0), &colors);
        for v in &with_words {
            assert!(
                v.color == colors.insert || v.color == colors.delete,
                "a wash used a color no class asks for: {:?}",
                v.color,
            );
        }
        assert_eq!(with_words.len() % 6, 0, "quads are six vertices");

        // Strip the word spans and the washes go with them.
        p.words.clear();
        assert!(build_word_wash_geometry(&p, &layout, (0.0, 0.0), &colors).is_empty());
    }

    // ── minimap ─────────────────────────────────────────────────────────────

    fn minimap_colors() -> MinimapColors {
        MinimapColors {
            rail: [1, 1, 1, 1],
            insert: [2, 2, 2, 2],
            delete: [3, 3, 3, 3],
            structure: [4, 4, 4, 4],
            viewport: [5, 5, 5, 5],
            cursor: [6, 6, 6, 6],
        }
    }

    fn strip() -> crate::shaders::SelectionRect {
        crate::shaders::SelectionRect::new(900.0, 50.0, 12.0, 600.0)
    }

    /// **Presence survives compression**: a bucket holding one deletion among
    /// a hundred context rows still draws a bar, even though its proportion
    /// rounds to a fraction of a pixel.
    #[test]
    fn a_lone_change_still_draws_a_bar() {
        let mut bucket = kaijutsu_diff::MinimapBucket {
            context: 100,
            ..Default::default()
        };
        let colors = minimap_colors();
        let rail_only = build_minimap_geometry(&[bucket], strip(), &colors);

        bucket.deletes = 1;
        let with_change = build_minimap_geometry(&[bucket], strip(), &colors);
        assert!(
            with_change.len() > rail_only.len(),
            "one deletion in a hundred rows drew nothing",
        );
        assert!(
            with_change.iter().any(|v| v.color == colors.delete),
            "the bar is not in the deletion color",
        );
    }

    /// Deletions grow left of centre and insertions right — the diverging
    /// shape is what makes "mostly additions here" readable without decoding
    /// a color.
    #[test]
    fn insertions_and_deletions_diverge_from_the_centre() {
        let colors = minimap_colors();
        let rect = strip();
        let centre = rect.x + rect.width * 0.5;
        let quads = build_minimap_geometry(
            &[kaijutsu_diff::MinimapBucket {
                inserts: 1,
                deletes: 1,
                ..Default::default()
            }],
            rect,
            &colors,
        );
        for v in quads.iter().filter(|v| v.color == colors.delete) {
            assert!(v.x <= centre + 0.01, "a deletion drew right of centre");
        }
        for v in quads.iter().filter(|v| v.color == colors.insert) {
            assert!(v.x >= centre - 0.01, "an insertion drew left of centre");
        }
    }

    /// An empty slot draws nothing but the rail: a short diff should look
    /// short, not smeared down the strip.
    #[test]
    fn empty_buckets_draw_only_the_rail() {
        let colors = minimap_colors();
        let quads =
            build_minimap_geometry(&[kaijutsu_diff::MinimapBucket::default(); 40], strip(), &colors);
        assert_eq!(quads.len(), 6, "the rail is one quad");
        assert!(quads.iter().all(|v| v.color == colors.rail));
    }

    #[test]
    fn a_degenerate_strip_draws_nothing() {
        let colors = minimap_colors();
        let flat = crate::shaders::SelectionRect::new(900.0, 50.0, 0.0, 600.0);
        assert!(build_minimap_geometry(&[kaijutsu_diff::MinimapBucket::default()], flat, &colors).is_empty());
        assert!(build_minimap_geometry(&[], strip(), &colors).is_empty());
        assert!(build_minimap_position_geometry(0, 8, 0..1, 0, strip(), &colors).is_empty());
        assert!(build_minimap_position_geometry(10, 0, 0..1, 0, strip(), &colors).is_empty());
    }

    /// The position layer tracks the reader: the viewport band moves down as
    /// the window does, and the cursor line lands inside it.
    #[test]
    fn the_position_layer_follows_the_window_and_the_cursor() {
        let colors = minimap_colors();
        let rect = strip();
        let y_of = |first: usize| {
            let quads =
                build_minimap_position_geometry(1000, 200, first..first + 40, first, rect, &colors);
            let band: Vec<f32> = quads
                .iter()
                .filter(|v| v.color == colors.viewport)
                .map(|v| v.y)
                .collect();
            let cursor: Vec<f32> = quads
                .iter()
                .filter(|v| v.color == colors.cursor)
                .map(|v| v.y)
                .collect();
            assert!(!band.is_empty() && !cursor.is_empty());
            (
                band.iter().cloned().fold(f32::MAX, f32::min),
                band.iter().cloned().fold(f32::MIN, f32::max),
                cursor.iter().cloned().fold(f32::MAX, f32::min),
            )
        };
        let (top_a, _, cur_a) = y_of(0);
        let (top_b, bottom_b, cur_b) = y_of(500);
        assert!(top_b > top_a, "the band must follow the window down");
        assert!(cur_b > cur_a, "and so must the cursor line");
        assert!(
            cur_b >= top_b - 0.01 && cur_b <= bottom_b + 0.01,
            "the cursor line must sit inside the band it is in",
        );
        assert!(
            top_a >= rect.y && bottom_b <= rect.y + rect.height + 2.0,
            "the strip must stay inside its own box",
        );
    }

    // ── bands ───────────────────────────────────────────────────────────────

    fn test_colors() -> DiffBandColors {
        DiffBandColors {
            insert: [1, 2, 3, 4],
            delete: [5, 6, 7, 8],
            error: [9, 10, 11, 12],
        }
    }

    /// Rows are addressed by byte offset, so a wrapped `+` line gets a band on
    /// every visual row it occupies — not just its first.
    #[test]
    fn bands_cover_every_visual_row_of_a_wrapped_changed_line() {
        let p = build_diff_preview(
            "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-old\n+new and rather long\n",
            ContentType::Diff,
        );
        let insert = p
            .lines
            .iter()
            .find(|l| l.class == DiffLineClass::Insert)
            .unwrap();
        // Pretend Parley wrapped the insertion across two rows.
        let mid = insert.start + 5;
        let rows = vec![
            PreviewRow { text_offset: insert.start, top: 0.0, bottom: 10.0 },
            PreviewRow { text_offset: mid, top: 10.0, bottom: 20.0 },
        ];
        let verts = build_diff_band_geometry(&p, &rows, 100.0, (0.0, 0.0), &test_colors());
        assert_eq!(verts.len(), 12, "two rows × one quad × six vertices");
        assert!(verts.iter().all(|v| v.color == [1, 2, 3, 4]));
    }

    /// Headers and context lines are carried by text color alone. A band
    /// behind every row would turn the preview into a solid block and cost a
    /// quad per line for nothing.
    #[test]
    fn unchanged_and_header_rows_produce_no_geometry() {
        let p = preview("canonical/single_file_modify.diff");
        let rows: Vec<PreviewRow> = p
            .lines
            .iter()
            .filter(|l| {
                matches!(
                    l.class,
                    DiffLineClass::Stat
                        | DiffLineClass::FileHeader
                        | DiffLineClass::HunkHeader
                        | DiffLineClass::Context
                )
            })
            .map(|l| PreviewRow { text_offset: l.start, top: 0.0, bottom: 10.0 })
            .collect();
        assert!(!rows.is_empty(), "test premise: there are unchanged rows");
        assert!(build_diff_band_geometry(&p, &rows, 100.0, (0.0, 0.0), &test_colors()).is_empty());
    }

    /// A band spans the full content width at the row's own height, offset
    /// into the block's padded content box.
    #[test]
    fn a_band_quad_matches_the_row_box() {
        let p = build_diff_preview(
            "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-old\n+new\n",
            ContentType::Diff,
        );
        let insert = p
            .lines
            .iter()
            .find(|l| l.class == DiffLineClass::Insert)
            .unwrap();
        let rows = vec![PreviewRow { text_offset: insert.start, top: 4.0, bottom: 20.0 }];
        let verts = build_diff_band_geometry(&p, &rows, 80.0, (6.0, 3.0), &test_colors());
        let xs: Vec<f32> = verts.iter().map(|v| v.x).collect();
        let ys: Vec<f32> = verts.iter().map(|v| v.y).collect();
        let min = |v: &Vec<f32>| v.iter().cloned().fold(f32::INFINITY, f32::min);
        let max = |v: &Vec<f32>| v.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        assert!((min(&xs) - 6.0).abs() < 0.01, "left edge at pad_left");
        assert!((max(&xs) - 86.0).abs() < 0.01, "right edge at pad_left + width");
        assert!((min(&ys) - 7.0).abs() < 0.01, "top at row top + pad_top");
        assert!((max(&ys) - 23.0).abs() < 0.01, "bottom at row bottom + pad_top");
    }

    /// A degenerate content box (a block whose layout hasn't settled) draws
    /// nothing rather than a zero-area or inverted quad.
    #[test]
    fn a_degenerate_box_draws_no_bands() {
        let p = build_diff_preview("--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b\n", ContentType::Diff);
        let insert = p.lines.iter().find(|l| l.class == DiffLineClass::Insert).unwrap();
        let rows = vec![PreviewRow { text_offset: insert.start, top: 0.0, bottom: 10.0 }];
        assert!(build_diff_band_geometry(&p, &rows, 0.0, (0.0, 0.0), &test_colors()).is_empty());

        let flat = vec![PreviewRow { text_offset: insert.start, top: 5.0, bottom: 5.0 }];
        assert!(build_diff_band_geometry(&p, &flat, 50.0, (0.0, 0.0), &test_colors()).is_empty());
    }

    // ── selection bands ─────────────────────────────────────────────────────

    /// Line-wise selection: every visual row of every selected logical line
    /// gets a band, and nothing outside the range does.
    #[test]
    fn selection_bands_cover_exactly_the_selected_rows() {
        let c = core("canonical/single_file_modify.diff");
        let v = whole(&c);
        let rows: Vec<PreviewRow> = v
            .lines
            .iter()
            .enumerate()
            .map(|(i, l)| PreviewRow {
                text_offset: l.start,
                top: i as f32 * 10.0,
                bottom: i as f32 * 10.0 + 10.0,
            })
            .collect();

        let verts = build_selection_band_geometry(&v, &rows, (1, 3), 100.0, (0.0, 0.0), [7; 4]);
        assert_eq!(verts.len(), 3 * 6, "three rows × one quad × six vertices");
        let ys: Vec<f32> = verts.iter().map(|x| x.y).collect();
        let min = ys.iter().cloned().fold(f32::INFINITY, f32::min);
        let max = ys.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        assert!((min - 10.0).abs() < 0.01, "starts at line 1");
        assert!((max - 40.0).abs() < 0.01, "ends at the bottom of line 3");
    }

    /// A wrapped selected line is highlighted across every row it occupies —
    /// the same byte-offset addressing the diff bands use.
    #[test]
    fn a_wrapped_selected_line_is_banded_on_every_row() {
        let c = core("canonical/single_file_modify.diff");
        let v = whole(&c);
        let line = v.lines[2];
        let rows = vec![
            PreviewRow {
                text_offset: line.start,
                top: 0.0,
                bottom: 10.0,
            },
            PreviewRow {
                text_offset: line.start + 2,
                top: 10.0,
                bottom: 20.0,
            },
        ];
        let verts = build_selection_band_geometry(&v, &rows, (2, 2), 50.0, (0.0, 0.0), [7; 4]);
        assert_eq!(verts.len(), 12);
    }

    #[test]
    fn a_degenerate_or_out_of_range_selection_draws_nothing() {
        let c = core("canonical/single_file_modify.diff");
        let v = whole(&c);
        let rows = vec![PreviewRow {
            text_offset: 0,
            top: 0.0,
            bottom: 10.0,
        }];
        // Inverted, out of range, and zero-width all draw nothing.
        assert!(
            build_selection_band_geometry(&v, &rows, (3, 1), 50.0, (0.0, 0.0), [7; 4]).is_empty()
        );
        assert!(
            build_selection_band_geometry(&v, &rows, (9_999, 9_999), 50.0, (0.0, 0.0), [7; 4])
                .is_empty()
        );
        assert!(
            build_selection_band_geometry(&v, &rows, (0, 0), 0.0, (0.0, 0.0), [7; 4]).is_empty()
        );
    }

    /// Parley reports a trailing empty line for text ending in a newline. We
    /// don't produce one, but a row past the end must resolve to "no class"
    /// rather than panic or index out of bounds.
    #[test]
    fn a_row_past_the_end_of_the_text_is_classless() {
        let p = build_diff_preview("--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b\n", ContentType::Diff);
        assert_eq!(p.class_at(p.plain_text.len()), None);
        let rows = vec![PreviewRow {
            text_offset: p.plain_text.len(),
            top: 0.0,
            bottom: 10.0,
        }];
        assert!(build_diff_band_geometry(&p, &rows, 50.0, (0.0, 0.0), &test_colors()).is_empty());
    }
}
