//! `BlockSnapshot` → styled `ratatui` lines. The pure mapper: no RPC, no
//! I/O, no clock.
//!
//! Everything a block needs from outside itself — who is speaking, the
//! context type, the wallclock stamp, whether it renders collapsed — arrives
//! in a [`BlockView`], so this module can be unit-tested without a kernel and
//! without a terminal. `docs/tui.md`, "Shape: the ACP bridge minus the
//! protocol".
//!
//! **Nothing you would paste is inside a box.** Tool output, code and paths
//! render flush-left with no border glyphs and no column bars. The role
//! divider is the only ruled line this module emits (`docs/tui.md`,
//! "Surfaces").

use std::collections::HashMap;

use kaijutsu_present::format::{BlockTone, block_tone, format_single_block};
use kaijutsu_present::markdown::{SpanTone, parse_to_rich_spans};
use kaijutsu_types::{
    BlockId, BlockKind, BlockSnapshot, ContextId, Role, StyleAttrs, StyleColor, StyleSpan,
};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use crate::layout::layout_output;

/// Marks a block rendered collapsed.
pub const COLLAPSED_MARK: &str = "▸";
/// Marks an `Error` block's one-line stub.
pub const ERROR_MARK: &str = "✗";

/// A tone's terminal style.
///
/// A `theme.toml` → ratatui loader is a later lane; [`Palette::builtin`] is
/// the shipped default. Both resolvers are total functions with no default
/// arm, so a tone added to `kaijutsu-present` fails this match rather than
/// silently painting a placeholder nobody notices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// Set when the terminal is known to render on a light ground. Only the
    /// dim role differs today; kept so a theme loader has somewhere to put
    /// the distinction.
    pub light_ground: bool,
}

impl Default for Palette {
    fn default() -> Self {
        Self::builtin()
    }
}

impl Palette {
    /// The built-in default palette: 16-color ANSI, so it inherits whatever
    /// the terminal's own scheme is instead of fighting it.
    pub const fn builtin() -> Self {
        Self {
            light_ground: false,
        }
    }

    /// The style a block's text renders in.
    pub fn block(&self, tone: BlockTone) -> Style {
        let s = Style::new();
        match tone {
            BlockTone::User => s.fg(Color::Cyan),
            BlockTone::Assistant => s.fg(Color::Reset),
            BlockTone::Thinking => s.fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
            BlockTone::ToolCall => s.fg(Color::Yellow),
            BlockTone::ToolResult => s.fg(Color::Gray),
            BlockTone::ToolError => s.fg(Color::LightRed),
            BlockTone::ErrorWarning => s.fg(Color::Yellow),
            BlockTone::ErrorSeverity => s.fg(Color::Red),
            BlockTone::ErrorFatal => s.fg(Color::Red).add_modifier(Modifier::BOLD),
            BlockTone::DriftPush => s.fg(Color::Magenta),
            BlockTone::DriftPull => s.fg(Color::LightMagenta),
            BlockTone::DriftMerge => s.fg(Color::Blue),
            BlockTone::Notification => s.fg(Color::LightBlue),
            BlockTone::Resource => s.fg(Color::LightGreen),
            BlockTone::Foreground => s.fg(Color::Reset),
            BlockTone::Dim => self.dim(),
        }
    }

    /// The style a markdown span renders in, layered over the block's own
    /// style by [`render_block`].
    pub fn span(&self, tone: SpanTone) -> Style {
        let s = Style::new();
        match tone {
            SpanTone::Heading => s.add_modifier(Modifier::BOLD).fg(Color::LightYellow),
            SpanTone::CodeBlock => s.fg(Color::LightCyan),
            SpanTone::Code => s.fg(Color::LightCyan),
            SpanTone::Strong => s.add_modifier(Modifier::BOLD),
            SpanTone::Plain => s,
        }
    }

    /// The role divider's rule and stamp.
    pub fn divider(&self) -> Style {
        self.dim()
    }

    /// The status line.
    pub fn status(&self) -> Style {
        Style::new().fg(Color::Blue)
    }

    /// A status-line figure that needs attention (a cache past 80% of its
    /// TTL, a pending ask, a disconnected kernel).
    pub fn warning(&self) -> Style {
        Style::new().fg(Color::Yellow)
    }

    /// A status-line figure that is past its limit.
    pub fn alarm(&self) -> Style {
        Style::new().fg(Color::Red)
    }

    /// The compose line's own text.
    pub fn compose(&self) -> Style {
        Style::new().fg(Color::Reset)
    }

    fn dim(&self) -> Style {
        if self.light_ground {
            Style::new().fg(Color::Gray)
        } else {
            Style::new().fg(Color::DarkGray)
        }
    }

