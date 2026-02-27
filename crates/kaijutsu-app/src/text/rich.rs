//! Rich text rendering via Parley + Vello.
//!
//! Renders markdown-styled text with per-span coloring by:
//! 1. Parsing markdown → `Vec<RichSpan>` (via `markdown::parse_to_rich_spans`)
//! 2. Building a Parley layout from the plain text (via `VelloFont::layout`)
//! 3. Rendering glyph runs with per-span brushes (color from theme)
//!
//! Since we use monospace fonts, glyph positions are identical regardless of
//! style. Only the brush (color) varies per span — no need for per-span Parley
//! styles, which would require access to bevy_vello's internal FontContext.

use bevy::prelude::*;
use bevy_vello::prelude::*;
use bevy_vello::parley;
use vello::kurbo::Affine;
use vello::peniko::{Brush, Fill};

use super::markdown::{MarkdownColors, RichSpan, parse_to_rich_spans};
use super::components::bevy_color_to_brush;

/// Per-span brush mapping: byte range → Brush.
struct SpanBrush {
    /// Byte offset of span start in the concatenated plain text.
    start: usize,
    /// Byte offset of span end.
    end: usize,
    /// Brush for this span.
    brush: Brush,
}

/// Rich text content for a block cell.
///
/// When present on a block cell entity alongside `UiVelloText`,
/// the `render_rich_text` system will build a `UiVelloScene` with
/// per-span colored text.
#[derive(Component)]
pub struct RichTextContent {
    /// Parsed markdown spans.
    pub spans: Vec<RichSpan>,
    /// Concatenated plain text (matches UiVelloText.value).
    pub plain_text: String,
    /// Version tracking — skip re-render when unchanged.
    pub version: u64,
    /// Last rendered version.
    pub last_render_version: u64,
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

/// System: render `RichTextContent` blocks as `UiVelloScene`.
///
/// For entities that have both `UiVelloText` (for layout measurement) and
/// `RichTextContent` (parsed markdown), this system:
/// 1. Builds a Parley layout from the plain text
/// 2. Maps spans to per-run brushes
/// 3. Renders to a `vello::Scene`
/// 4. Inserts/updates `UiVelloScene` on the entity
///
/// The `UiVelloText.style.brush` is set to transparent so only the
/// scene renders visually. The UiVelloText still provides content sizing
/// for layout_block_cells.
pub fn render_rich_text(
    mut commands: Commands,
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<super::resources::FontHandles>,
    theme: Res<crate::ui::theme::Theme>,
    mut query: Query<(Entity, &mut RichTextContent, &UiVelloText, &ComputedNode)>,
) {
    let Some(font) = fonts.get(&font_handles.mono) else {
        return;
    };

    let md_colors = MarkdownColors {
        heading: theme.md_heading_color,
        code: theme.md_code_fg,
        strong: theme.md_strong_color,
        code_block: theme.md_code_block_fg,
    };

    for (entity, mut rich, vello_text, node) in query.iter_mut() {
        if rich.version == rich.last_render_version {
            continue;
        }

        // Use computed node width for word wrapping
        let max_advance = {
            let w = node.size().x;
            if w > 0.0 { Some(w) } else { None }
        };

        // Build layout using VelloFont (same as UiVelloText's internal layout)
        let layout = font.layout(
            &rich.plain_text,
            &vello_text.style,
            vello_text.text_align,
            max_advance,
        );

        // Map markdown spans → brush per byte range
        let span_brushes = build_span_brushes(
            &rich.spans,
            theme.block_assistant, // base color for Model/Text blocks
            &md_colors,
        );

        let fallback_brush = bevy_color_to_brush(theme.block_assistant);

        // Render layout to scene with per-span brushes
        let mut scene = vello::Scene::new();
        render_layout_with_brushes(&mut scene, &layout, &span_brushes, &fallback_brush);

        commands.entity(entity).insert(UiVelloScene::from(scene));

        rich.last_render_version = rich.version;
    }
}

/// Parse a block's content into `RichTextContent` if it contains markdown.
///
/// Called from `sync_block_cell_buffers` for Model/Text blocks.
/// Returns None for plain text that has no markdown formatting.
pub fn parse_rich_content(text: &str, version: u64) -> Option<RichTextContent> {
    let spans = parse_to_rich_spans(text);

    // Skip rich rendering for trivially plain text (all spans are unstyled)
    let has_formatting = spans.iter().any(|s| {
        s.bold || s.italic || s.code || s.code_block || s.heading_level.is_some()
    });

    if !has_formatting {
        return None;
    }

    // Concatenate span text to rebuild the plain text
    let plain_text: String = spans.iter().map(|s| s.text.as_str()).collect();

    Some(RichTextContent {
        spans,
        plain_text,
        version,
        last_render_version: 0,
    })
}
