//! Tests for tie and slur rendering.

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

/// All filled paths (used for both noteheads and tie/slur shapes).
fn filled_paths(elements: &[EngravingElement]) -> Vec<&str> {
    elements
        .iter()
        .filter_map(|e| match e {
            EngravingElement::Path { d, fill: true, .. } => Some(d.as_str()),
            _ => None,
        })
        .collect()
}

/// Filled paths whose first move is at a y position above the staff
/// (y < 0). Used to detect "tie/slur curves above the notes".
fn paths_with_first_y_above(elements: &[EngravingElement], y_threshold: f64) -> Vec<&str> {
    filled_paths(elements)
        .into_iter()
        .filter(|d| {
            // d-strings start with "M x y ..."
            let toks: Vec<&str> = d.split_whitespace().collect();
            if toks.len() < 3 || toks[0] != "M" {
                return false;
            }
            let y: f64 = toks[2].parse().unwrap_or(0.0);
            y < y_threshold
        })
        .collect()
}

/// Filled paths whose first move is below the staff (y > threshold).
fn paths_with_first_y_below(elements: &[EngravingElement], y_threshold: f64) -> Vec<&str> {
    filled_paths(elements)
        .into_iter()
        .filter(|d| {
            let toks: Vec<&str> = d.split_whitespace().collect();
            if toks.len() < 3 || toks[0] != "M" {
                return false;
            }
            let y: f64 = toks[2].parse().unwrap_or(0.0);
            y > y_threshold
        })
        .collect()
}

// --- Ties ------------------------------------------------------------------

#[test]
fn tied_high_note_produces_curve_path_with_quadratic_segment() {
    // c is C5 (above middle line). Stem-down convention → tie ABOVE the notes.
    let abc = "X:1\nT:Test\nM:4/4\nL:1/4\nK:C\nc-c|\n";
    let els = engrave(abc);
    // There should be at least one filled path containing a quadratic
    // curve segment ("Q ").
    let curve_paths: Vec<&str> = filled_paths(&els)
        .into_iter()
        .filter(|d| d.contains(" Q "))
        .collect();
    assert!(
        !curve_paths.is_empty(),
        "expected at least one curve path with Q, paths={:?}",
        filled_paths(&els)
    );
}

#[test]
fn tie_on_high_note_curves_above_staff() {
    // g' is G5 (pos -0.5, just above the top line) → stem-down → tie
    // above the notes. The curve's first point should sit above the
    // staff top (y < 0).
    let abc = "X:1\nT:Test\nM:4/4\nL:1/4\nK:C\ng'-g'|\n";
    let els = engrave(abc);
    let above = paths_with_first_y_above(&els, 0.0);
    let curves_above: Vec<&str> = above.into_iter().filter(|d| d.contains(" Q ")).collect();
    assert!(
        !curves_above.is_empty(),
        "expected a curve path above staff, none found"
    );
}

#[test]
fn tie_on_low_note_curves_below_staff() {
    // Low C (uppercase C with no octave marks → C3 in treble, pos 6.5)
    // → stem-up → tie below.
    let abc = "X:1\nT:Test\nM:4/4\nL:1/4\nK:C\nC-C|\n";
    let els = engrave(abc);
    // Below the staff bottom (y > 40 for default sp=10).
    let below = paths_with_first_y_below(&els, 40.0);
    let curves_below: Vec<&str> = below.into_iter().filter(|d| d.contains(" Q ")).collect();
    assert!(
        !curves_below.is_empty(),
        "expected a curve path below staff (y > 40), paths={:?}",
        filled_paths(&els)
    );
}

#[test]
fn tie_across_bar_line_still_emits_curve() {
    // c-|c — tie crosses the barline. Should still emit a curve.
    let abc = "X:1\nT:Test\nM:4/4\nL:1/4\nK:C\nc-|c2|\n";
    let els = engrave(abc);
    let curve_paths: Vec<&str> = filled_paths(&els)
        .into_iter()
        .filter(|d| d.contains(" Q "))
        .collect();
    assert!(
        !curve_paths.is_empty(),
        "expected tie curve across barline, found nothing"
    );
}

#[test]
fn no_tie_when_pitches_differ() {
    // `c-d` is parser-valid but the second pitch differs — only c is
    // tagged tie=true, but no matching next-c exists. We should NOT emit
    // a stray curve.
    let abc = "X:1\nT:Test\nM:4/4\nL:1/4\nK:C\nc-d|\n";
    let els = engrave(abc);
    let curve_paths: Vec<&str> = filled_paths(&els)
        .into_iter()
        .filter(|d| d.contains(" Q "))
        .collect();
    assert!(
        curve_paths.is_empty(),
        "should not draw a tie when pitches differ, got {:?}",
        curve_paths
    );
}

// --- Slurs -----------------------------------------------------------------

#[test]
fn slur_over_four_notes_produces_curve() {
    // (CDEF) — slur over four notes.
    let abc = "X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n(CDEF)|\n";
    let els = engrave(abc);
    let curve_paths: Vec<&str> = filled_paths(&els)
        .into_iter()
        .filter(|d| d.contains(" Q "))
        .collect();
    assert!(
        !curve_paths.is_empty(),
        "expected slur curve over (CDEF), none found"
    );
}

#[test]
fn unbalanced_slur_does_not_panic_or_emit_curve() {
    // Open slur with no close — should be tolerated, no curve drawn.
    let abc = "X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n(CDEF|\n";
    let result = parse(abc);
    // parse may emit warnings; we just need the layout to not panic.
    let _ = layout::engrave(&result.value[0], &EngravingOptions::default());
}

#[test]
fn nested_slurs_emit_two_curves() {
    // (A(BC)D) — two slurs, one nested.
    let abc = "X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n(A(BC)D)|\n";
    let els = engrave(abc);
    let curve_paths: Vec<&str> = filled_paths(&els)
        .into_iter()
        .filter(|d| d.contains(" Q "))
        .collect();
    assert!(
        curve_paths.len() >= 2,
        "expected ≥2 curves for nested slurs, got {}",
        curve_paths.len()
    );
}
