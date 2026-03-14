//! Rich content rendering via Parley + Vello.
//!
//! Supports multiple content formats via `RichContentKind`:
//! - **Markdown**: per-span brush coloring (headings, code, bold, etc.)
//! - **Sparkline**: inline timeseries mini-charts (pure Vello vector paths)
//! - **SVG**: inline vector graphics (via bevy_vello's `load_svg_from_str`)
//!
//! Detection is centralized in `detect_rich_content()` — tries sparkline first
//! (more specific fence pattern), then SVG, then falls back to markdown.

use bevy::prelude::*;
use bevy_vello::prelude::*;
use bevy_vello::parley;
use vello::kurbo::Affine;
use vello::peniko::{Brush, Fill};

use std::sync::Arc;

use kaijutsu_types::{OutputData, OutputEntryType};

use super::markdown::{MarkdownColors, RichSpan, parse_to_rich_spans};
use super::sparkline::{SparklineData, SparklineColors, try_parse_sparkline, build_sparkline_paths, render_sparkline_scene};
use super::components::bevy_color_to_brush;

use crate::view::format::{OutputLayout, compute_output_layout, format_output_data};

/// Per-span brush mapping: byte range → Brush.
struct SpanBrush {
    /// Byte offset of span start in the concatenated plain text.
    start: usize,
    /// Byte offset of span end.
    end: usize,
    /// Brush for this span.
    brush: Brush,
}

/// Rich content for a block cell — dispatches rendering by format.
///
/// When present on a block cell entity alongside `UiVelloText`,
/// the `render_rich_content` system will render the appropriate format.
#[derive(Component)]
pub struct RichContent {
    pub kind: RichContentKind,
    /// Version tracking — skip re-render when unchanged.
    pub version: u64,
    /// Last rendered version.
    pub last_render_version: u64,
    /// The max_advance used for the last render (0.0 = None).
    pub last_max_advance: f32,
}

impl RichContent {
    /// Desired height for non-text content (sparklines, SVGs).
    ///
    /// Returns `Some(height)` for formats that need explicit sizing
    /// (no Parley text to measure). Returns `None` for text-based formats.
    pub fn desired_height(&self, theme: &crate::ui::theme::Theme) -> Option<f32> {
        match &self.kind {
            RichContentKind::Markdown { .. } => None,
            RichContentKind::Sparkline(_) => Some(theme.sparkline_height),
            RichContentKind::Svg { height, .. } => Some(*height),
            RichContentKind::Output { .. } => None,
        }
    }
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
    },
    /// Structured OutputData with per-cell coloring by EntryType.
    Output {
        /// Pre-computed column→byte mapping for per-cell brushes.
        layout: OutputLayout,
        /// Whitespace-padded measurement text (same as UiVelloText.value).
        plain_text: String,
    },
}