    // ── the alternate screen: editor and diff (docs/tui.md, ruling 1) ───────

    /// Buffer text in the editor.
    pub fn editor_text(&self) -> Style {
        Style::new().fg(Color::Reset)
    }

    /// vi's `~` marker for a row past the end of the buffer.
    pub fn editor_filler(&self) -> Style {
        self.dim()
    }

    /// The editor's mode line: path, dirty marker, mode label, position.
    pub fn editor_status(&self) -> Style {
        Style::new()
            .fg(Color::Black)
            .bg(Color::Gray)
            .add_modifier(Modifier::BOLD)
    }

    /// The `:`-line while command mode is active.
    pub fn editor_command(&self) -> Style {
        Style::new().fg(Color::Reset)
    }

    /// The editor's transient message line (vim `E492`).
    pub fn editor_message(&self) -> Style {
        Style::new().fg(Color::Yellow)
    }

    /// A diff's file heading.
    pub fn diff_header(&self) -> Style {
        Style::new()
            .fg(Color::LightYellow)
            .add_modifier(Modifier::BOLD)
    }

    /// A diff's `@@` hunk header.
    pub fn diff_hunk(&self) -> Style {
        Style::new().fg(Color::Cyan)
    }

    /// An inserted line's band.
    pub fn diff_insert(&self) -> Style {
        Style::new().fg(Color::Green)
    }

    /// A deleted line's band.
    pub fn diff_delete(&self) -> Style {
        Style::new().fg(Color::Red)
    }

    /// An unchanged line.
    pub fn diff_context(&self) -> Style {
        Style::new().fg(Color::Reset)
    }

    /// The changed words inside an inserted line, over its band.
    pub fn diff_word_insert(&self) -> Style {
        Style::new()
            .fg(Color::LightGreen)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    }

    /// The changed words inside a deleted line, over its band.
    pub fn diff_word_delete(&self) -> Style {
        Style::new()
            .fg(Color::LightRed)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    }

    /// Copy mode's search: the line the cursor sits on when it holds the
    /// active match. Layered over the line's own styled spans (`patch`), so
    /// the text keeps its color and only gains the reverse.
    pub fn copy_match(&self) -> Style {
        Style::new().add_modifier(Modifier::REVERSED)
    }

    /// Copy mode's `v` linewise selection, before `y` yanks it.
    pub fn copy_selection(&self) -> Style {
        Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
    }
}

/// What a block needs from outside itself to render.
#[derive(Debug, Clone, Copy)]
pub struct BlockView<'a> {
    /// Who is speaking, for the role divider — a principal's display name,
    /// the model, or the tool. Resolved by the caller, which is the only
    /// side that knows the identity and the cast.
    pub speaker: &'a str,
    /// The context's rc bucket, for the role divider.
    pub context_type: &'a str,
    /// The block's wallclock, already formatted (`14:02:11`). The clock
    /// lives at the edge so this module stays pure.
    pub stamp: &'a str,
    /// Draw the role divider above this block. A caller suppresses it when
    /// the previous printed block had the same speaker.
    pub show_divider: bool,
    /// Render collapsed. Kernel state (`CollapsedChanged`) after first
    /// arrival; [`collapses_by_default`] supplies the first-arrival value.
    pub collapsed: bool,
    /// The context being rendered, for drift push/pull direction.
    pub local_ctx: Option<ContextId>,
}

/// Whether a block of this kind renders collapsed the first time it is seen.
///
/// Only `Error`. Tool calls and results print whole: the transcript lives
/// in scrollback and is never redrawn, so a collapsed result is one nobody
/// can open — reading a long one is copy mode's job (`docs/tui.md`,
/// guidance 7). The kernel's `collapsed` field has no per-kind default, so
/// the default is the client's to apply — once, on first arrival, then
/// carried forward per block id so a later change is not wiped by the next
/// redraw.
pub fn collapses_by_default(kind: BlockKind) -> bool {
    matches!(kind, BlockKind::Error)
}

/// A block's wrap-cache version: every field whose change alters the
/// rendered text.
///
/// Streaming appends move `content`, a completing tool call moves `status`,
/// an expand moves `collapsed`, and an edit moves `updated_at`. A version
/// that missed one of those would serve a stale wrap forever.
pub fn block_version(block: &BlockSnapshot, collapsed: bool) -> u64 {
    let mut v = block.updated_at;
    v = v.wrapping_mul(31).wrapping_add(block.content.len() as u64);
    v = v.wrapping_mul(31).wrapping_add(block.status as u64);
    v = v.wrapping_mul(31).wrapping_add(u64::from(collapsed));
    v = v.wrapping_mul(31).wrapping_add(u64::from(block.excluded));
    v = v.wrapping_mul(31).wrapping_add(u64::from(block.is_error));
    v
}

