//! Rich content rendering via Parley + Vello.
//!
//! Supports multiple content formats via `RichContentKind`:
//! - **Markdown**: per-span brush coloring (headings, code, bold, etc.)
//! - **Sparkline**: inline timeseries mini-charts — plain Bevy UI rectangle
//!   geometry now, not Vello (see `text::sparkline`); only detection and the
//!   `SparklineData` payload live here.
//! - **SVG**: inline vector graphics, CPU-rasterized via `usvg` + `resvg` +
//!   `tiny-skia` (`text::svg_raster`) to a Bevy `Image`/`ImageNode` — no vello
//! - **Diff**: a collapsed unified-diff preview — diffstat header plus the
//!   first N lines, semantically colored from the theme (`text::diff` builds
//!   the preview; this module only maps its classes to brushes)
//!
//! Detection is centralized in `detect_rich_content_typed()` — tries sparkline
//! first (more specific fence pattern), then SVG, then a ` ```diff ` fence,
//! then falls back to markdown.

use bevy::prelude::*;
use peniko::Brush;

use std::sync::Arc;

use kaijutsu_types::{BlockSnapshot, ContentType, OutputData, OutputEntryType, OutputNode};

use super::components::{bevy_color_to_brush, color_to_rgba8 as rgba8};
use super::markdown::{MarkdownColors, RichSpan, parse_to_rich_spans};
use super::sparkline::{
    SparklineData, try_parse_sparkline,
};

use crate::view::format::{OutputLayout, compute_output_layout, format_output_data};

/// Per-span brush mapping: byte range → Brush.
///
/// `Clone`/`Debug` so the conversation surface can cache spans alongside the
/// formatted text they describe and slice them per shaping chunk
/// (`view::surface::chunk::slice_spans`).
#[derive(Clone, Debug)]
pub struct SpanBrush {
    /// Byte offset of span start in the concatenated plain text.
    pub start: usize,
    /// Byte offset of span end.
    pub end: usize,
    /// Brush for this span.
    pub brush: Brush,
}

/// Rich content for a conversation block — dispatches rendering by format.
///
/// Conversation-only: the compose overlay, shell dock, editor, and
/// diff-view surfaces render plain text and never produce this. The
/// conversation surface (`view::surface::rich`/`shape_cache`) turns it into
/// MSDF glyphs (Markdown, Output), MSDF glyphs + geometry (Abc), a cached
/// CPU raster drawn as a textured quad (Svg, via `text::svg_raster`), or
/// chrome-drawn placeholder geometry (Sparkline, Image).
#[derive(Component)]
pub struct RichContent {
    pub kind: RichContentKind,
}



/// The actual content variant being rendered.
pub enum RichContentKind {
    /// Markdown with per-span brush coloring.
    Markdown {
        spans: Vec<RichSpan>,
        plain_text: String,
    },
    /// Inline timeseries mini-chart — rendered as plain UI rectangle
    /// geometry (`text::sparkline::build_sparkline_geometry`), not Vello.
    Sparkline(SparklineData),
    /// Inline SVG vector graphic — parsed once here, rasterized (and
    /// re-rasterized on a physical-pixel-size change) by
    /// `view::block_render` via `text::svg_raster`.
    Svg {
        /// Pre-parsed usvg tree. `<text>` elements are already resolved to
        /// outlines against whatever `SvgFontDb` was available at parse time.
        tree: Arc<usvg::Tree>,
        /// Original SVG width (for aspect-ratio scaling).
        width: f32,
        /// Original SVG height (for aspect-ratio scaling).
        height: f32,
        /// Raw SVG source, retained for diagnostics / future re-parse.
        #[allow(dead_code)] // Not read yet — kept for error-path diagnostics.
        source: Arc<String>,
    },
    /// ABC music notation — rendered via MSDF glyphs + flat-colored
    /// geometry from engraving IR (see `text::msdf::music_bridge`), not
    /// Vello.
    Abc {
        /// Raw ABC source text.
        #[allow(dead_code)] // Retained for re-parse / source inspection; render uses `tune`.
        source: Arc<String>,
        /// Parsed AST (avoids re-parsing on resize).
        tune: Arc<kaijutsu_abc::Tune>,
    },
    /// A unified diff, previewed inline: diffstat header + the first N lines,
    /// collapsed (`docs/diff.md` Decision 5). Rendered as MSDF glyphs +
    /// `MsdfBlockGeometry` background bands, the same pair ABC uses.
    ///
    /// Always present for a block declaring `ContentType::Diff`, including one
    /// whose content does not parse — that case carries an error preview
    /// (`DiffPreview::is_error`), because content and content-type are
    /// separate LWW registers and a visible error beats an empty cell.
    Diff(Box<super::diff::DiffPreview>),
    /// Structured OutputData with per-cell coloring by EntryType.
    Output {
        /// Pre-computed column→byte mapping for per-cell brushes.
        layout: OutputLayout,
        /// Whitespace-padded measurement text (same as UiVelloText.value).
        plain_text: String,
    },
    /// Raster image stored in CAS by hash. The block text is the 32-char hex hash.
    /// Actual decoding happens in the render pass where Bevy Commands are available.
    Image {
        hash: String,
    },
}

