//! Engrave the ensemble on flat facets, with the same beat scale on each staff.

use std::fmt::Write;

use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use kaijutsu_abc::ast::Meter;
use kaijutsu_abc::engrave::{EngravingElement, font::font_cache, layout::engrave_timed_staff, svg::write_elements};

use crate::text::{SvgFontDb, svg_raster::rasterize_svg};

pub(super) const BEAT_LEFT: f32 = 0.18;
pub(super) const BEAT_RIGHT: f32 = 0.96;
const WIDTH: u32 = 768;
const HEIGHT: u32 = 384;

pub(super) fn beats_per_bar(source: &str) -> Option<f64> {
    let parsed = kaijutsu_abc::parse(source);
    let meter = parsed.value.first()?.header.meter.as_ref()?;
    if matches!(meter, Meter::None) { return None; }
    let (n, d) = meter.to_fraction();
    (n > 0 && d > 0).then(|| 4.0 * f64::from(n) / f64::from(d))
}

#[derive(Default)]
pub(super) struct EngravedBand {
    pub pages: Vec<Image>,
    pub spans: Vec<(f64, f64)>,
}

/// Horizontal ink bounds including the outline of a glyph, line or curve.
fn ink_extent(mark: &EngravingElement) -> (f64, f64) {
    match mark {
        EngravingElement::Glyph { x, scale, codepoint, .. } => {
            if let Some(path) = font_cache().glyph_bezpath(*codepoint) {
                let b = path.control_box();
                (x + b.x0 * scale, x + b.x1 * scale)
            } else {
                (*x, *x)
            }
        }
        EngravingElement::Line { x1, x2, width, .. } => (x1.min(*x2) - width, x1.max(*x2) + width),
        EngravingElement::Path { d, .. } => kurbo::BezPath::from_svg(d).map_or(
            (f64::NEG_INFINITY, f64::INFINITY),
            |p| { let b = p.control_box(); (b.x0 - 1.0, b.x1 + 1.0) },
        ),
        EngravingElement::Text { x, content, size, .. } =>
            (x - size * 2.0, x + size * (content.chars().count() + 1) as f64 * 2.0),
    }
}

struct ScoreRow {
    start: f64,
    stop: f64,
    y: f64,
    signature: String,
    music: Vec<((f64, f64), String)>,
}

