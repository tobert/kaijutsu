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
use kaijutsu_types::{BlockId, BlockKind, BlockSnapshot, ContextId, Role};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

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
/// The kernel's `collapsed` field has no per-kind default, so the default is
/// the client's to apply — once, on first arrival, then carried forward per
/// block id so a later expand is not wiped by the next redraw.
pub fn collapses_by_default(kind: BlockKind) -> bool {
    matches!(
        kind,
        BlockKind::ToolCall | BlockKind::ToolResult | BlockKind::Error
    )
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
/// A collapsed `ToolCall`/`ToolResult` is one `▸` line; an `Error` block is
/// one `✗` stub line; `Thinking` renders dim through its tone. Everything
/// else is the block's formatted text, word-wrapped, flush-left.
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
        lines.push(stub_line(COLLAPSED_MARK, &text, width, base));
        return lines;
    }

    // Markdown is a model's own prose. A tool's stdout is not markdown, and
    // parsing it would eat the very backticks and asterisks a paste needs.
    let styled: Vec<(Style, String)> =
        if block.kind == BlockKind::Text && block.role == Role::Model {
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
        } else {
            vec![(base, text)]
        };

    lines.extend(wrap_styled(&styled, width));
    lines
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

    #[test]
    fn tool_calls_and_results_and_errors_collapse_by_default() {
        assert!(collapses_by_default(BlockKind::ToolCall));
        assert!(collapses_by_default(BlockKind::ToolResult));
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