/// Build a `Vec<SpanBrush>` from parsed spans + theme colors.
///
/// Maps each span's byte range to a Brush based on its formatting:
/// - Headings → `md_heading_color`
/// - Code/code blocks → `md_code_fg` / `md_code_block_fg`
/// - Bold → `md_strong_color` or base_color
/// - Plain text → `base_color`
pub fn build_span_brushes(
    spans: &[RichSpan],
    base_color: Color,
    md_colors: &MarkdownColors,
) -> Vec<SpanBrush> {
    let mut result = Vec::with_capacity(spans.len());
    let mut byte_offset = 0usize;

    for span in spans {
        let start = byte_offset;
        let end = start + span.text.len();

        let color = if span.heading_level.is_some() {
            md_colors.heading
        } else if span.code_block {
            md_colors.code_block
        } else if span.code {
            md_colors.code
        } else if span.bold {
            md_colors.strong.unwrap_or(base_color)
        } else {
            base_color
        };

        result.push(SpanBrush {
            start,
            end,
            brush: bevy_color_to_brush(color),
        });

        byte_offset = end;
    }

    result
}

/// Find the brush for a given byte offset in the span mapping.
pub fn brush_at_offset(span_brushes: &[SpanBrush], offset: usize) -> Option<&Brush> {
    // Spans are contiguous and ordered — binary search on start.
    span_brushes
        .iter()
        .find(|sb| offset >= sb.start && offset < sb.end)
        .map(|sb| &sb.brush)
}

/// Map an `OutputEntryType` to a theme color for the name column.
fn entry_type_color(entry_type: OutputEntryType, theme: &crate::ui::theme::Theme) -> Color {
    match entry_type {
        OutputEntryType::Directory => theme.output_directory,
        OutputEntryType::Executable => theme.output_executable,
        OutputEntryType::Symlink => theme.output_symlink,
        OutputEntryType::File | OutputEntryType::Text => theme.block_tool_result,
        // non_exhaustive fallback
        _ => theme.block_tool_result,
    }
}

/// Build `SpanBrush` vec from an `OutputLayout` for per-cell coloring.
///
/// - Header rows → `theme.output_header` for all columns
/// - Data rows: name column (index 0) → `entry_type_color`, others → `theme.block_tool_result`
pub fn build_output_span_brushes(
    layout: &OutputLayout,
    theme: &crate::ui::theme::Theme,
) -> Vec<SpanBrush> {
    let mut result = Vec::new();

    for row in &layout.rows {
        for (col_idx, &(start, end)) in row.col_byte_ranges.iter().enumerate() {
            if start == end {
                continue;
            }
            let color = if row.is_header {
                theme.output_header
            } else if col_idx == 0 {
                entry_type_color(row.entry_type, theme)
            } else {
                theme.block_tool_result
            };
            result.push(SpanBrush {
                start,
                end,
                brush: bevy_color_to_brush(color),
            });
        }
    }

    result
}

/// The theme color for a diff line class.
fn diff_line_color(
    class: super::diff::DiffLineClass,
    theme: &crate::ui::theme::Theme,
) -> Color {
    use super::diff::DiffLineClass;
    match class {
        DiffLineClass::Stat => theme.diff_stat,
        DiffLineClass::FileHeader => theme.diff_file_header,
        DiffLineClass::HunkHeader => theme.diff_hunk_header,
        DiffLineClass::Insert => theme.diff_insert_fg,
        DiffLineClass::Delete => theme.diff_delete_fg,
        DiffLineClass::Context => theme.diff_context_fg,
        DiffLineClass::Meta => theme.diff_meta,
        DiffLineClass::Error => theme.diff_error_fg,
    }
}

/// The emphasis color for a *word* the refinement marked changed inside a
/// line of that class.
///
/// Only the two changed classes have one. A word span on any other class would
/// be a claim the model never makes (`DiffLine::words` is empty on context
/// lines and on anything with no counterpart), so it draws in the line color
/// rather than inventing an emphasis for it.
fn diff_word_color(
    class: super::diff::DiffLineClass,
    theme: &crate::ui::theme::Theme,
) -> Option<Color> {
    use super::diff::DiffLineClass;
    match class {
        DiffLineClass::Insert => Some(theme.diff_insert_word_fg),
        DiffLineClass::Delete => Some(theme.diff_delete_word_fg),
        _ => None,
    }
}