/// The role divider: `─ claude · coder ────────────────── 14:02:11`.
///
/// One ruled line, never a box: a selection of the text under it pastes
/// clean.
pub fn divider_line(view: &BlockView<'_>, width: u16, palette: &Palette) -> Line<'static> {
    let width = usize::from(width.max(1));
    let head = format!("─ {} · {} ", view.speaker, view.context_type);
    let tail = format!(" {}", view.stamp);
    let head_w = display_width(&head);
    let tail_w = display_width(&tail);
    let rule = width.saturating_sub(head_w + tail_w);
    let text = if rule == 0 {
        // Too narrow for the stamp — the identity is what must survive.
        truncate(&head, width)
    } else {
        format!("{head}{}{tail}", "─".repeat(rule))
    };
    Line::from(Span::styled(text, palette.divider()))
}

/// One block as styled lines, wrapped to `width`.
///
/// A collapsed `ToolCall`/`ToolResult` is one `▸` line; a collapsed
/// `Thinking` block is one `▸ thinking · N lines · …` line; an `Error`
/// block is one `✗` stub line; `Thinking` renders dim through its tone.
/// Everything else is the block's formatted text, word-wrapped, flush-left.
pub fn render_block(
    block: &BlockSnapshot,
    view: &BlockView<'_>,
    width: u16,
    palette: &Palette,
) -> Vec<Line<'static>> {
    let width = width.max(1);
    let base = palette.block(block_tone(block));
    let text = format_single_block(block, view.local_ctx, &|_| None);

    let mut lines = Vec::new();
    if view.show_divider {
        lines.push(divider_line(view, width, palette));
    }

    if block.kind == BlockKind::Error {
        lines.push(stub_line(ERROR_MARK, &text, width, base));
        return lines;
    }
    if view.collapsed {
        if block.kind == BlockKind::Thinking {
            // The raw content, not `text`: the formatter's `Thinking` header
            // line would otherwise be the stub's "first line".
            lines.push(thinking_stub_line(block.content.trim(), width, base));
        } else {
            lines.push(stub_line(COLLAPSED_MARK, &text, width, base));
        }
        return lines;
    }

    // A tool result with structured output lays out at the real width
    // (columns, a table, a tree) instead of `format_output_data`'s
    // no-width fallback — kaish owns the data, the tui owns the layout
    // (`docs/tui.md`, guidance 7). `layout_output` carries no color of its
    // own besides an entry-type hint, so the block's base tone is patched
    // underneath every span, the same way markdown span tones layer over it
    // below.
    if block.kind == BlockKind::ToolResult
        && let Some(output) = block.output.as_ref()
        && let Some(body) = layout_output(output, width, palette)
    {
        lines.extend(body.into_iter().map(|line| {
            let spans = line
                .spans
                .into_iter()
                .map(|s| Span::styled(s.content, base.patch(s.style)))
                .collect::<Vec<_>>();
            Line::from(spans)
        }));
        if let Some(stderr) = block.stderr.as_deref() {
            let stderr = stderr.trim();
            if !stderr.is_empty() {
                lines.extend(wrap_styled(&[(base, stderr.to_string())], width));
            }
        }
        return lines;
    }

    // Markdown is a model's own prose. A tool's stdout is not markdown, and
    // parsing it would eat the very backticks and asterisks a paste needs.
    // Its own color, if any, comes from `style_spans` instead — the ANSI the
    // kernel already stripped at ingestion (`ansi_segments`).
    let styled: Vec<(Style, String)> = if block.kind == BlockKind::Text && block.role == Role::Model
    {
        parse_to_rich_spans(&text)
            .into_iter()
            .map(|span| {
                let mut style = base.patch(palette.span(span.tone()));
                if span.italic {
                    style = style.add_modifier(Modifier::ITALIC);
                }
                (style, span.text)
            })
            .collect()
    } else if let Some(segments) = ansi_segments(block, base) {
        segments
    } else {
        vec![(base, text)]
    };

    lines.extend(wrap_styled(&styled, width));
    lines
}

