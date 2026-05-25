//! Tests for grace-note rendering.

use kaijutsu_abc::engrave::{layout, EngravingElement, EngravingOptions};
use kaijutsu_abc::parse;

fn engrave(abc: &str) -> Vec<EngravingElement> {
    let result = parse(abc);
    assert!(
        !result.has_errors(),
        "parse errors: {:?}",
        result.errors().collect::<Vec<_>>()
    );
    layout::engrave(&result.value[0], &EngravingOptions::default())
}

/// Return every filled notehead glyph (cp 0xE0A4) with its (x, scale).
fn filled_noteheads(elements: &[EngravingElement]) -> Vec<(f64, f64)> {
    elements
        .iter()
        .filter_map(|e| match e {
            EngravingElement::Glyph {
                codepoint: 0xE0A4,
                x,
                scale,
                ..
            } => Some((*x, *scale)),
            _ => None,
        })
        .collect()
}

#[test]
fn grace_notes_emit_small_noteheads_before_principal() {
    // {AB}c → two grace heads (A, B) and one principal (c).
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n{AB}c|\n");
    let heads = filled_noteheads(&els);
    assert_eq!(heads.len(), 3, "expected 3 filled noteheads, got {}", heads.len());

    // Sort by x; the first two should be smaller than the third.
    let mut sorted = heads.clone();
    sorted.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    let principal_scale = sorted[2].1;
    for (i, &(_x, s)) in sorted[..2].iter().enumerate() {
        assert!(
            s < principal_scale,
            "grace #{} scale {} should be smaller than principal scale {}",
            i,
            s,
            principal_scale
        );
    }
}

#[test]
fn single_grace_note_renders() {
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n{g}c|\n");
    let heads = filled_noteheads(&els);
    assert_eq!(heads.len(), 2, "expected 2 filled noteheads, got {}", heads.len());
}

#[test]
fn acciaccatura_adds_a_slash() {
    // {/A}B — acciaccatura grace + principal. The slash is a short
    // diagonal line at roughly the grace note's y. We detect it by
    // looking for a non-vertical, non-horizontal line.
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n{/A}B|\n");
    let plain = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n{A}B|\n");

    let count_diag = |list: &[EngravingElement]| {
        list.iter()
            .filter(|e| match e {
                EngravingElement::Line { x1, y1, x2, y2, .. } => {
                    let horiz = (y1 - y2).abs() < 0.01;
                    let vert = (x1 - x2).abs() < 0.01;
                    !horiz && !vert
                }
                _ => false,
            })
            .count()
    };

    let with_slash = count_diag(&els);
    let without_slash = count_diag(&plain);
    assert!(
        with_slash > without_slash,
        "acciaccatura should add a diagonal slash line; with={} without={}",
        with_slash,
        without_slash
    );
}

#[test]
fn grace_notes_dont_consume_principal_duration_width() {
    // Without grace, c2 takes the same horizontal width as {AB}c2 minus
    // the small grace-prefix space. Just check that grace notes don't
    // make the staff dramatically longer.
    let with_grace = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n{ABCDE}c|\n");
    let plain = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\nc|\n");

    // Use the rightmost x reached by any glyph.
    let max_x = |list: &[EngravingElement]| -> f64 {
        list.iter()
            .filter_map(|e| match e {
                EngravingElement::Glyph { x, .. } => Some(*x),
                EngravingElement::Line { x2, .. } => Some(*x2),
                _ => None,
            })
            .fold(0.0_f64, f64::max)
    };
    // Grace notes do shift everything right, but at small scale the
    // staff growth should be less than the unit_width (sp * 2.5 = 25)
    // per grace note. With 5 graces, ≤ 5 * sp * 1.2 = 60 extra px.
    let delta = max_x(&with_grace) - max_x(&plain);
    assert!(
        delta < 80.0,
        "5 grace notes inflated staff by {} px (limit 80)",
        delta
    );
}
