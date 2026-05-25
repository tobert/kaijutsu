//! Tests for barline variants, repeat dots, and volta brackets.

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

/// Vertical lines that span the full staff height (within a small epsilon),
/// returned as (x, width). A staff with 5 lines + barlines + ledgers will
/// also pass through here; filter by staff-top-to-bottom Y range.
fn full_height_vertical_lines(elements: &[EngravingElement]) -> Vec<(f64, f64)> {
    elements
        .iter()
        .filter_map(|e| match e {
            EngravingElement::Line { x1, y1, x2, y2, width, .. }
                if (x1 - x2).abs() < 0.01
                    && (y1 - 0.0).abs() < 0.5
                    && (y2 - 40.0).abs() < 0.5 =>
            {
                Some((*x1, *width))
            }
            _ => None,
        })
        .collect()
}

fn filled_paths(elements: &[EngravingElement]) -> Vec<&str> {
    elements
        .iter()
        .filter_map(|e| match e {
            EngravingElement::Path { d, fill: true, .. } => Some(d.as_str()),
            _ => None,
        })
        .collect()
}

fn texts(elements: &[EngravingElement]) -> Vec<(&str, f64, f64)> {
    elements
        .iter()
        .filter_map(|e| match e {
            EngravingElement::Text { content, x, y, .. } => Some((content.as_str(), *x, *y)),
            _ => None,
        })
        .collect()
}

// --- Bar variants -----------------------------------------------------------

#[test]
fn end_barline_emits_thick_line() {
    // `|]` at end should add a thick line. Use C2| C2|] form — the body
    // ends with |] explicitly, and the engrave's auto-emitted final
    // barline becomes irrelevant since the body itself ended on |].
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\nC2|C2|]\n");
    let lines = full_height_vertical_lines(&els);
    // Should include at least one thick line (width > 1.5).
    let thick = lines.iter().filter(|(_, w)| *w > 1.5).count();
    assert!(thick >= 1, "expected ≥1 thick barline, got widths {:?}", lines.iter().map(|(_, w)| w).collect::<Vec<_>>());
}

#[test]
fn double_barline_emits_two_thin_lines() {
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\nC2||C2|\n");
    let lines = full_height_vertical_lines(&els);
    // Find two thin lines very close together (gap < 1.5*sp = 15.0).
    let mut xs: Vec<f64> = lines.iter().filter(|(_, w)| *w < 1.5).map(|(x, _)| *x).collect();
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let close_pairs = xs.windows(2).filter(|w| (w[1] - w[0]) < 6.0 && (w[1] - w[0]) > 0.1).count();
    assert!(
        close_pairs >= 1,
        "expected at least one pair of close thin lines (double bar), xs={:?}",
        xs
    );
}

// --- Repeat dots ------------------------------------------------------------

#[test]
fn repeat_start_emits_two_dots() {
    // `|:` — should produce 2 filled dot paths near the bar
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n|:C2|C2:|\n");
    let dots = filled_paths(&els);
    // |: contributes 2 dots, :| contributes 2 dots → at least 4.
    assert!(
        dots.len() >= 4,
        "expected ≥4 filled dot paths from |: ... :|, got {}",
        dots.len()
    );
}

#[test]
fn repeat_both_emits_four_dots() {
    // `::` is dots-thin-thick-thin-dots, so 4 dots total
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n|:C2::C2:|\n");
    let dots = filled_paths(&els);
    // |: (2 dots) + :: (4 dots) + :| (2 dots) = 8 dots
    assert!(
        dots.len() >= 8,
        "expected ≥8 filled dot paths, got {}",
        dots.len()
    );
}

// --- Voltas -----------------------------------------------------------------

#[test]
fn first_ending_emits_label_text() {
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n|:C2|1 D2:|2 E2|]\n");
    let labels: Vec<&str> = texts(&els)
        .iter()
        .map(|(t, _, _)| *t)
        .filter(|t| *t == "1" || *t == "2")
        .collect();
    assert!(
        labels.contains(&"1"),
        "expected '1' volta label, got texts: {:?}",
        texts(&els)
    );
    assert!(
        labels.contains(&"2"),
        "expected '2' volta label, got texts: {:?}",
        texts(&els)
    );
}

#[test]
fn nth_ending_emits_joined_label() {
    // [1,3,5 → label should contain "1", "3", "5"
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n|:C2|[1,3 D2:|2 E2|]\n");
    let labels: Vec<&str> = texts(&els).iter().map(|(t, _, _)| *t).collect();
    let has_nth = labels.iter().any(|l| l.contains('1') && l.contains('3'));
    assert!(
        has_nth,
        "expected label containing 1 and 3, got texts: {:?}",
        labels
    );
}

#[test]
fn volta_label_sits_above_staff() {
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n|:C2|1 D2:|2 E2|]\n");
    let label_ys: Vec<f64> = texts(&els)
        .iter()
        .filter(|(t, _, _)| *t == "1" || *t == "2")
        .map(|(_, _, y)| *y)
        .collect();
    // Staff top is y=0; volta labels should sit above the staff (y < 0).
    for y in &label_ys {
        assert!(
            *y < 0.0,
            "volta label y={} should be above the staff (y < 0)",
            y
        );
    }
    assert!(!label_ys.is_empty());
}

#[test]
fn volta_bracket_horizontal_line_above_staff() {
    // The volta should also produce a horizontal line (the bracket)
    // above the staff.
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n|:C2|1 D2:|2 E2|]\n");
    let bracket_lines: Vec<_> = elements_horizontal_lines_above_staff(&els);
    assert!(
        !bracket_lines.is_empty(),
        "expected at least one horizontal bracket line above the staff"
    );
}

fn elements_horizontal_lines_above_staff(elements: &[EngravingElement]) -> Vec<(f64, f64, f64)> {
    elements
        .iter()
        .filter_map(|e| match e {
            EngravingElement::Line { x1, y1, x2, y2, .. }
                // horizontal (y1 == y2), above the staff (y < 0), and
                // spans more than a single staff width.
                if (y1 - y2).abs() < 0.01 && *y1 < 0.0 && (x2 - x1).abs() > 5.0 =>
            {
                Some((*x1, *x2, *y1))
            }
            _ => None,
        })
        .collect()
}