/// Build `SpanBrush` vec for a diff preview — one span per preview line, split
/// around the intra-line word highlights.
///
/// Diff color is **semantic** (`docs/diff.md` Decision 8): the block's content
/// is plain unified text with no escape codes, and every color below comes
/// from the theme. Spans are contiguous by construction (see
/// `text::diff::PreviewLine`), which matters because `collect_msdf_glyphs`
/// resolves a brush from each shaping *run's start byte* — a gap would fall
/// through to the fallback brush mid-line.
///
/// **The output is non-overlapping**: a line carrying word highlights is cut
/// into line-colored and word-colored pieces rather than having the word spans
/// stacked on top. Parley's ranged styles would resolve the overlap by push
/// order and the byte-offset lookup would resolve it by *first match* — two
/// mechanisms with opposite answers. Emitting disjoint spans means both agree,
/// whichever the caller uses.
///
/// Word coloring only reaches the screen through
/// `VelloFont::layout_spanned` + `collect_msdf_glyphs_styled`: the plain
/// `collect_msdf_glyphs` looks a brush up per shaping run and cannot see a
/// span that starts mid-run.
pub fn build_diff_span_brushes(
    preview: &super::diff::DiffPreview,
    theme: &crate::ui::theme::Theme,
) -> Vec<SpanBrush> {
    let mut out = Vec::with_capacity(preview.lines.len() + 2 * preview.words.len());
    let mut words = preview.words.iter().peekable();

    for line in &preview.lines {
        let line_brush = || bevy_color_to_brush(diff_line_color(line.class, theme));
        let mut at = line.start;
        while let Some(word) = words.peek().filter(|w| w.start < line.end) {
            let (start, end) = (word.start.max(at), word.end.min(line.end));
            let brush = diff_word_color(word.class, theme).map(bevy_color_to_brush);
            words.next();
            let Some(brush) = brush else {
                continue;
            };
            if start >= end {
                continue;
            }
            if start > at {
                out.push(SpanBrush { start: at, end: start, brush: line_brush() });
            }
            out.push(SpanBrush { start, end, brush });
            at = end;
        }
        if at < line.end {
            out.push(SpanBrush { start: at, end: line.end, brush: line_brush() });
        }
    }

    out
}

/// Resolve the diff band colors from the theme, as RGBA8 for the geometry
/// vertex format.
pub fn diff_band_colors(theme: &crate::ui::theme::Theme) -> super::diff::DiffBandColors {
    super::diff::DiffBandColors {
        insert: rgba8(theme.diff_insert_bg),
        delete: rgba8(theme.diff_delete_bg),
        error: rgba8(theme.diff_error_bg),
    }
}


/// The background washes behind *changed words*, resolved from the theme.
///
/// The background half of what `diff_word_color` does for the foreground, and
/// it draws on top of the line band — the two alphas add, which is why the
/// theme's word washes are kept low.
pub fn diff_word_colors(theme: &crate::ui::theme::Theme) -> super::diff::DiffWordColors {
    super::diff::DiffWordColors {
        insert: rgba8(theme.diff_insert_word_bg),
        delete: rgba8(theme.diff_delete_word_bg),
    }
}

/// The minimap's palette, resolved from the theme.
///
/// The density bars reuse the *line band* colors at full strength rather than
/// inventing a second green and a second rose: the strip is a compression of
/// the page, so it should be the same diff in miniature.
pub fn diff_minimap_colors(theme: &crate::ui::theme::Theme) -> super::diff::MinimapColors {
    super::diff::MinimapColors {
        rail: rgba8(theme.diff_minimap_rail),
        insert: rgba8(theme.diff_insert_fg),
        delete: rgba8(theme.diff_delete_fg),
        structure: rgba8(theme.diff_hunk_header),
        viewport: rgba8(theme.diff_minimap_viewport),
        // The same violet the block cursor draws in, so the strip's marker and
        // the cursor on the page are visibly the same thing. `cursor_normal`
        // is already linear RGBA for the shader, so it converts directly.
        cursor: [
            (theme.cursor_normal.x.clamp(0.0, 1.0) * 255.0) as u8,
            (theme.cursor_normal.y.clamp(0.0, 1.0) * 255.0) as u8,
            (theme.cursor_normal.z.clamp(0.0, 1.0) * 255.0) as u8,
            (theme.cursor_normal.w.clamp(0.0, 1.0) * 255.0) as u8,
        ],
    }
}

/// Detect rich content from structured OutputData.
///
/// Returns `None` for simple text (no coloring needed).
/// For tabular/tree/list data, returns a `RichContent::Output` with
/// pre-computed layout for per-cell coloring.
pub fn detect_output_content(output: &OutputData) -> Option<RichContent> {
    // A rich_json-ONLY payload (kj's structured `.output` sideband, wired
    // through OutputData::rich_json) has an empty node tree AND no headers —
    // there is nothing here for the tree/table renderer to lay out. Rendering
    // rich_json itself is a deliberate follow-up; for now this must fall
    // through to the plain-text path deterministically rather than rely on
    // the empty-plain-text check below (which happens to cover it, but
    // doesn't say so). Headers alone (an empty-result table that still has
    // columns) is real structure — it must NOT be caught by this guard, or a
    // legitimate headers-only table loses its header rendering.
    if output.root.is_empty() && output.headers.is_none() {
        return None;
    }

    // Simple text gets no rich treatment
    if output.as_text().is_some() {
        return None;
    }

    let plain_text = format_output_data(output);
    if plain_text.is_empty() {
        return None;
    }

    let layout = compute_output_layout(output, &plain_text)?;

    Some(RichContent {
        kind: RichContentKind::Output { layout, plain_text },
    })
}