/// Build a `Vec<SpanBrush>` from parsed spans + theme colors.
///
/// Maps each span's byte range to a Brush based on its formatting:
/// - Headings → `md_heading_color`
/// - Code/code blocks → `md_code_fg` / `md_code_block_fg`
/// - Bold → `md_strong_color` or base_color
/// - Plain text → `base_color`
fn build_span_brushes(
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
fn brush_at_offset(span_brushes: &[SpanBrush], offset: usize) -> Option<&Brush> {
    // Spans are contiguous and ordered — binary search on start.
    span_brushes
        .iter()
        .find(|sb| offset >= sb.start && offset < sb.end)
        .map(|sb| &sb.brush)
}

/// Render a Parley layout to a vello Scene with per-span brushes.
///
/// This is a modified version of bevy_vello's `VelloFont::render()` that
/// uses per-glyph-run brush lookup instead of a single global brush.
fn render_layout_with_brushes(
    scene: &mut vello::Scene,
    layout: &parley::Layout<Brush>,
    span_brushes: &[SpanBrush],
    fallback_brush: &Brush,
) {
    let transform = Affine::IDENTITY;

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
            let run_brush = brush_at_offset(span_brushes, text_range.start)
                .unwrap_or(fallback_brush);

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

/// System: render `RichContent` blocks as `UiVelloScene`.
///
/// Dispatches on `RichContentKind`:
/// - **Markdown**: builds Parley layout with per-span brushes
/// - **Sparkline**: renders Vello vector paths (line + fill)
pub fn render_rich_content(
    mut commands: Commands,
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<super::resources::FontHandles>,
    theme: Res<crate::ui::theme::Theme>,
    mut query: Query<(Entity, &mut RichContent, &UiVelloText, &ComputedNode)>,
) {
    let md_colors = MarkdownColors {
        heading: theme.md_heading_color,
        code: theme.md_code_fg,
        strong: theme.md_strong_color,
        code_block: theme.md_code_block_fg,
    };

    let sparkline_colors = SparklineColors {
        line: theme.sparkline_line_color,
        fill: theme.sparkline_fill_color,
    };

    for (entity, mut rich, vello_text, node) in query.iter_mut() {
        let current_advance = node.size().x;
        let advance_changed = (rich.last_max_advance - current_advance).abs() > 1.0
            && current_advance > 0.0;

        if rich.version == rich.last_render_version && !advance_changed {
            continue;
        }

        let mut scene = vello::Scene::new();

        match &rich.kind {
            RichContentKind::Markdown { spans, plain_text } => {
                let Some(font) = fonts.get(&font_handles.mono) else {
                    continue;
                };

                let max_advance = {
                    if current_advance > 0.0 { Some(current_advance) } else { None }
                };

                let layout = font.layout(
                    plain_text,
                    &vello_text.style,
                    vello_text.text_align,
                    max_advance,
                );

                let span_brushes = build_span_brushes(
                    spans,
                    theme.block_assistant,
                    &md_colors,
                );

                let fallback_brush = bevy_color_to_brush(theme.block_assistant);
                render_layout_with_brushes(&mut scene, &layout, &span_brushes, &fallback_brush);
            }
            RichContentKind::Sparkline(data) => {
                let w = node.size().x as f64;
                let h = theme.sparkline_height as f64;
                if w > 0.0 && h > 0.0 {
                    let paths = build_sparkline_paths(data, w, h, 4.0);
                    render_sparkline_scene(&mut scene, &paths, &sparkline_colors);
                }
            }
            RichContentKind::Svg { scene: svg_scene, width: svg_w, height: svg_h } => {
                let container_w = node.size().x;
                if container_w > 0.0 && *svg_w > 0.0 {
                    // Scale SVG to fit container width while preserving aspect ratio
                    let scale = (container_w / svg_w).min(1.0) as f64;
                    scene.append(svg_scene, Some(Affine::scale(scale)));
                } else {
                    let _ = svg_h; // suppress unused warning when width is zero
                    scene.append(svg_scene, None);
                }
            }
            RichContentKind::Output { layout, plain_text } => {
                let Some(font) = fonts.get(&font_handles.mono) else {
                    continue;
                };

                let max_advance = if current_advance > 0.0 { Some(current_advance) } else { None };

                let parley_layout = font.layout(
                    plain_text,
                    &vello_text.style,
                    vello_text.text_align,
                    max_advance,
                );

                let span_brushes = build_output_span_brushes(layout, &theme);
                let fallback_brush = bevy_color_to_brush(theme.block_tool_result);
                render_layout_with_brushes(&mut scene, &parley_layout, &span_brushes, &fallback_brush);
            }
        }

        commands.entity(entity).insert(UiVelloScene::from(scene));
        rich.last_render_version = rich.version;
        rich.last_max_advance = current_advance;
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
fn build_output_span_brushes(
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
pub fn detect_output_content(output: &OutputData, version: u64) -> Option<RichContent> {
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
        version,
        last_render_version: 0,
        last_max_advance: 0.0,
    })
}

/// Maximum SVG source size we'll attempt to parse (100KB).
const SVG_MAX_BYTES: usize = 100 * 1024;

/// Maximum rendered height for inline SVGs.
const SVG_MAX_HEIGHT: f32 = 400.0;

/// Try to extract and parse SVG content from block text.
///
/// Recognizes two patterns:
/// - Raw SVG: text starts with `<svg`
/// - Fenced SVG: ` ```svg\n...\n``` `
fn try_parse_svg(text: &str) -> Option<(Arc<vello::Scene>, f32, f32)> {
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

    match bevy_vello::integrations::svg::load_svg_from_str(svg_str) {
        Ok(svg) => {
            let w = svg.width;
            let h = svg.height.min(SVG_MAX_HEIGHT);
            Some((svg.scene, w, h))
        }
        Err(e) => {
            warn!("SVG parse failed: {}", e);
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
pub fn detect_rich_content(text: &str, version: u64) -> Option<RichContent> {
    detect_rich_content_typed(text, version, None)
}

/// Detect rich content with an optional content type hint.
///
/// When `content_type` is `Some`, the declared type takes priority over sniffing:
/// - `"image/svg+xml"` → parse as SVG directly
/// - `"text/markdown"` → parse as markdown directly
/// - Other types → fall through to heuristic detection
///
/// When `content_type` is `None`, tries sparkline, then SVG, then markdown.
pub fn detect_rich_content_typed(text: &str, version: u64, content_type: Option<&str>) -> Option<RichContent> {
    // If content type is declared, use it directly
    if let Some(ct) = content_type {
        match ct {
            "image/svg+xml" => {
                if let Some((scene, width, height)) = try_parse_svg(text) {
                    return Some(RichContent {
                        kind: RichContentKind::Svg { scene, width, height },
                        version,
                        last_render_version: 0,
                        last_max_advance: 0.0,
                    });
                }
            }
            "text/markdown" => {
                let spans = parse_to_rich_spans(text);
                let plain_text: String = spans.iter().map(|s| s.text.as_str()).collect();
                return Some(RichContent {
                    kind: RichContentKind::Markdown { spans, plain_text },
                    version,
                    last_render_version: 0,
                    last_max_advance: 0.0,
                });
            }
            _ => {} // Unknown content types fall through to heuristic detection
        }
    }
    // Try sparkline first — more specific pattern
    if let Some(data) = try_parse_sparkline(text) {
        return Some(RichContent {
            kind: RichContentKind::Sparkline(data),
            version,
            last_render_version: 0,
            last_max_advance: 0.0,
        });
    }

    // Try SVG
    if let Some((scene, width, height)) = try_parse_svg(text) {
        return Some(RichContent {
            kind: RichContentKind::Svg { scene, width, height },
            version,
            last_render_version: 0,
            last_max_advance: 0.0,
        });
    }

    // Fall back to markdown
    let spans = parse_to_rich_spans(text);

    let has_formatting = spans.iter().any(|s| {
        s.bold || s.italic || s.code || s.code_block || s.heading_level.is_some()
    });

    if !has_formatting {
        return None;
    }

    let plain_text: String = spans.iter().map(|s| s.text.as_str()).collect();

    Some(RichContent {
        kind: RichContentKind::Markdown { spans, plain_text },
        version,
        last_render_version: 0,
        last_max_advance: 0.0,
    })
}
