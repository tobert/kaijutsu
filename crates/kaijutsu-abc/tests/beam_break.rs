//! Tests for ABC v2.1 §4.7 backtick beam-break.
//!
//! `` ` `` between two notes forces a visual beam break without
//! inserting a space. The character is purely a typesetting hint; the
//! AST has no place to record it, so the parser silently consumes
//! backticks (no warning) and the surrounding notes parse normally.

use kaijutsu_abc::{parse_with_mode, Element, ParseMode};

fn notes(tune: &kaijutsu_abc::Tune) -> Vec<&kaijutsu_abc::Note> {
    tune.voices
        .iter()
        .flat_map(|v| v.elements.iter())
        .filter_map(|e| match e {
            Element::Note(n) => Some(n),
            _ => None,
        })
        .collect()
}

#[test]
fn single_backtick_between_notes() {
    let result = parse_with_mode("G`A", ParseMode::Fragment);
    assert!(!result.has_errors());
    assert_eq!(notes(&result.value[0]).len(), 2);
}

#[test]
fn multiple_backticks_consumed() {
    // From §4.15 fixture: `G```AB`c` — three backticks between G and
    // AB, one between B and c.
    let result = parse_with_mode("G```AB`c", ParseMode::Fragment);
    assert!(!result.has_errors());
    assert_eq!(notes(&result.value[0]).len(), 4);
}

#[test]
fn backticks_emit_no_skipping_warnings() {
    let result = parse_with_mode("G``A`B", ParseMode::Fragment);
    let skip = result
        .feedback
        .iter()
        .filter(|f| f.message.contains("Skipping unknown character"))
        .count();
    assert_eq!(skip, 0, "feedback: {:?}", result.feedback);
}