/// Maximum SVG source size we'll attempt to parse (100KB).
const SVG_MAX_BYTES: usize = 100 * 1024;

/// Soft cap on rendered height for inline SVG and ABC notation, used as the
/// height term in the fit-to-block scale (`scale = min(width_fit, height_fit)`
/// in `block_render`). Because it's the *height* term, it only binds for
/// content that is tall relative to its width — e.g. a 4-part ABC score on
/// four stacked staves — where the old 400px value forced the whole score
/// (notes included) to shrink. Short/wide content is width-bound and never
/// touched this cap.
///
/// Set to the GPU/Vello usable texture dimension (`VELLO_MAX_TEXTURE_DIM`,
/// 8192) so it effectively defers to the real ceiling: the
/// `GpuTextureLimits` clamp + texture-stretch fallback in `block_render`
/// (see tech-debt: tall block texture stretching). Real scores are orders of
/// magnitude shorter than this, so in practice tall multi-staff scores now
/// render at width-fit scale instead of being squeezed.
pub const SVG_MAX_HEIGHT: f32 = 8192.0;

/// Try to extract and parse SVG content from block text.
///
/// Recognizes two patterns:
/// - Raw SVG: text starts with `<svg`
/// - Fenced SVG: ` ```svg\n...\n``` `
///
/// When `svg_fontdb` is provided, SVG `<text>` elements are rendered using
/// the fonts in the database. Without it, text elements are silently dropped.
///
/// `is_streaming` is purely diagnostic (which log message an error takes,
/// see below) — it changes no control flow. Both parse failure and a
/// non-positive intrinsic size return `None` unconditionally either way, so
/// the caller always falls through to the plain-text/markdown path: a
/// malformed or zero-size SVG must render as *something visible* (the raw
/// source), never a silently blank block.
fn try_parse_svg(
    text: &str,
    svg_fontdb: Option<&super::SvgFontDb>,
    is_streaming: bool,
) -> Option<(Arc<usvg::Tree>, f32, f32, Arc<String>)> {
    let svg_str = if text.trim_start().starts_with("<svg") {
        text.trim()
    } else { extract_fenced_block(text, "svg")? };

    if svg_str.len() > SVG_MAX_BYTES {
        return None;
    }

    let options = match svg_fontdb {
        Some(fdb) => fdb.usvg_options(),
        None => usvg::Options::default(),
    };

    match usvg::Tree::from_str(svg_str, &options) {
        Ok(tree) => {
            let size = tree.size();
            if size.width() <= 0.0 || size.height() <= 0.0 {
                // Well-formed SVG (parses fine) but with no drawable area —
                // a bare `<svg width="0" height="0">`, or a viewBox usvg
                // couldn't resolve to a positive size. Left unchecked this
                // reaches `view::block_render`'s `Svg` arm, whose
                // `fit_svg_to_box` rejects a non-positive size too, but by
                // then `detect_rich_content_typed` has already committed to
                // `RichContentKind::Svg` — there's no falling back to plain
                // text from inside that match arm, so the block would
                // render as a silent zero-height blank instead of showing
                // its source. Reject it here instead, at the single point
                // that already owns "this SVG isn't renderable, show the
                // text" for parse failures.
                warn!(
                    "SVG parsed but has non-positive intrinsic size ({}x{}) \
                     — rendering as text instead",
                    size.width(),
                    size.height(),
                );
                return None;
            }
            let source = Arc::new(svg_str.to_string());
            Some((Arc::new(tree), size.width(), size.height(), source))
        }
        Err(e) => {
            if is_streaming {
                // Status::Running: incomplete SVG is expected to fail
                // parsing until the closing fence lands — routine, not a bug.
                warn!("SVG parse failed (mid-stream, expected until the closing fence lands): {e}");
            } else {
                // Status::Done: the kernel should have already validated and
                // attached Error children, so a parse failure here is a real
                // divergence worth investigating, not a streaming artifact.
                warn!("SVG parse failed at rest (not mid-stream — likely a real bug): {e}");
            }
            None
        }
    }
}

/// Extract content from a fenced code block with the given language tag.
pub(crate) fn extract_fenced_block<'a>(text: &'a str, lang: &str) -> Option<&'a str> {
    let fence_start = format!("```{}", lang);
    let trimmed = text.trim();
    if !trimmed.starts_with(&fence_start) {
        return None;
    }
    // Find the end fence
    let after_fence = &trimmed[fence_start.len()..];
    let content_start = after_fence.find('\n')? + 1;
    let content = &after_fence[content_start..];
    let end_idx = content.rfind("```")?;
    let inner = content[..end_idx].trim();
    if inner.is_empty() {
        return None;
    }
    Some(inner)
}

