//! Integration tests for the engrave pipeline: ABC → IR → SVG.

use kaijutsu_abc::engrave::{engrave_to_svg, layout, EngravingOptions};
use kaijutsu_abc::parse;

fn default_options() -> EngravingOptions {
    EngravingOptions::default()
}

#[test]
fn simple_melody_produces_valid_svg() {
    let abc = "X:1\nT:Test\nM:4/4\nL:1/8\nK:C\nCDEF GABc|\n";
    let result = parse(abc);
    assert!(!result.has_errors());

    let svg = engrave_to_svg(&result.value[0], &default_options());
    assert!(svg.starts_with("<svg"), "Should start with <svg>");
    assert!(svg.contains("</svg>"), "Should end with </svg>");
}

#[test]
fn svg_has_five_staff_lines() {
    let abc = "X:1\nT:Test\nM:4/4\nL:1/8\nK:C\nCDEF|\n";
    let result = parse(abc);
    assert!(!result.has_errors());

    let svg = engrave_to_svg(&result.value[0], &default_options());
    // Count horizontal staff lines (they span most of the width)
    let line_count = svg.matches("<line").count();
    // At minimum: 5 staff lines + barlines + stems
    assert!(
        line_count >= 5,
        "Should have at least 5 lines (staff lines), got {}",
        line_count
    );
}

#[test]
fn svg_has_clef_path() {
    let abc = "X:1\nT:Test\nM:4/4\nK:C\nC|\n";
    let result = parse(abc);
    assert!(!result.has_errors());

    let svg = engrave_to_svg(&result.value[0], &default_options());
    // The treble clef should produce a <path> element
    assert!(
        svg.contains("<path"),
        "Should contain a path element (clef glyph)"
    );
}

#[test]
fn key_signature_adds_accidental_glyphs() {
    // G major has 1 sharp (F#)
    let abc = "X:1\nT:Test\nM:4/4\nK:G\nG|\n";
    let result = parse(abc);
    assert!(!result.has_errors());

    let elements = layout::engrave(&result.value[0], &default_options());
    // Should have at least one glyph with the sharp codepoint (0xE262)
    let sharp_glyphs: Vec<_> = elements
        .iter()
        .filter(|e| {
            matches!(
                e,
                kaijutsu_abc::engrave::EngravingElement::Glyph {
                    codepoint: 0xE262,
                    ..
                }
            )
        })
        .collect();
    assert_eq!(
        sharp_glyphs.len(),
        1,
        "G major should have 1 sharp in key signature"
    );
}

#[test]
fn flat_key_signature() {
    // F major has 1 flat (Bb)
    let abc = "X:1\nT:Test\nM:4/4\nK:F\nF|\n";
    let result = parse(abc);
    assert!(!result.has_errors());

    let elements = layout::engrave(&result.value[0], &default_options());
    let flat_glyphs: Vec<_> = elements
        .iter()
        .filter(|e| {
            matches!(
                e,
                kaijutsu_abc::engrave::EngravingElement::Glyph {
                    codepoint: 0xE260,
                    ..
                }
            )
        })
        .collect();
    assert_eq!(
        flat_glyphs.len(),
        1,
        "F major should have 1 flat in key signature"
    );
}

#[test]
fn time_signature_glyphs() {
    let abc = "X:1\nT:Test\nM:4/4\nK:C\nC|\n";
    let result = parse(abc);
    assert!(!result.has_errors());

    let elements = layout::engrave(&result.value[0], &default_options());
    // Should have time sig digit 4 (U+E084) twice (4/4)
    let digit_4_count = elements
        .iter()
        .filter(|e| {
            matches!(
                e,
                kaijutsu_abc::engrave::EngravingElement::Glyph {
                    codepoint: 0xE084,
                    ..
                }
            )
        })
        .count();
    assert_eq!(digit_4_count, 2, "4/4 should produce two '4' digit glyphs");
}