/// Each part starts at a signed beat relative to the first facet. Rasterize
/// on a worker when a score changes. The playhead moves independently.
pub(super) fn engrave_facets(
    parts: &[(&str, f64)], beats_per_facet: f64, facet_count: usize, ink: Color, fonts: Option<&SvgFontDb>,
) -> EngravedBand {
    if !beats_per_facet.is_finite() || beats_per_facet <= 0.0 || !(1..=8).contains(&facet_count) {
        return EngravedBand::default();
    }
    let tunes = parts.iter().filter_map(|(abc, start)| {
        if !start.is_finite() { return None; }
        let parsed = kaijutsu_abc::parse(abc);
        if parsed.has_errors() { return None; }
        Some((parsed.value.into_iter().next()?, *start))
    }).collect::<Vec<_>>();
    let voices = tunes.iter().flat_map(|(tune, start)| tune.voices.iter()
        .filter(|v| !v.elements.is_empty()).map(move |v| (tune, v, *start))).collect::<Vec<_>>();
    if voices.is_empty() { return EngravedBand::default(); }

    let color = ink.to_srgba().to_hex();
    let width = f64::from(WIDTH);
    let left = width * f64::from(BEAT_LEFT);
    let right = width * f64::from(BEAT_RIGHT);
    let beat_width = (right - left) / beats_per_facet;
    let row_height = f64::from(HEIGHT) / voices.len() as f64;
    let spacing = (row_height / 10.0).min(16.0);
    let mut rows = Vec::new();
    for (i, (tune, voice, start)) in voices.into_iter().enumerate() {
        let mut staff = engrave_timed_staff(tune, voice, spacing, beat_width);
        let stop = start + staff.duration_beats;
        if start >= beats_per_facet * facet_count as f64 || stop <= 0.0 || stop <= start { continue; }
        for mark in &mut staff.music {
            if let EngravingElement::Line { width, .. } = mark { *width = width.max(1.5); }
        }
        let mut signature = String::new();
        write_elements(&mut signature, &staff.signature, &color);
        let music = staff.music.iter().map(|mark| {
            let mut markup = String::new();
            write_elements(&mut markup, std::slice::from_ref(mark), &color);
            (ink_extent(mark), markup)
        }).collect();
        let y = row_height * (i as f64 + 0.5) - spacing * 2.0;
        rows.push(ScoreRow { start, stop, y, signature, music });
    }
    if rows.is_empty() { return EngravedBand::default(); }
    // Accidentals and ledger lines lead the beat by up to one staff space.
    let clip_left = left - spacing * 1.25;
    let clip_right = clip_left + right - left;
    let options = fonts.map(SvgFontDb::usvg_options).unwrap_or_default();
    let mut pages = Vec::with_capacity(facet_count);
    for page in 0..facet_count {
        let page_beat = beats_per_facet * page as f64;
        let mut svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}"><defs><clipPath id="bar"><rect x="{clip_left}" y="0" width="{}" height="{HEIGHT}"/></clipPath></defs>"#,
            right - left,
        );
        let mut music = String::new();
        for row in rows.iter().filter(|r| r.stop > page_beat && r.start < page_beat + beats_per_facet) {
            let y = row.y;
            for line in 0..5 {
                let line_y = y + spacing * f64::from(line);
                write!(svg, r#"<path d="M10 {line_y} H{right}" fill="none" stroke="{color}" stroke-opacity=".67" stroke-width="1.7"/>"#).unwrap();
            }
            write!(svg, r#"<g transform="translate(12 {y})">{}</g>"#, row.signature).unwrap();
            let x = left + (row.start - page_beat) * beat_width;
            write!(music, r#"<g transform="translate({x} {y})">"#).unwrap();
            for &((min, max), ref mark) in &row.music {
                if x + max >= clip_left - 1.0 && x + min <= clip_right + 1.0 {
                    music.push_str(mark);
                }
            }
            music.push_str("</g>");
        }
        write!(svg, r#"<g clip-path="url(#bar)">{music}</g></svg>"#).unwrap();
        let Ok(tree) = usvg::Tree::from_str(&svg, &options) else { return EngravedBand::default(); };
        let Some(pixels) = rasterize_svg(&tree, WIDTH, HEIGHT, 1.0) else { return EngravedBand::default(); };
        pages.push(Image::new(
            Extent3d { width: WIDTH, height: HEIGHT, depth_or_array_layers: 1 },
            TextureDimension::D2, pixels, TextureFormat::Rgba8UnormSrgb, default(),
        ));
    }
    let end_beat = facet_count as f64 * beats_per_facet;
    let spans = rows.into_iter().map(|r| (r.start.max(0.0), r.stop.min(end_beat))).collect();
    EngravedBand { pages, spans }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_compound_meter_in_quarter_beats() {
        assert_eq!(beats_per_bar("X:1\nM:6/8\nL:1/8\nK:C\nCDEFGA|"), Some(3.0));
        assert_eq!(beats_per_bar("X:1\nM:none\nK:C\nCDEFGA|"), None);
    }

    #[test]
    fn engraved_facets_show_only_parts_active_in_their_beat_span() {
        let notes = "X:1\nM:4/4\nL:1/4\nK:C\nCDEF|";
        let result = engrave_facets(&[(notes, 4.0)], 4.0, 3, Color::srgb(0.2, 0.8, 1.0), None);
        let facets = result.pages;
        assert_eq!(facets.len(), 3);
        let has_ink = |i: usize| facets[i].data.as_ref().unwrap().chunks_exact(4).any(|p| p[3] > 127);
        assert!(!has_ink(0));
        assert!(has_ink(1));
        assert!(!has_ink(2));
        assert_eq!(result.spans, [(4.0, 8.0)]);
        assert!(engrave_facets(&[(notes, -4.0)], 4.0, 1, Color::WHITE, None).pages.is_empty());
    }
}