/// Fingerprint of every input the detectors above read for one block.
///
/// Detection is pure over these — there is no cache inside it, so calling it
/// twice with the same inputs burns a markdown span parse or a usvg tree
/// build for an answer the caller already has. The document version cannot
/// stand in for "did *this* block change": it is a whole-document counter, so
/// one streaming block bumps it for every block on screen and the surface's
/// content sync (`view::surface::content::sync_block_content`) would
/// re-parse the entire spawn band on every frame of someone else's stream.
/// This is the per-block signal it gates on.
///
/// Hashing is O(text) like the parse it avoids, but it is one linear pass
/// with no allocation, against span vectors and XML trees.
///
/// `OutputData::rich_json` is deliberately absent: nothing on this path reads
/// it (`detect_output_content` explicitly falls through for a rich_json-only
/// payload), and `serde_json::Value` is not `Hash`. If a renderer ever starts
/// reading it, it has to join the fingerprint in the same commit.
pub fn rich_input_fingerprint(text: &str, block: &BlockSnapshot) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    // Discriminants rather than the values: every one of these is a fieldless
    // enum, and `mem::discriminant` needs no derive on the type to stay
    // correct if a variant is added.
    std::mem::discriminant(&block.content_type).hash(&mut hasher);
    std::mem::discriminant(&block.kind).hash(&mut hasher);
    std::mem::discriminant(&block.role).hash(&mut hasher);
    // `is_streaming` — it only picks which diagnostic a failed SVG parse
    // logs, but Running → Done is exactly when the at-rest complaint should
    // get its chance to fire.
    std::mem::discriminant(&block.status).hash(&mut hasher);
    block.is_error.hash(&mut hasher);
    match block.output {
        None => hasher.write_u8(0),
        Some(ref output) => {
            hasher.write_u8(1);
            output.headers.hash(&mut hasher);
            hash_output_nodes(&output.root, &mut hasher);
        }
    }
    hasher.finish()
}

/// Structural hash of an `OutputData` node tree — everything
/// `compute_output_layout` colors by, including the entry types that never
/// reach the formatted text.
fn hash_output_nodes<H: std::hash::Hasher>(nodes: &[OutputNode], hasher: &mut H) {
    use std::hash::Hash;
    hasher.write_usize(nodes.len());
    for node in nodes {
        node.name.hash(hasher);
        std::mem::discriminant(&node.entry_type).hash(hasher);
        node.text.hash(hasher);
        node.cells.hash(hasher);
        hash_output_nodes(&node.children, hasher);
    }
}

