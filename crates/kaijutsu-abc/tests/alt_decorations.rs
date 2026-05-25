//! Tests for ABC v2.1 §4.14 alternate decoration syntax `+name+`.
//!
//! `+trill+` is equivalent to `!trill!`; the alternate form exists
//! because some legacy dialects used `!` for line breaks.

use kaijutsu_abc::{parse_with_mode, Decoration, Dynamic, Element, ParseMode};

fn decorations(tune: &kaijutsu_abc::Tune) -> Vec<Decoration> {
    tune.voices
        .iter()
        .flat_map(|v| v.elements.iter())
        .filter_map(|e| match e {
            Element::Decoration(d) => Some(d.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn plus_trill_equivalent_to_bang_trill() {
    let result = parse_with_mode("+trill+C", ParseMode::Fragment);
    assert!(!result.has_errors(), "feedback: {:?}", result.feedback);
    assert_eq!(decorations(&result.value[0]), vec![Decoration::Trill]);
}

#[test]
fn plus_dynamics() {
    let result = parse_with_mode("+pp+C +ff+D", ParseMode::Fragment);
    assert!(!result.has_errors());
    assert_eq!(
        decorations(&result.value[0]),
        vec![
            Decoration::Dynamic(Dynamic::PP),
            Decoration::Dynamic(Dynamic::FF),
        ]
    );
}

#[test]
fn plus_unknown_falls_to_other() {
    let result = parse_with_mode("+novelty+C", ParseMode::Fragment);
    assert!(!result.has_errors());
    assert_eq!(
        decorations(&result.value[0]),
        vec![Decoration::Other("novelty".to_string())]
    );
}

#[test]
fn plus_decorations_emit_no_skipping_warnings() {
    let result = parse_with_mode("+f+ABCD +mp+EFGA", ParseMode::Fragment);
    let skip = result
        .feedback
        .iter()
        .filter(|f| f.message.contains("Skipping unknown character"))
        .count();
    assert_eq!(skip, 0, "feedback: {:?}", result.feedback);
}

#[test]
fn unmatched_plus_does_not_consume() {
    // A `+` with no closing `+` before end-of-line is not a decoration
    // (and shouldn't be silently swallowed). The fallback should fire.
    let result = parse_with_mode("C+D", ParseMode::Fragment);
    let has_warning = result
        .feedback
        .iter()
        .any(|f| f.message.contains("Skipping unknown character '+'"));
    assert!(
        has_warning,
        "expected fallback for unmatched +, got: {:?}",
        result.feedback,
    );
}
