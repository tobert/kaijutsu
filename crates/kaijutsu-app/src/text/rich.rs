//! Rich content rendering via Parley + Vello.
//!
//! Supports multiple content formats via `RichContentKind`:
//! - **Markdown**: per-span brush coloring (headings, code, bold, etc.)
//! - **Sparkline**: inline timeseries mini-charts — plain Bevy UI rectangle
//!   geometry now, not Vello (see `text::sparkline`); only detection and the
//!   `SparklineData` payload live here.
//! - **SVG**: inline vector graphics, CPU-rasterized via `usvg` + `resvg` +
//!   `tiny-skia` (`text::svg_raster`) to a Bevy `Image`/`ImageNode` — no vello
//!
//! Detection is centralized in `detect_rich_content()` — tries sparkline first
//! (more specific fence pattern), then SVG, then falls back to markdown.

use bevy::prelude::*;
use vello::peniko::Brush;

use std::sync::Arc;

use kaijutsu_types::{ContentType, OutputData, OutputEntryType};

use super::components::bevy_color_to_brush;
use super::markdown::{MarkdownColors, RichSpan, parse_to_rich_spans};
use super::sparkline::{
    SparklineData, try_parse_sparkline,
};

use crate::view::format::{OutputLayout, compute_output_layout, format_output_data};

/// Per-span brush mapping: byte range → Brush.
pub struct SpanBrush {
    /// Byte offset of span start in the concatenated plain text.
    pub start: usize,
    /// Byte offset of span end.
    pub end: usize,
    /// Brush for this span.
    pub brush: Brush,
}

/// Rich content for a block cell — dispatches rendering by format.
///
/// When present on a block cell entity, `build_block_scenes` renders it:
/// into MSDF glyphs (Markdown, Output), MSDF glyphs + `MsdfBlockGeometry`
/// (Abc), a CPU-rasterized `Image`/`ImageNode` child (Svg, via
/// `text::svg_raster`), or plain UI rectangle children (Sparkline, Image).
/// No arm reaches the per-block vello scene any more.
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

/// Detect rich content from structured OutputData.
///
/// Returns `None` for simple text (no coloring needed).
/// For tabular/tree/list data, returns a `RichContent::Output` with
/// pre-computed layout for per-cell coloring.
pub fn detect_output_content(output: &OutputData, _version: u64) -> Option<RichContent> {
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
    } else if let Some(inner) = extract_fenced_block(text, "svg") {
        inner
    } else {
        return None;
    };

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
fn extract_fenced_block<'a>(text: &'a str, lang: &str) -> Option<&'a str> {
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

/// Detect rich content from a block's text.
///
/// When `content_type` is provided, skips heuristic detection and uses the
/// declared type directly. Falls back to sniffing when `content_type` is `None`.
#[allow(dead_code)]
pub fn detect_rich_content(text: &str, _version: u64) -> Option<RichContent> {
    // No block status available at this (unused) call site — `is_streaming`
    // only changes which diagnostic a parse failure logs, not behavior, so
    // `false` ("treat as at rest") is a safe default here.
    detect_rich_content_typed(text, 0, ContentType::Plain, None, false)
}

/// Detect rich content with a content type hint.
///
/// When `content_type` is a specific variant, the declared type takes priority over sniffing:
/// - `ContentType::Svg` → parse as SVG directly
/// - `ContentType::Markdown` → parse as markdown directly
/// - `ContentType::Abc` → parse as ABC notation directly
/// - `ContentType::Plain` → fall through to heuristic detection
///
/// With `ContentType::Plain`, tries sparkline, then SVG, then markdown heuristics.
///
/// `svg_fontdb` provides fonts for SVG `<text>` rendering. Pass `None` if
/// the resource isn't available (text elements will be dropped).
///
/// `is_streaming` should reflect `block.status == Status::Running` — it only
/// changes which diagnostic an SVG parse failure logs (see `try_parse_svg`),
/// never the returned content.
pub fn detect_rich_content_typed(
    text: &str,
    _version: u64,
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
            // Errors are attached as child Error blocks by the kernel
            // and rendered via the ErrorChildIndex stacking path.
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
                .unwrap_or_else(kaijutsu_abc::Tune::default);
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
            detect_output_content(&output, 0).is_none(),
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
            detect_output_content(&output, 0).is_some(),
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
        let result = detect_rich_content_typed(svg, 0, ContentType::Svg, None, false);
        if let Some(rich) = result {
            assert!(
                !matches!(rich.kind, RichContentKind::Svg { .. }),
                "a zero-size SVG must not be classified as RichContentKind::Svg"
            );
        }
    }
}

// abc_summary() removed — ABC parse errors are now handled as structured
// Error child blocks by the kernel, not as fallback markdown summaries.
