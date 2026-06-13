//! Rich content rendering via Parley + Vello.
//!
//! Supports multiple content formats via `RichContentKind`:
//! - **Markdown**: per-span brush coloring (headings, code, bold, etc.)
//! - **Sparkline**: inline timeseries mini-charts (pure Vello vector paths)
//! - **SVG**: inline vector graphics (via `vello_svg` + `usvg`)
//!
//! Detection is centralized in `detect_rich_content()` — tries sparkline first
//! (more specific fence pattern), then SVG, then falls back to markdown.

use bevy::prelude::*;
use vello::kurbo::Affine;
use vello::peniko::{Brush, Fill};

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
/// When present on a block cell entity, `build_block_scenes` renders
/// the content into the per-block vello scene.
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
    /// Inline timeseries mini-chart.
    Sparkline(SparklineData),
    /// Inline SVG vector graphic.
    Svg {
        /// Pre-parsed Vello scene from the SVG content.
        scene: Arc<vello::Scene>,
        /// Original SVG width (for aspect-ratio scaling).
        width: f32,
        /// Display height (capped to a reasonable maximum).
        height: f32,
        /// Raw SVG source for future re-parse (e.g. DPI-aware re-rendering).
        #[allow(dead_code)] // Retained for the planned DPI-aware re-parse.
        source: Arc<String>,
        /// Scale factor at parse time (placeholder for future DPI re-parse).
        #[allow(dead_code)] // Retained for the planned DPI-aware re-parse.
        rendered_at_dpi: f32,
    },
    /// ABC music notation — rendered directly to vello from engraving IR.
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

/// Render a Parley layout to a vello Scene with per-span brushes.
///
/// Walks the Parley layout's glyph runs and emits them into the scene, using
/// per-glyph-run brush lookup instead of a single global brush.
pub fn render_layout_with_brushes(
    scene: &mut vello::Scene,
    layout: &parley::Layout<Brush>,
    span_brushes: &[SpanBrush],
    fallback_brush: &Brush,
    offset: (f64, f64),
) {
    let transform = Affine::translate(offset);

    for line in layout.lines() {
        for item in line.items() {
            let parley::PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                continue;
            };

            let mut x = glyph_run.offset();
            let y = glyph_run.baseline();
            let run = glyph_run.run();
            let font = run.font();
            let font_size = run.font_size();
            let synthesis = run.synthesis();
            let glyph_xform = synthesis
                .skew()
                .map(|angle| Affine::skew(angle.to_radians().tan() as f64, 0.0));

            // Determine brush from the glyph run's text range
            let text_range = run.text_range();
            let run_brush =
                brush_at_offset(span_brushes, text_range.start).unwrap_or(fallback_brush);

            scene
                .draw_glyphs(font)
                .brush(run_brush)
                .hint(true)
                .transform(transform)
                .glyph_transform(glyph_xform)
                .font_size(font_size)
                .normalized_coords(run.normalized_coords())
                .draw(
                    Fill::NonZero,
                    glyph_run.glyphs().map(|glyph| {
                        let gx = x + glyph.x;
                        let gy = y - glyph.y;
                        x += glyph.advance;
                        vello::Glyph {
                            id: glyph.id as _,
                            x: gx,
                            y: gy,
                        }
                    }),
                );
        }
    }
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
fn try_parse_svg(
    text: &str,
    svg_fontdb: Option<&super::SvgFontDb>,
) -> Option<(Arc<vello::Scene>, f32, f32, Arc<String>)> {
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
        None => vello_svg::usvg::Options::default(),
    };

    match vello_svg::usvg::Tree::from_str(svg_str, &options) {
        Ok(tree) => {
            let scene = vello_svg::render_tree(&tree);
            let size = tree.size();
            let source = Arc::new(svg_str.to_string());
            Some((Arc::new(scene), size.width(), size.height(), source))
        }
        Err(e) => {
            // During streaming (Status::Running), incomplete SVG is expected to
            // fail parsing — just return None and let the block render as text.
            // After Status::Done, the kernel should have already validated and
            // attached Error children; a parse failure here is a real divergence.
            warn!("SVG parse failed (may be mid-stream): {e}");
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
    detect_rich_content_typed(text, 0, ContentType::Plain, None)
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
pub fn detect_rich_content_typed(
    text: &str,
    _version: u64,
    content_type: ContentType,
    svg_fontdb: Option<&super::SvgFontDb>,
) -> Option<RichContent> {
    // If content type is declared, use it directly
    match content_type {
        ContentType::Svg => {
            if let Some((scene, width, height, source)) = try_parse_svg(text, svg_fontdb) {
                return Some(RichContent {
                    kind: RichContentKind::Svg {
                        scene,
                        width,
                        height,
                        source,
                        rendered_at_dpi: 1.0,
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
    if let Some((scene, width, height, source)) = try_parse_svg(text, svg_fontdb) {
        return Some(RichContent {
            kind: RichContentKind::Svg {
                scene,
                width,
                height,
                source,
                rendered_at_dpi: 1.0,
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

// abc_summary() removed — ABC parse errors are now handled as structured
// Error child blocks by the kernel, not as fallback markdown summaries.
