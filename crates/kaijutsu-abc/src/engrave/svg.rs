//! SVG renderer: converts positioned EngravingElements to a self-contained SVG string.

use crate::engrave::font::font_cache;
use crate::engrave::ir::EngravingElement;

/// Render engraving elements to an SVG string.
///
/// `color` controls the foreground color for all notation elements.
/// Glyph elements are emitted as `<path>` with the outline from the font cache.
/// All elements carry `data-span-start` / `data-span-end` attributes for
/// future interactivity.
pub fn to_svg(elements: &[EngravingElement], margin: f64, color: &str) -> String {
    let bounds = super::ir::engraving_bounds(elements, margin);
    let (vb_x, vb_y) = (bounds.origin_x, bounds.origin_y);
    let (width, height) = (bounds.width, bounds.height);

    let mut svg = String::with_capacity(4096);
    svg.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"{:.1} {:.1} {:.1} {:.1}\" width=\"{:.0}\" height=\"{:.0}\">\n",
        vb_x, vb_y, width, height, width, height
    ));

    write_elements(&mut svg, elements, color);
    svg.push_str("</svg>\n");
    svg
}

/// Append notation inside an existing SVG coordinate system.
pub fn write_elements(svg: &mut String, elements: &[EngravingElement], color: &str) {
    let font = font_cache();
    for elem in elements {
        match elem {
            EngravingElement::Glyph {
                codepoint,
                x,
                y,
                scale,
                source_span,
            } => {
                if let Some(path_d) = font.glyph_path(*codepoint) {
                    svg.push_str(&format!(
                        "  <path d=\"{}\" transform=\"translate({:.1},{:.1}) scale({:.6})\" fill=\"{}\" data-span-start=\"{}\" data-span-end=\"{}\"/>\n",
                        path_d, x, y, scale, color, source_span.0, source_span.1
                    ));
                }
            }
            EngravingElement::Line {
                x1,
                y1,
                x2,
                y2,
                width,
                source_span,
            } => {
                svg.push_str(&format!(
                    "  <line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" stroke=\"{}\" stroke-width=\"{:.1}\" data-span-start=\"{}\" data-span-end=\"{}\"/>\n",
                    x1, y1, x2, y2, color, width, source_span.0, source_span.1
                ));
            }
            EngravingElement::Path {
                d,
                fill,
                source_span,
            } => {
                let fill_str = if *fill { color } else { "none" };
                svg.push_str(&format!(
                    "  <path d=\"{}\" fill=\"{}\" stroke=\"{}\" stroke-width=\"0.5\" data-span-start=\"{}\" data-span-end=\"{}\"/>\n",
                    d, fill_str, color, source_span.0, source_span.1
                ));
            }
            EngravingElement::Text {
                content,
                x,
                y,
                size,
                source_span,
            } => {
                svg.push_str(&format!(
                    "  <text x=\"{:.1}\" y=\"{:.1}\" font-family=\"serif\" font-size=\"{:.1}\" fill=\"{}\" data-span-start=\"{}\" data-span-end=\"{}\">{}</text>\n",
                    x, y, size, color, source_span.0, source_span.1,
                    escape_xml(content)
                ));
            }
        }
    }
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engrave::ir::EngravingElement;

    #[test]
    fn empty_elements_produce_valid_svg() {
        let svg = to_svg(&[], 20.0, "black");
        assert!(svg.starts_with("<svg"));
        assert!(svg.contains("</svg>"));
    }

    #[test]
    fn line_element_renders() {
        let elements = vec![EngravingElement::Line {
            x1: 0.0,
            y1: 0.0,
            x2: 100.0,
            y2: 0.0,
            width: 1.0,
            source_span: (0, 5),
        }];
        let svg = to_svg(&elements, 10.0, "black");
        assert!(svg.contains("<line"));
        assert!(svg.contains("data-span-start=\"0\""));
        assert!(svg.contains("data-span-end=\"5\""));
    }

    #[test]
    fn text_is_xml_escaped() {
        let elements = vec![EngravingElement::Text {
            content: "A<B&C".to_string(),
            x: 0.0,
            y: 0.0,
            size: 12.0,
            source_span: (0, 0),
        }];
        let svg = to_svg(&elements, 10.0, "black");
        assert!(svg.contains("A&lt;B&amp;C"));
    }

    #[test]
    fn custom_color_renders() {
        let elements = vec![EngravingElement::Line {
            x1: 0.0,
            y1: 0.0,
            x2: 100.0,
            y2: 0.0,
            width: 1.0,
            source_span: (0, 0),
        }];
        let svg = to_svg(&elements, 10.0, "#ffffff");
        assert!(svg.contains("stroke=\"#ffffff\""));
    }
}