/// A kernel-side [`StyleColor`] as a ratatui color. Indexed stays semantic —
/// the terminal's own palette slot, themeable by the terminal, not us —
/// truecolor rides verbatim.
fn ansi_color(color: StyleColor) -> Color {
    match color {
        StyleColor::Indexed(n) => Color::Indexed(n),
        StyleColor::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// One [`StyleSpan`]'s style, layered over the block's own tone. Every
/// attribute the kernel's `ansi-strip` transform records
/// (`crates/kaijutsu-ansi`) maps onto a ratatui [`Modifier`] bit but italic
/// and blink, which ratatui's own doc notes most terminals render as bold or
/// not at all — carried anyway so a terminal that does support them shows
/// them, and dropped by no terminal's fault of ours if it doesn't.
fn ansi_span_style(span: &StyleSpan, base: Style) -> Style {
    let mut style = base;
    if let Some(fg) = span.fg {
        style = style.fg(ansi_color(fg));
    }
    if let Some(bg) = span.bg {
        style = style.bg(ansi_color(bg));
    }
    if span.attrs.contains(StyleAttrs::BOLD) {
        style = style.add_modifier(Modifier::BOLD);
    }
    if span.attrs.contains(StyleAttrs::DIM) {
        style = style.add_modifier(Modifier::DIM);
    }
    if span.attrs.contains(StyleAttrs::ITALIC) {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if span.attrs.contains(StyleAttrs::UNDERLINE) {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if span.attrs.contains(StyleAttrs::BLINK) {
        style = style.add_modifier(Modifier::SLOW_BLINK);
    }
    if span.attrs.contains(StyleAttrs::INVERSE) {
        style = style.add_modifier(Modifier::REVERSED);
    }
    if span.attrs.contains(StyleAttrs::STRIKETHROUGH) {
        style = style.add_modifier(Modifier::CROSSED_OUT);
    }
    style
}

/// A tool block's own `style_spans`, sliced against its content, when the
/// spans' byte offsets are still meaningful against the text this module
/// renders.
///
/// The kernel's `ansi-strip` ingest transform (`crates/kaijutsu-ansi`) turns
/// escape bytes into clean text plus this exact span map at ingestion — there
/// is no escape byte left to strip here, only the already-parsed spans to
/// paint. Offsets are bytes into `BlockSnapshot::content`
/// (`kaijutsu_types::StyleSpan`, "Byte offset of span start in `content`"),
/// so this only fires on the plain-content path: a `ToolResult` with
/// structured `output` renders from that instead
/// (`kaijutsu_present::format::format_block_inner`), and a span's offset
/// means nothing against a different string.
fn ansi_segments(block: &BlockSnapshot, base: Style) -> Option<Vec<(Style, String)>> {
    if block.style_spans.is_empty() || block.output.is_some() {
        return None;
    }
    // Matches `format_single_block`'s own trailing trim (the leading edge is
    // left alone, so every span offset — measured from byte 0 of `content` —
    // still lands on the text this slices).
    let content = block.content.trim_end();
    let mut segments = Vec::new();
    let mut cursor = 0usize;
    for span in &block.style_spans {
        let start = (span.start as usize).min(content.len());
        let end = (span.end as usize).min(content.len());
        if end <= start {
            continue;
        }
        if start > cursor {
            segments.push((base, content[cursor..start].to_string()));
        }
        segments.push((ansi_span_style(span, base), content[start..end].to_string()));
        cursor = end;
    }
    if cursor < content.len() {
        segments.push((base, content[cursor..].to_string()));
    }
    // `format_block_inner` appends stderr after the body for a `ToolResult`;
    // spans never cover it (it is a separate field, not part of `content`),
    // so it rides in the block's own tone.
    if let Some(stderr) = block.stderr.as_deref() {
        let stderr = stderr.trim();
        if !stderr.is_empty() {
            segments.push((base, format!("\n{stderr}")));
        }
    }
    Some(segments)
}

/// A one-line stub: `▸ cargo test -p kaijutsu-kernel vfs::`, cut with `…`
/// rather than wrapped.
fn stub_line(mark: &str, text: &str, width: u16, style: Style) -> Line<'static> {
    let first = text.lines().next().unwrap_or("").trim_end();
    let body = truncate(
        &format!("{mark} {first}"),
        usize::from(width),
    );
    Line::from(Span::styled(body, style))
}

/// The thinking pane's stub: `▸ thinking · 14 lines · The unlink bug…`,
/// cut with `…` rather than wrapped. The size says how much reasoning the
/// stub stands for; the first line says what it was about.
fn thinking_stub_line(text: &str, width: u16, style: Style) -> Line<'static> {
    let count = text.lines().count();
    let noun = if count == 1 { "line" } else { "lines" };
    let first = text.lines().next().unwrap_or("").trim_end();
    let body = truncate(
        &format!("{COLLAPSED_MARK} thinking · {count} {noun} · {first}"),
        usize::from(width),
    );
    Line::from(Span::styled(body, style))
}

/// Greedy word wrap over a styled character stream.
///
/// Breaks at the last space that fits; a word longer than `width` is cut at
/// the column rather than pushed off the edge. Blank source lines survive as
/// blank output lines, because a model's paragraph breaks are meaning.
fn wrap_styled(segments: &[(Style, String)], width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut out = Vec::new();
    let mut cur: Vec<(char, Style)> = Vec::new();
    let mut cur_w = 0usize;
    let mut last_space: Option<usize> = None;

    for (style, text) in segments {
        for ch in text.chars() {
            if ch == '\n' {
                out.push(coalesce(std::mem::take(&mut cur)));
                cur_w = 0;
                last_space = None;
                continue;
            }
            let w = ch.width().unwrap_or(0);
            if cur_w + w > width && !cur.is_empty() {
                match last_space {
                    Some(i) if i > 0 => {
                        let rest: Vec<(char, Style)> = cur
                            .split_off(i)
                            .into_iter()
                            .skip_while(|(c, _)| *c == ' ')
                            .collect();
                        out.push(coalesce(std::mem::take(&mut cur)));
                        cur_w = rest.iter().map(|(c, _)| c.width().unwrap_or(0)).sum();
                        cur = rest;
                    }
                    _ => {
                        out.push(coalesce(std::mem::take(&mut cur)));
                        cur_w = 0;
                    }
                }
                last_space = None;
            }
            if ch == ' ' {
                last_space = Some(cur.len());
            }
            cur.push((ch, *style));
            cur_w += w;
        }
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(coalesce(cur));
    }
    out
}

/// Merge runs of equal style into spans.
fn coalesce(chars: Vec<(char, Style)>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_style: Option<Style> = None;
    for (ch, style) in chars {
        match run_style {
            Some(s) if s == style => run.push(ch),
            Some(s) => {
                spans.push(Span::styled(std::mem::take(&mut run), s));
                run.push(ch);
                run_style = Some(style);
            }
            None => {
                run.push(ch);
                run_style = Some(style);
            }
        }
    }
    if let Some(s) = run_style {
        spans.push(Span::styled(run, s));
    }
    Line::from(spans)
}

fn display_width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Cut to `width` columns, marking the cut with `…` when anything was lost.
fn truncate(s: &str, width: usize) -> String {
    if display_width(s) <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > width.saturating_sub(1) {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// Wrapped lines per block, keyed `(block id, version, width)`.
///
/// A streaming block is re-wrapped on every append; a settled one is wrapped
/// once and then handed back. `docs/tui.md`, "What is reused, what is new".
#[derive(Debug, Default)]
pub struct WrapCache {
    entries: HashMap<BlockId, Entry>,
}

#[derive(Debug)]
struct Entry {
    version: u64,
    width: u16,
    lines: Vec<Line<'static>>,
}

impl WrapCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The block's wrapped lines, re-wrapping only when the version or the
    /// width moved.
    pub fn lines(
        &mut self,
        block: &BlockSnapshot,
        view: &BlockView<'_>,
        width: u16,
        palette: &Palette,
    ) -> &[Line<'static>] {
        let version = block_version(block, view.collapsed);
        let entry = self.entries.entry(block.id).or_insert_with(|| Entry {
            version,
            width,
            lines: render_block(block, view, width, palette),
        });
        if entry.version != version || entry.width != width {
            entry.version = version;
            entry.width = width;
            entry.lines = render_block(block, view, width, palette);
        }
        &entry.lines
    }

    /// Drop one block's wrap — a deleted block, or one that has left the
    /// live region for scrollback and will never be redrawn.
    pub fn forget(&mut self, id: &BlockId) {
        self.entries.remove(id);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{BlockSnapshotBuilder, PrincipalId, Status};

    fn view<'a>() -> BlockView<'a> {
        BlockView {
            speaker: "claude",
            context_type: "coder",
            stamp: "14:02:11",
            show_divider: false,
            collapsed: false,
            local_ctx: None,
        }
    }

    fn block(kind: BlockKind, role: Role, content: &str) -> BlockSnapshot {
        let id = BlockId::new(ContextId::new(), PrincipalId::new(), 1);
        BlockSnapshotBuilder::new(id, kind)
            .role(role)
            .content(content)
            .build()
    }

    fn plain(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    /// The exhaustiveness guard the palette exists for: a tone added to
    /// `kaijutsu-present` must fail the client's match, not paint a
    /// placeholder. `BlockTone::all()` is derived from the enum, so this
    /// test cannot fall behind a new variant.
    #[test]
    fn every_block_tone_resolves_to_a_style() {
        let palette = Palette::builtin();
        for tone in BlockTone::all() {
            let style = palette.block(tone);
            assert!(
                style.fg.is_some() || style.add_modifier != Modifier::empty(),
                "{tone:?} resolved to an empty style"
            );
        }
    }

    #[test]
    fn every_span_tone_resolves_to_a_style() {
        let palette = Palette::builtin();
        // Plain is deliberately the unstyled one — it inherits the block's
        // own style — so it is the single exception to "carries something".
        for tone in [
            SpanTone::Heading,
            SpanTone::CodeBlock,
            SpanTone::Code,
            SpanTone::Strong,
        ] {
            let style = palette.span(tone);
            assert!(
                style.fg.is_some() || style.add_modifier != Modifier::empty(),
                "{tone:?} resolved to an empty style"
            );
        }
        assert_eq!(palette.span(SpanTone::Plain), Style::new());
    }

    #[test]
    fn the_divider_names_speaker_context_type_and_wallclock() {
        let line = divider_line(&view(), 60, &Palette::builtin());
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text.chars().count(), 60, "the rule fills the width");
        assert!(text.starts_with("─ claude · coder ─"), "got {text:?}");
        assert!(text.ends_with(" 14:02:11"), "got {text:?}");
    }

    #[test]
    fn a_narrow_divider_keeps_the_identity_over_the_stamp() {
        let line = divider_line(&view(), 12, &Palette::builtin());
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("claude"), "got {text:?}");
        assert!(!text.contains("14:02:11"), "got {text:?}");
    }

    /// Tool output prints whole: scrollback can never be redrawn, so a
    /// collapsed result is one nobody can open. Only `Error` keeps its
    /// one-line stub.
    #[test]
    fn only_errors_collapse_by_default() {
        assert!(!collapses_by_default(BlockKind::ToolCall));
        assert!(!collapses_by_default(BlockKind::ToolResult));
        assert!(collapses_by_default(BlockKind::Error));
        assert!(!collapses_by_default(BlockKind::Text));
        assert!(!collapses_by_default(BlockKind::Thinking));
    }

    #[test]
    fn a_collapsed_block_renders_one_marked_line() {
        let mut b = block(BlockKind::ToolResult, Role::Tool, "line one\nline two\nline three");
        b.status = Status::Done;
        let mut v = view();
        v.collapsed = true;
        let lines = render_block(&b, &v, 60, &Palette::builtin());
        assert_eq!(lines.len(), 1);
        assert!(plain(&lines)[0].starts_with("▸ "), "got {:?}", plain(&lines));
    }

    #[test]
    fn an_error_block_is_a_one_line_stub_even_when_expanded() {
        let mut b = block(BlockKind::Error, Role::System, "boom");
        b.error = Some(kaijutsu_types::ErrorPayload {
            category: kaijutsu_types::ErrorCategory::Tool,
            severity: kaijutsu_types::ErrorSeverity::Error,
            code: None,
            detail: Some(
                "a long detail\nspread over several lines\nthat would otherwise wrap".to_string(),
            ),
            span: None,
            source_kind: None,
        });
        let mut v = view();
        v.collapsed = false;
        let lines = render_block(&b, &v, 60, &Palette::builtin());
        assert_eq!(lines.len(), 1, "got {:?}", plain(&lines));
        assert!(plain(&lines)[0].starts_with("✗ "), "got {:?}", plain(&lines));
    }

    #[test]
    fn thinking_renders_dim_and_italic() {
        let b = block(BlockKind::Thinking, Role::Model, "considering");
        let lines = render_block(&b, &view(), 60, &Palette::builtin());
        let style = lines[0].spans[0].style;
        assert!(style.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(style.fg, Some(Color::DarkGray));
    }

    #[test]
    fn body_text_wraps_at_the_last_space_that_fits() {
        let b = block(BlockKind::Text, Role::User, "alpha beta gamma delta");
        let lines = render_block(&b, &view(), 12, &Palette::builtin());
        assert_eq!(plain(&lines), vec!["alpha beta", "gamma delta"]);
    }

    #[test]
    fn a_word_longer_than_the_width_is_cut_at_the_column() {
        let b = block(BlockKind::Text, Role::User, "abcdefghijklmno");
        let lines = render_block(&b, &view(), 5, &Palette::builtin());
        assert_eq!(plain(&lines), vec!["abcde", "fghij", "klmno"]);
    }

    #[test]
    fn a_blank_source_line_survives_as_a_blank_output_line() {
        let b = block(BlockKind::Text, Role::User, "one\n\ntwo");
        let lines = render_block(&b, &view(), 20, &Palette::builtin());
        assert_eq!(plain(&lines), vec!["one", "", "two"]);
    }

    #[test]
    fn wide_characters_count_two_columns() {
        let b = block(BlockKind::Text, Role::User, "日本語のテスト");
        let lines = render_block(&b, &view(), 6, &Palette::builtin());
        for line in plain(&lines) {
            let w: usize = line.chars().map(|c| c.width().unwrap_or(0)).sum();
            assert!(w <= 6, "line {line:?} is {w} columns wide");
        }
    }

    /// Emoji are wide too, and a run of them must not be split mid-glyph the
    /// way the byte-oriented `truncate`/wrap path could if it counted bytes
    /// instead of display columns.
    #[test]
    fn emoji_count_two_columns_and_survive_narrow_wrap() {
        let b = block(BlockKind::Text, Role::User, "🎺🎺🎺🎺 rest");
        let lines = render_block(&b, &view(), 6, &Palette::builtin());
        for line in plain(&lines) {
            let w: usize = line.chars().map(|c| c.width().unwrap_or(0)).sum();
            assert!(w <= 6, "line {line:?} is {w} columns wide");
        }
        let joined = plain(&lines).join("");
        assert_eq!(joined.chars().filter(|c| *c == '🎺').count(), 4, "no glyph lost");
    }

    /// Nothing pasteable is inside a box: a tool result's text starts at
    /// column zero with no border glyph in front of it.
    #[test]
    fn tool_output_renders_flush_left_with_no_border_glyphs() {
        let mut b = block(BlockKind::ToolResult, Role::Tool, "/home/atobey/src/kaijutsu");
        b.status = Status::Done;
        let mut v = view();
        v.collapsed = false;
        let lines = render_block(&b, &v, 60, &Palette::builtin());
        assert_eq!(plain(&lines), vec!["/home/atobey/src/kaijutsu"]);
    }

    #[test]
    fn a_model_reply_gets_markdown_span_tones() {
        let b = block(BlockKind::Text, Role::Model, "plain and **bold**");
        let lines = render_block(&b, &view(), 60, &Palette::builtin());
        let bold = lines[0]
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::BOLD));
        assert!(bold, "markdown emphasis reached the span style");
    }

    /// A tool's stdout is not markdown; parsing it would eat the backticks
    /// and asterisks a paste needs.
    #[test]
    fn tool_output_is_not_parsed_as_markdown() {
        let mut b = block(BlockKind::ToolResult, Role::Tool, "**not bold** `not code`");
        b.status = Status::Done;
        let mut v = view();
        v.collapsed = false;
        let lines = render_block(&b, &v, 60, &Palette::builtin());
        assert_eq!(plain(&lines), vec!["**not bold** `not code`"]);
    }

    /// The shape `kaijutsu_ansi::strip` actually produces for `a\x1b[31mred\x1b[0mb`
    /// (crates/kaijutsu-ansi/src/lib.rs, `basic_color_span`): clean text plus
    /// one span over the styled run. The TUI never sees the escape bytes —
    /// only this projection — so this is what it has to paint from.
    #[test]
    fn ansi_style_spans_color_a_tool_results_text() {
        let mut b = block(BlockKind::ToolResult, Role::Tool, "aredb");
        b.status = Status::Done;
        b.style_spans = vec![StyleSpan {
            start: 1,
            end: 4,
            fg: Some(StyleColor::Indexed(1)),
            bg: None,
            attrs: StyleAttrs::default(),
        }];
        let mut v = view();
        v.collapsed = false;
        let lines = render_block(&b, &v, 60, &Palette::builtin());
        assert_eq!(lines.len(), 1);
        let spans = &lines[0].spans;
        let text: Vec<&str> = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, vec!["a", "red", "b"], "got {text:?}");
        assert_eq!(spans[1].style.fg, Some(Color::Indexed(1)));
        assert_eq!(
            spans[0].style.fg,
            Some(Color::Gray),
            "the unstyled runs keep the block's own tone (BlockTone::ToolResult)"
        );
    }

    /// Bold, underline and truecolor all land on the ratatui `Style` the
    /// kernel's `StyleAttrs`/`StyleColor` describe.
    #[test]
    fn ansi_attributes_and_truecolor_map_onto_ratatui_modifiers() {
        let mut b = block(BlockKind::ToolResult, Role::Tool, "warn");
        b.status = Status::Done;
        b.style_spans = vec![StyleSpan {
            start: 0,
            end: 4,
            fg: Some(StyleColor::Rgb(255, 200, 0)),
            bg: None,
            attrs: StyleAttrs::BOLD | StyleAttrs::UNDERLINE,
        }];
        let mut v = view();
        v.collapsed = false;
        let lines = render_block(&b, &v, 60, &Palette::builtin());
        let style = lines[0].spans[0].style;
        assert_eq!(style.fg, Some(Color::Rgb(255, 200, 0)));
        assert!(style.add_modifier.contains(Modifier::BOLD));
        assert!(style.add_modifier.contains(Modifier::UNDERLINED));
    }

    /// A `ToolResult` with structured `output` renders from that, not from
    /// `content` (`format_block_inner`), so a span's offset — always measured
    /// against `content` — means nothing there and must not be applied.
    #[test]
    fn style_spans_are_ignored_when_output_data_is_present() {
        let mut b = block(BlockKind::ToolResult, Role::Tool, "aredb");
        b.status = Status::Done;
        b.style_spans = vec![StyleSpan {
            start: 1,
            end: 4,
            fg: Some(StyleColor::Indexed(1)),
            bg: None,
            attrs: StyleAttrs::default(),
        }];
        b.output = Some(kaijutsu_types::OutputData::text("aredb"));
        let mut v = view();
        v.collapsed = false;
        let lines = render_block(&b, &v, 60, &Palette::builtin());
        assert!(
            lines[0]
                .spans
                .iter()
                .all(|s| s.style.fg == Some(Color::Gray)),
            "output data has no span offsets to honor, so every run keeps the plain block tone: {lines:?}"
        );
    }

    /// stderr rides after the body in the block's own tone — spans cover
    /// `content` only, never the separate `stderr` field.
    #[test]
    fn stderr_appends_after_styled_content_in_the_blocks_own_tone() {
        let mut b = block(BlockKind::ToolResult, Role::Tool, "ok");
        b.status = Status::Done;
        b.style_spans = vec![StyleSpan {
            start: 0,
            end: 2,
            fg: Some(StyleColor::Indexed(2)),
            bg: None,
            attrs: StyleAttrs::default(),
        }];
        b.stderr = Some("warning: deprecated".to_string());
        let mut v = view();
        v.collapsed = false;
        let lines = render_block(&b, &v, 60, &Palette::builtin());
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("warning: deprecated"), "got {text:?}");
    }

    /// An `ls`-shaped `ToolResult` renders through `layout::layout_output`
    /// instead of `format_output_data`'s one-name-per-line fallback: at a
    /// wide-enough terminal, several short names share a row.
    #[test]
    fn a_tool_result_with_structured_output_lays_out_in_columns() {
        let mut b = block(BlockKind::ToolResult, Role::Tool, "");
        b.status = Status::Done;
        b.output = Some(kaijutsu_types::OutputData::nodes(
            (0..12)
                .map(|i| kaijutsu_types::OutputNode::new(format!("f{i}")))
                .collect(),
        ));
        let mut v = view();
        v.collapsed = false;
        let lines = render_block(&b, &v, 40, &Palette::builtin());
        assert!(
            lines.len() < 12,
            "12 short names at width 40 should share rows, got {} lines: {:?}",
            lines.len(),
            plain(&lines)
        );
        assert!(plain(&lines)[0].contains("f0") && plain(&lines)[0].contains("f1"));
    }

    #[test]
    fn the_wrap_cache_reuses_a_settled_block_and_rewraps_a_grown_one() {
        let palette = Palette::builtin();
        let mut cache = WrapCache::new();
        let mut b = block(BlockKind::Text, Role::User, "alpha beta");
        let v = view();

        let first = cache.lines(&b, &v, 20, &palette).to_vec();
        assert_eq!(cache.lines(&b, &v, 20, &palette), first.as_slice());
        assert_eq!(cache.len(), 1);

        b.content.push_str(" gamma");
        let grown = cache.lines(&b, &v, 20, &palette);
        assert_ne!(grown, first.as_slice(), "an append re-wraps");
    }

    #[test]
    fn the_wrap_cache_rewraps_on_a_width_change() {
        let palette = Palette::builtin();
        let mut cache = WrapCache::new();
        let b = block(BlockKind::Text, Role::User, "alpha beta gamma");
        let v = view();
        let wide = cache.lines(&b, &v, 40, &palette).len();
        let narrow = cache.lines(&b, &v, 12, &palette).len();
        assert_eq!(wide, 1);
        assert!(narrow > 1, "a narrower width wraps to more lines");
    }

    #[test]
    fn the_wrap_cache_rewraps_when_a_block_collapses() {
        let palette = Palette::builtin();
        let mut cache = WrapCache::new();
        let mut b = block(BlockKind::ToolResult, Role::Tool, "one\ntwo\nthree");
        b.status = Status::Done;
        let mut v = view();
        v.collapsed = false;
        let expanded = cache.lines(&b, &v, 40, &palette).len();
        v.collapsed = true;
        let collapsed = cache.lines(&b, &v, 40, &palette).len();
        assert_eq!(expanded, 3);
        assert_eq!(collapsed, 1);
    }

    #[test]
    fn forgetting_a_block_drops_its_wrap() {
        let palette = Palette::builtin();
        let mut cache = WrapCache::new();
        let b = block(BlockKind::Text, Role::User, "alpha");
        let _ = cache.lines(&b, &view(), 20, &palette);
        cache.forget(&b.id);
        assert!(cache.is_empty());
    }
}
