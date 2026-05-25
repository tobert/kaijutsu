//! Tests for w:/W: lyric rendering under note positions.

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

/// Text elements with y below the staff (y > 40 for default sp=10).
fn lyric_texts(elements: &[EngravingElement]) -> Vec<(&str, f64, f64)> {
    elements
        .iter()
        .filter_map(|e| match e {
            EngravingElement::Text { content, x, y, .. } if *y > 40.0 => {
                Some((content.as_str(), *x, *y))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn simple_w_line_emits_one_text_per_syllable() {
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\nCDE|\nw:do re mi\n");
    let syllables: Vec<&str> = lyric_texts(&els).iter().map(|(t, _, _)| *t).collect();
    assert_eq!(syllables, vec!["do", "re", "mi"]);
}

#[test]
fn hyphenated_word_splits_to_consecutive_notes() {
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\nCD|\nw:hel-lo\n");
    let syllables: Vec<&str> = lyric_texts(&els).iter().map(|(t, _, _)| *t).collect();
    assert_eq!(syllables, vec!["hel-", "lo"]);
}

#[test]
fn star_skips_a_note() {
    // do * mi  → first note gets "do", second is skipped, third gets "mi"
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\nCDE|\nw:do * mi\n");
    let syllables: Vec<&str> = lyric_texts(&els).iter().map(|(t, _, _)| *t).collect();
    assert_eq!(syllables, vec!["do", "mi"]);
}

#[test]
fn underscore_extends_syllable() {
    // do _ mi  → second note has no syllable (do extends across)
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\nCDE|\nw:do _ mi\n");
    let syllables: Vec<&str> = lyric_texts(&els).iter().map(|(t, _, _)| *t).collect();
    assert_eq!(syllables, vec!["do", "mi"]);
}

#[test]
fn syllables_align_horizontally_with_notes() {
    // Syllable x should be near its note's x (within a notehead width or so).
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\nCDE|\nw:do re mi\n");
    let lyrs = lyric_texts(&els);
    let note_xs: Vec<f64> = els
        .iter()
        .filter_map(|e| match e {
            EngravingElement::Glyph {
                codepoint: 0xE0A4,
                x,
                ..
            } => Some(*x),
            _ => None,
        })
        .collect();
    assert_eq!(lyrs.len(), 3, "expected 3 lyric texts");
    assert!(note_xs.len() >= 3, "need at least 3 noteheads");
    for (i, (txt, lx, _)) in lyrs.iter().enumerate() {
        // Allow a generous tolerance — staff width / 2.
        let nx = note_xs[i];
        assert!(
            (lx - nx).abs() < 8.0,
            "lyric '{}' at x={} not aligned with note at x={}",
            txt,
            lx,
            nx
        );
    }
}

#[test]
fn capital_w_line_does_not_render_under_notes() {
    // W: (capital) is end-of-tune verses, not aligned. We should NOT
    // produce per-note lyric texts.
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\nCDE|\nW:end of tune verse\n");
    let lyrs = lyric_texts(&els);
    // We may emit zero, or we may emit a single block — but not three
    // per-note syllables.
    let aligned_under_notes = lyrs.iter().filter(|(t, _, _)| *t == "end" || *t == "of").count();
    assert_eq!(
        aligned_under_notes, 0,
        "W: should not emit per-syllable aligned text, got {:?}",
        lyrs
    );
}