#[test]
fn barlines_produce_vertical_lines() {
    let abc = "X:1\nT:Test\nM:4/4\nL:1/4\nK:C\nCDEF|GABc|\n";
    let result = parse(abc);
    assert!(!result.has_errors());

    let elements = layout::engrave(&result.value[0], &default_options());
    // Barlines are vertical lines (x1 == x2)
    let barlines: Vec<_> = elements
        .iter()
        .filter(|e| {
            if let kaijutsu_abc::engrave::EngravingElement::Line { x1, x2, y1, y2, .. } = e {
                (x1 - x2).abs() < 0.01 && (y2 - y1).abs() > 1.0
            } else {
                false
            }
        })
        .collect();
    // Should have at least 2 barlines (one between measures + final)
    assert!(
        barlines.len() >= 2,
        "Should have at least 2 barlines, got {}",
        barlines.len()
    );
}

#[test]
fn all_elements_have_source_spans() {
    let abc = "X:1\nT:Test\nM:4/4\nK:C\nCDEF|\n";
    let result = parse(abc);
    assert!(!result.has_errors());

    let svg = engrave_to_svg(&result.value[0], &default_options());
    // Every rendered element should have data-span-start and data-span-end
    for line in svg.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("<line")
            || trimmed.starts_with("<path")
            || trimmed.starts_with("<text")
        {
            assert!(
                trimmed.contains("data-span-start="),
                "Element missing data-span-start: {}",
                trimmed
            );
            assert!(
                trimmed.contains("data-span-end="),
                "Element missing data-span-end: {}",
                trimmed
            );
        }
    }
}

#[test]
fn multi_measure_layout_does_not_overflow() {
    let abc = "X:1\nT:Test\nM:4/4\nL:1/8\nK:C\nCDEF GABc|cBAG FEDC|CDEF GABc|cBAG FEDC|\n";
    let result = parse(abc);
    assert!(!result.has_errors());

    let svg = engrave_to_svg(&result.value[0], &default_options());
    // Should produce a valid SVG with reasonable dimensions
    assert!(svg.contains("viewBox="));
    // Parse the viewBox to check it's reasonable
    if let Some(vb_start) = svg.find("viewBox=\"") {
        let after = &svg[vb_start + 9..];
        if let Some(vb_end) = after.find('"') {
            let vb = &after[..vb_end];
            let parts: Vec<f64> = vb
                .split_whitespace()
                .filter_map(|s| s.parse().ok())
                .collect();
            assert_eq!(parts.len(), 4, "viewBox should have 4 values");
            let width = parts[2];
            let height = parts[3];
            assert!(width > 100.0, "Width should be > 100, got {}", width);
            assert!(height > 40.0, "Height should be > 40, got {}", height);
            assert!(
                width < 10000.0,
                "Width should be reasonable (<10000), got {}",
                width
            );
        }
    }
}

#[test]
fn round_trip_parse_engrave_no_panic() {
    let abc = "X:1\nT:Test\nM:4/4\nL:1/8\nK:C\nCDEF|\n";
    let result = parse(abc);
    assert!(!result.has_errors());

    // Should not panic
    let svg = engrave_to_svg(&result.value[0], &default_options());
    assert!(!svg.is_empty());
}

#[test]
fn rest_produces_rest_glyph() {
    let abc = "X:1\nT:Test\nM:4/4\nL:1/4\nK:C\nCz2E|\n";
    let result = parse(abc);
    assert!(!result.has_errors());

    let elements = layout::engrave(&result.value[0], &default_options());
    // Quarter rest = U+E4E5, half rest = U+E4E4
    let rest_glyphs: Vec<_> = elements
        .iter()
        .filter(|e| {
            matches!(
                e,
                kaijutsu_abc::engrave::EngravingElement::Glyph {
                    codepoint: 0xE4E3..=0xE4E7,
                    ..
                }
            )
        })
        .collect();
    assert!(
        !rest_glyphs.is_empty(),
        "Should have at least one rest glyph"
    );
}

#[test]
fn title_appears_as_text_element() {
    let abc = "X:1\nT:Cooley's Reel\nM:4/4\nK:Emin\nE|\n";
    let result = parse(abc);
    assert!(!result.has_errors());

    let svg = engrave_to_svg(&result.value[0], &default_options());
    assert!(svg.contains("Cooley"), "SVG should contain the tune title");
}

