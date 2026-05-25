//! Tests for ABC v2.1 §7.4 voice-overlay marker `&`.
//!
//! `&` starts a parallel run aligned to the music since the last bar.
//! Multiple `&` chars in a row (`&&`, `&&&`) define additional overlay
//! layers. The parser captures the count; rendering / playback
//! semantics are deferred.

use kaijutsu_abc::{parse_with_mode, Element, ParseMode};

fn overlays(tune: &kaijutsu_abc::Tune) -> Vec<u8> {
    tune.voices
        .iter()
        .flat_map(|v| v.elements.iter())
        .filter_map(|e| match e {
            Element::Overlay { layers } => Some(*layers),
            _ => None,
        })
        .collect()
}

#[test]
fn single_ampersand_emits_one_overlay() {
    let result = parse_with_mode("CDEF&GABc", ParseMode::Fragment);
    assert!(!result.has_errors(), "feedback: {:?}", result.feedback);
    assert_eq!(overlays(&result.value), vec![1]);
}

#[test]
fn double_ampersand_emits_two_layers() {
    let result = parse_with_mode("CDEF&&GABc", ParseMode::Fragment);
    assert!(!result.has_errors());
    assert_eq!(overlays(&result.value), vec![2]);
}

#[test]
fn triple_ampersand_emits_three_layers() {
    let result = parse_with_mode("CDEF&&&GABc", ParseMode::Fragment);
    assert!(!result.has_errors());
    assert_eq!(overlays(&result.value), vec![3]);
}

#[test]
fn overlay_at_line_start() {
    // Adapted from §7.4 example
    let abc = "g4 f4|e6 e2|\n&(d8|c6)c2|";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    assert!(!result.has_errors());
    assert_eq!(overlays(&result.value), vec![1]);
}

#[test]
fn overlay_emits_no_skipping_warnings() {
    let result = parse_with_mode("CDEF&GABc", ParseMode::Fragment);
    let skip = result
        .feedback
        .iter()
        .filter(|f| f.message.contains("Skipping unknown character"))
        .count();
    assert_eq!(skip, 0, "feedback: {:?}", result.feedback);
}