/// Detect rich content with a content type hint.
///
/// When `content_type` is a specific variant, the declared type takes priority over sniffing:
/// - `ContentType::Svg` → parse as SVG directly; on parse failure falls
///   through to the heuristics (SVG source still reads as text)
/// - `ContentType::Markdown` → parse as markdown directly (cannot fail)
/// - `ContentType::Abc` → parse as ABC notation directly (generous parser,
///   never fails; kernel attaches errors as child blocks)
/// - `ContentType::Image` → CAS hash; an invalid hash falls through to the
///   heuristics
/// - `ContentType::Diff` → **always** returns `Some`, even for content that
///   does not parse (error preview, never falls through — see the arm's
///   comment for why Diff inverts the SVG policy)
/// - `ContentType::Plain` → fall through to heuristic detection
///
/// With `ContentType::Plain`, tries sparkline, then SVG, then the
/// ` ```diff ` fence sniff (which may enrich but never accuses — a fence
/// that doesn't parse falls through untouched), then markdown.
///
/// `svg_fontdb` provides fonts for SVG `<text>` rendering. Pass `None` if
/// the resource isn't available (text elements will be dropped).
///
/// `is_streaming` should reflect `block.status == Status::Running` — it only
/// changes which diagnostic an SVG parse failure logs (see `try_parse_svg`),
/// never the returned content.
///
/// Pure over its arguments and not cheap: callers must gate re-entry
/// themselves — see [`rich_input_fingerprint`], which is exactly this
/// function's input set.
pub fn detect_rich_content_typed(
    text: &str,
    content_type: ContentType,
    svg_fontdb: Option<&super::SvgFontDb>,
    is_streaming: bool,
) -> Option<RichContent> {
    // If content type is declared, use it directly
    match content_type {
        ContentType::Svg => {
            if let Some((tree, width, height, source)) =
                try_parse_svg(text, svg_fontdb, is_streaming)
            {
                return Some(RichContent {
                    kind: RichContentKind::Svg {
                        tree,
                        width,
                        height,
                        source,
                    },
                });
            }
        }
        ContentType::Markdown => {
            let spans = parse_to_rich_spans(text);
            let plain_text: String = spans.iter().map(|s| s.text.as_str()).collect();
            return Some(RichContent {
                kind: RichContentKind::Markdown { spans, plain_text },
            });
        }
        ContentType::Abc => {
            // Always render whatever the generous parser returned.
            // Errors are attached as child Error blocks by the kernel and
            // rendered as ordinary sibling blocks in the conversation stream.
            let result = kaijutsu_abc::parse(text);
            // TODO(multi-tune): RichContent::Abc currently holds a single
            // Tune. When a file contains multiple tunes (e.g. §13 sample
            // libraries), only the first is rendered. Revisit when the
            // renderer / kaijutsu block model decides whether to split
            // tunes across blocks or render them stacked in one block.
            let tune = result
                .value
                .into_iter()
                .next()
                .unwrap_or_default();
            return Some(RichContent {
                kind: RichContentKind::Abc {
                    source: Arc::new(text.to_string()),
                    tune: Arc::new(tune),
                },
            });
        }
        ContentType::Image => {
            let hash = text.trim().to_string();
            if hash.len() == 32 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
                return Some(RichContent {
                    kind: RichContentKind::Image { hash },
                });
            }
        }
        ContentType::Plain => {} // Fall through to heuristic detection
        ContentType::Diff => {
            // Unconditionally `Some` — including for content that does not
            // parse, which becomes an error preview rather than falling
            // through. That is deliberate and the opposite of the SVG arm
            // above: an SVG that won't parse still *reads* as its source
            // text, but a diff that won't parse rendered as plain text is
            // indistinguishable from a diff we simply chose not to color.
            // Content and content_type are separate LWW registers, so this
            // state is legitimate and must say so out loud
            // (`kaijutsu-diff`'s viewer contract).
            return Some(RichContent {
                kind: RichContentKind::Diff(Box::new(crate::text::diff::build_diff_preview(
                    text,
                    ContentType::Diff,
                ))),
            });
        }
    }
    // Try sparkline first — more specific pattern
    if let Some(data) = try_parse_sparkline(text) {
        return Some(RichContent {
            kind: RichContentKind::Sparkline(data),
        });
    }

    // Try SVG
    if let Some((tree, width, height, source)) = try_parse_svg(text, svg_fontdb, is_streaming) {
        return Some(RichContent {
            kind: RichContentKind::Svg {
                tree,
                width,
                height,
                source,
            },
        });
    }

    // Try a ` ```diff ` fence. Both mechanisms exist for the same reason SVG
    // has both: a *declared* type is authoritative, but a model writing prose
    // fences its diffs like everything else. Unlike the declared arm above,
    // a fence whose body doesn't parse falls through untouched — sniffing may
    // never promote a block into an error state it didn't ask for.
    //
    // Must precede the markdown fallback: markdown would otherwise claim the
    // fence as an ordinary code block and render it in one flat color.
    if let Some(inner) = extract_fenced_block(text, "diff")
        && let Some(preview) = crate::text::diff::try_build_diff_preview(inner, ContentType::Plain)
    {
        return Some(RichContent {
            kind: RichContentKind::Diff(Box::new(preview)),
        });
    }

    // Fall back to markdown
    let spans = parse_to_rich_spans(text);

    let has_formatting = spans
        .iter()
        .any(|s| s.bold || s.italic || s.code || s.code_block || s.heading_level.is_some());

    if !has_formatting {
        return None;
    }

    let plain_text: String = spans.iter().map(|s| s.text.as_str()).collect();

    Some(RichContent {
        kind: RichContentKind::Markdown { spans, plain_text },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── rich_input_fingerprint (the re-parse gate) ──────────────────────────

    fn text_block() -> BlockSnapshot {
        use kaijutsu_types::{BlockId, BlockKind, ContextId, PrincipalId};
        kaijutsu_types::BlockSnapshotBuilder::new(
            BlockId::new(ContextId::new(), PrincipalId::new(), 0),
            BlockKind::Text,
        )
        .content("# heading\n\nbody\n")
        .build()
    }

    /// The property `sync_block_content` gates on: a block nobody
    /// touched fingerprints the same, so it never re-parses. The document
    /// version is deliberately not an input — it belongs to the whole
    /// document, and one streaming block bumping it must not drag every other
    /// block on screen through a markdown parse.
    #[test]
    fn fingerprint_is_stable_for_an_untouched_block() {
        let block = text_block();
        let text = block.content.clone();
        assert_eq!(
            rich_input_fingerprint(&text, &block),
            rich_input_fingerprint(&text, &block),
            "identical inputs must fingerprint identically or nothing is ever reused"
        );
    }

    #[test]
    fn fingerprint_moves_when_the_text_grows() {
        let block = text_block();
        let before = rich_input_fingerprint(&block.content, &block);
        let after = rich_input_fingerprint(&format!("{} more", block.content), &block);
        assert_ne!(before, after, "a streaming block must keep re-parsing");
    }

    #[test]
    fn fingerprint_moves_when_the_declared_content_type_changes() {
        // Content and content_type are separate LWW registers — a retype with
        // untouched text is a legitimate state, and it changes the answer.
        let mut block = text_block();
        let text = block.content.clone();
        let before = rich_input_fingerprint(&text, &block);
        block.content_type = ContentType::Diff;
        assert_ne!(before, rich_input_fingerprint(&text, &block));
    }

    #[test]
    fn fingerprint_moves_when_the_block_stops_streaming() {
        let mut block = text_block();
        let text = block.content.clone();
        block.status = kaijutsu_types::Status::Running;
        let streaming = rich_input_fingerprint(&text, &block);
        block.status = kaijutsu_types::Status::Done;
        assert_ne!(
            streaming,
            rich_input_fingerprint(&text, &block),
            "Running → Done is when the at-rest SVG diagnostic gets its chance to fire"
        );
    }

    /// `OutputData` carries per-entry types that never reach the formatted
    /// text but do drive `compute_output_layout`'s coloring. Keying the gate
    /// on the text alone would let a recolor pass unnoticed.
    #[test]
    fn fingerprint_moves_when_only_an_output_entry_type_changes() {
        let mut block = text_block();
        let text = block.content.clone();
        let node = |t| vec![OutputNode::new("thing").with_entry_type(t)];

        block.output = Some(OutputData::nodes(node(OutputEntryType::File)));
        let as_file = rich_input_fingerprint(&text, &block);

        block.output = Some(OutputData::nodes(node(OutputEntryType::Directory)));
        assert_ne!(
            as_file,
            rich_input_fingerprint(&text, &block),
            "an entry-type recolor is invisible in the formatted text — the \
             fingerprint has to see it"
        );
    }

    /// A rich_json-only `OutputData` (kj's structured `.output` sideband —
    /// see `KjBuiltin::execute`) has an empty `root`: there is no node tree
    /// for the table/tree renderer to lay out. `detect_output_content` must
    /// return `None` deterministically so the block falls through to the
    /// plain-text path — rendering rich_json itself is a follow-up, not
    /// this fix. Without the explicit `root.is_empty()` guard this
    /// happened to work anyway (the empty-plain-text check below covers
    /// it), but this test pins the behavior on purpose rather than by
    /// accident.
    #[test]
    fn rich_json_only_output_falls_back_to_text_path() {
        let output = OutputData::new().with_rich_json(serde_json::json!(["x"]));
        assert!(output.root.is_empty(), "test premise: no node tree");

        assert!(
            detect_output_content(&output).is_none(),
            "rich_json-only OutputData must yield the text path (None), not a blank structured view"
        );
    }

    /// A headers-only `OutputData` (an empty-result table that still carries
    /// its column names — `root` empty, `headers` some) is real structure, not
    /// the "nothing here" shape the rich_json-only guard above targets. The
    /// old `if output.root.is_empty() { return None; }` guard fired on this
    /// too, before the headers check ever ran, so a legitimate empty-table-
    /// with-columns lost its header rendering. It must proceed past the
    /// empty-root guard.
    #[test]
    fn headers_only_output_is_not_dropped_by_the_empty_root_guard() {
        let output = OutputData::new().with_headers(vec!["A".to_string()]);
        assert!(output.root.is_empty(), "test premise: no rows");
        assert!(output.headers.is_some(), "test premise: has columns");

        assert!(
            detect_output_content(&output).is_some(),
            "headers-only OutputData must proceed past the empty-root guard \
             and render as a (header-only) table, not fall back to plain text"
        );
    }

    /// A well-formed SVG with non-positive intrinsic size (`width="0"
    /// height="0"`) parses fine at the XML/usvg level — there is no `Err` to
    /// catch. Left unchecked this would flow all the way to
    /// `RichContentKind::Svg { width: 0.0, height: 0.0, .. }`, which
    /// `view::block_render`'s `Svg` arm can't recover from (its
    /// `fit_svg_to_box` rejects a non-positive size, but by then the match
    /// has already committed to the `Svg` arm — there's no falling back to
    /// plain text from inside it) — a silently blank, zero-height block.
    /// `try_parse_svg` must reject it at the same point it already rejects a
    /// genuine parse failure.
    #[test]
    fn try_parse_svg_rejects_non_positive_intrinsic_size() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="0" height="0">
            <rect width="10" height="10" fill="red"/>
        </svg>"#;
        assert!(
            try_parse_svg(svg, None, false).is_none(),
            "a zero-intrinsic-size SVG must be rejected, not carried into a \
             Svg RichContent that then renders as a silently blank block"
        );
    }

    /// End-to-end version of the test above, through the same
    /// `ContentType::Svg`-declared path the kernel uses for a block it has
    /// already typed as SVG: a zero-size SVG must never surface as
    /// `RichContentKind::Svg` — whatever it falls through to instead
    /// (markdown, or `None` for the caller's plain-text path), the raw
    /// source stays visible rather than vanishing into an empty block.
    #[test]
    fn zero_size_svg_never_becomes_svg_rich_content() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="0" height="0">
            <rect width="10" height="10" fill="red"/>
        </svg>"#;
        let result = detect_rich_content_typed(svg, ContentType::Svg, None, false);
        if let Some(rich) = result {
            assert!(
                !matches!(rich.kind, RichContentKind::Svg { .. }),
                "a zero-size SVG must not be classified as RichContentKind::Svg"
            );
        }
    }

    // ── Diff (docs/diff.md slice 4) ────────────────────────────────────────

    fn diff_preview(rich: Option<RichContent>) -> Option<Box<crate::text::diff::DiffPreview>> {
        match rich?.kind {
            RichContentKind::Diff(p) => Some(p),
            _ => None,
        }
    }

    /// A block the kernel typed `Diff` renders as a diff preview, not as text
    /// and not as markdown.
    #[test]
    fn declared_diff_content_becomes_a_diff_preview() {
        let text = kaijutsu_diff::fixtures::read("canonical/single_file_modify.diff");
        let preview = diff_preview(detect_rich_content_typed(
            &text,
            ContentType::Diff,
            None,
            false,
        ))
        .expect("declared Diff must produce RichContentKind::Diff");
        assert!(!preview.is_error);
        assert_eq!(preview.stat.files_changed, 1);
    }

    /// The contract that makes the error state reachable at all: a block
    /// *declared* Diff whose content doesn't parse must still come back as a
    /// Diff — carrying the error preview — rather than falling through to the
    /// plain/markdown path where nothing would say the block is broken.
    #[test]
    fn declared_diff_that_does_not_parse_still_renders_as_a_diff_error() {
        let preview = diff_preview(detect_rich_content_typed(
            "I am not a diff.\n",
            ContentType::Diff,
            None,
            false,
        ))
        .expect("a declared Diff must never fall through to the text path");
        assert!(preview.is_error, "must be the visible error treatment");
    }

    /// A declared Diff is never `None` — that would render an empty cell,
    /// which the crate's viewer contract forbids explicitly.
    #[test]
    fn declared_diff_is_never_empty_even_for_empty_content() {
        assert!(
            detect_rich_content_typed("", ContentType::Diff, None, false).is_some(),
            "an empty declared-Diff block must still render something"
        );
    }

    /// The second mechanism: a model writing prose fences its diff. SVG has
    /// both a declared arm and a fence sniff; so does this.
    #[test]
    fn a_fenced_diff_block_is_sniffed_from_plain_content() {
        let body = kaijutsu_diff::fixtures::read("canonical/single_file_modify.diff");
        let fenced = format!("```diff\n{body}```\n");
        let preview = diff_preview(detect_rich_content_typed(
            &fenced,
            ContentType::Plain,
            None,
            false,
        ))
        .expect("a ```diff fence must sniff as a diff");
        assert!(!preview.is_error);
    }

    /// Sniffing must not hijack: a fence tagged `diff` whose body isn't one
    /// falls through to the ordinary text/markdown path. Promoting it to a
    /// diff *error* would be the app inventing a problem the author never
    /// declared.
    #[test]
    fn a_fenced_block_that_is_not_a_diff_falls_through() {
        let fenced = "```diff\nnot actually a diff\n```\n";
        let rich = detect_rich_content_typed(fenced, ContentType::Plain, None, false);
        assert!(
            diff_preview(rich).is_none(),
            "a non-diff ```diff fence must not become a diff preview"
        );
    }

    // ── diff span brushes ───────────────────────────────────────────────────

    fn word_diff_preview() -> crate::text::diff::DiffPreview {
        crate::text::diff::build_diff_preview(
            "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-the quick brown fox\n+the quick red fox\n",
            ContentType::Diff,
        )
    }

    /// The invariant both color lookups depend on: spans tile the text with no
    /// gap and no overlap. A gap falls through to the default brush mid-line;
    /// an overlap means parley (push order) and the byte-offset lookup (first
    /// match) would disagree about the color of the same byte.
    #[test]
    fn diff_span_brushes_tile_the_text_without_overlapping() {
        let theme = crate::ui::theme::Theme::default();
        let p = word_diff_preview();
        let spans = build_diff_span_brushes(&p, &theme);
        assert!(!p.words.is_empty(), "test premise: the fixture refines");

        let mut at = 0usize;
        for span in &spans {
            assert_eq!(span.start, at, "gap or overlap at {}", span.start);
            assert!(span.end > span.start);
            at = span.end;
        }
        assert_eq!(at, p.plain_text.len(), "spans must cover the whole text");
    }

    /// A changed word gets the emphasis color and the rest of its line keeps
    /// the line color — the whole point of the intra-line refinement.
    #[test]
    fn a_changed_word_is_brushed_apart_from_its_line() {
        let theme = crate::ui::theme::Theme::default();
        let p = word_diff_preview();
        let spans = build_diff_span_brushes(&p, &theme);

        let colored = |needle: &str| -> Brush {
            let at = p.plain_text.find(needle).expect("needle in the preview");
            let span = spans
                .iter()
                .find(|s| at >= s.start && at < s.end)
                .expect("every byte is covered");
            span.brush.clone()
        };

        assert_eq!(colored("brown"), bevy_color_to_brush(theme.diff_delete_word_fg));
        assert_eq!(colored("red fox"), bevy_color_to_brush(theme.diff_insert_word_fg));
        // The unchanged head of the inserted line stays the line color.
        let insert_line = p
            .lines
            .iter()
            .find(|l| l.class == crate::text::diff::DiffLineClass::Insert)
            .unwrap();
        let head = spans
            .iter()
            .find(|s| s.start == insert_line.start)
            .expect("the line starts a span");
        assert_eq!(head.brush, bevy_color_to_brush(theme.diff_insert_fg));
    }
}

// abc_summary() removed — ABC parse errors are now handled as structured
// Error child blocks by the kernel, not as fallback markdown summaries.