#[test]
fn chord_symbol_appears_as_text() {
    let abc = "X:1\nT:Test\nM:4/4\nK:C\n\"Am\"A2|\n";
    let result = parse(abc);
    assert!(!result.has_errors());

    let svg = engrave_to_svg(&result.value[0], &default_options());
    assert!(svg.contains("Am"), "SVG should contain chord symbol Am");
}

#[test]
fn accidental_note_gets_accidental_glyph() {
    let abc = "X:1\nT:Test\nM:4/4\nK:C\n^CE|\n";
    let result = parse(abc);
    assert!(!result.has_errors());

    let elements = layout::engrave(&result.value[0], &default_options());
    // Should have a sharp glyph (0xE262) for the ^C
    let sharp_count = elements
        .iter()
        .filter(|e| {
            matches!(
                e,
                kaijutsu_abc::engrave::EngravingElement::Glyph {
                    codepoint: 0xE262,
                    ..
                }
            )
        })
        .count();
    assert!(
        sharp_count >= 1,
        "Should have at least 1 sharp accidental glyph"
    );
}

#[test]
fn sharp_minor_key_signature_draws_correct_sharps() {
    // K:G#m has 5 sharps (relative major B). The staff must draw 5 sharps, not
    // fall through the major-only table to 3 flats. §3.1.14.
    use kaijutsu_abc::engrave::EngravingElement;
    let abc = "X:1\nT:t\nM:4/4\nK:G#m\nz|\n";
    let result = parse(abc);
    assert!(!result.has_errors(), "{:?}", result.feedback);
    let elements = layout::engrave(&result.value[0], &default_options());
    let sharps = elements
        .iter()
        .filter(|e| matches!(e, EngravingElement::Glyph { codepoint: 0xE262, .. }))
        .count();
    let flats = elements
        .iter()
        .filter(|e| matches!(e, EngravingElement::Glyph { codepoint: 0xE260, .. }))
        .count();
    assert_eq!(sharps, 5, "G#m → 5 sharps on the staff");
    assert_eq!(flats, 0, "G#m → no flats");
}

#[test]
fn tuplet_renders_inner_chords_and_rests() {
    // §4.13: a tuplet groups notes, rests AND chords — the layout must render
    // all of them, not just bare notes (the old arm dropped rests/chords).
    use kaijutsu_abc::engrave::EngravingElement;
    let is_head = |cp: u32| (0xE0A2..=0xE0A4).contains(&cp);
    let is_rest = |cp: u32| (0xE4E3..=0xE4E7).contains(&cp);

    // `(3[CEG]ab` → chord (3 noteheads) + a + b = 5 noteheads.
    let abc = "X:1\nT:t\nM:4/4\nL:1/4\nK:C\n(3[CEG]ab|\n";
    let els = layout::engrave(&parse(abc).value[0], &default_options());
    let heads = els
        .iter()
        .filter(|e| matches!(e, EngravingElement::Glyph { codepoint, .. } if is_head(*codepoint)))
        .count();
    assert!(heads >= 5, "chord(3)+a+b should be ≥5 noteheads, got {heads}");

    // `(3zab` → one rest glyph for the z inside the tuplet.
    let abc2 = "X:1\nT:t\nM:4/4\nL:1/4\nK:C\n(3zab|\n";
    let els2 = layout::engrave(&parse(abc2).value[0], &default_options());
    let rests = els2
        .iter()
        .filter(|e| matches!(e, EngravingElement::Glyph { codepoint, .. } if is_rest(*codepoint)))
        .count();
    assert_eq!(rests, 1, "the z inside the tuplet renders a rest glyph");
}

#[test]
fn invisible_rest_draws_no_glyph() {
    // §4.5: `x` is an invisible rest — it occupies time but draws nothing,
    // unlike the visible `z`.
    use kaijutsu_abc::engrave::EngravingElement;
    let count_rests = |abc: &str| {
        layout::engrave(&parse(abc).value[0], &default_options())
            .iter()
            .filter(|e| matches!(e, EngravingElement::Glyph { codepoint, .. } if (0xE4E3..=0xE4E7).contains(codepoint)))
            .count()
    };
    assert_eq!(count_rests("X:1\nT:t\nM:4/4\nL:1/4\nK:C\nC z D|\n"), 1, "z draws a rest");
    assert_eq!(count_rests("X:1\nT:t\nM:4/4\nL:1/4\nK:C\nC x D|\n"), 0, "x draws nothing");
}

#[test]
fn whole_rest_hangs_higher_than_half_rest() {
    // A whole rest hangs from the line above the middle (smaller y = higher);
    // a half rest sits on the middle line.
    use kaijutsu_abc::engrave::EngravingElement;
    let rest_y = |abc: &str| {
        layout::engrave(&parse(abc).value[0], &default_options())
            .iter()
            .find_map(|e| match e {
                EngravingElement::Glyph { codepoint, y, .. }
                    if (0xE4E3..=0xE4E7).contains(codepoint) => Some(*y),
                _ => None,
            })
            .expect("a rest glyph")
    };
    let whole = rest_y("X:1\nT:t\nM:4/4\nL:1/1\nK:C\nz|\n");
    let half = rest_y("X:1\nT:t\nM:4/4\nL:1/2\nK:C\nz|\n");
    assert!(whole < half, "whole rest ({whole}) should hang higher than half ({half})");
}

#[test]
fn free_meter_draws_no_time_signature() {
    // M:none is free meter — no time signature should be drawn.
    use kaijutsu_abc::engrave::EngravingElement;
    let count_timesig = |abc: &str| {
        layout::engrave(&parse(abc).value[0], &default_options())
            .iter()
            .filter(|e| matches!(e, EngravingElement::Glyph { codepoint, .. } if (0xE080..=0xE089).contains(codepoint)))
            .count()
    };
    assert!(count_timesig("X:1\nT:t\nM:4/4\nK:C\nC|\n") > 0, "4/4 draws a time sig");
    assert_eq!(count_timesig("X:1\nT:t\nM:none\nK:C\nC|\n"), 0, "M:none draws none");
}

#[test]
fn dotted_notes_get_augmentation_dots() {
    // §4.3: dotted durations draw augmentation dots. C3/2 = 1 dot, C7/4 = 2.
    use kaijutsu_abc::engrave::EngravingElement;
    let count_dots = |abc: &str| {
        layout::engrave(&parse(abc).value[0], &default_options())
            .iter()
            .filter(|e| matches!(e, EngravingElement::Glyph { codepoint: 0xE1E7, .. }))
            .count()
    };
    assert_eq!(count_dots("X:1\nT:t\nM:4/4\nL:1/4\nK:C\nC3/2 D|\n"), 1, "dotted quarter");
    assert_eq!(count_dots("X:1\nT:t\nM:4/4\nL:1/4\nK:C\nC7/4 D|\n"), 2, "double-dotted");
    assert_eq!(count_dots("X:1\nT:t\nM:4/4\nL:1/4\nK:C\nC D|\n"), 0, "plain, no dots");
}

#[test]
fn chord_seconds_offset_to_avoid_collision() {
    // §4.17 engraving: notes a staff-second apart in a chord must offset so
    // their noteheads don't overlap. [CD] are adjacent — two distinct x's.
    use kaijutsu_abc::engrave::EngravingElement;
    let els = layout::engrave(&parse("X:1\nT:t\nM:4/4\nK:C\n[CD]|\n").value[0], &default_options());
    let xs: Vec<f64> = els
        .iter()
        .filter_map(|e| match e {
            EngravingElement::Glyph { codepoint, x, .. }
                if (0xE0A2..=0xE0A4).contains(codepoint) => Some(*x),
            _ => None,
        })
        .collect();
    assert_eq!(xs.len(), 2, "two noteheads");
    assert!((xs[0] - xs[1]).abs() > 1.0, "noteheads must be offset, got {xs:?}");
}
