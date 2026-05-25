//! Tests for ABC v2.1 §4.15 `s:` symbol-line parsing.
//!
//! `s:` lines pair symbols (decorations, chord symbols, annotations,
//! skip-marks `*`, bar-aligns `|`) with notes on the preceding music
//! line. The parser captures the content verbatim; symbol-level parsing
//! is the renderer's job.

use kaijutsu_abc::{parse_with_mode, Element, ParseMode};

fn symbol_lines_in(tune: &kaijutsu_abc::Tune) -> Vec<String> {
    tune.voices
        .iter()
        .flat_map(|v| v.elements.iter())
        .filter_map(|e| match e {
            Element::SymbolLine(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn simple_s_line_captured() {
    // From spec §4.15
    let abc = "C2  C2 Ez   A2|\ns: \"C\" *  \"Am\" * |\n";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    assert!(!result.has_errors(), "feedback: {:?}", result.feedback);

    let symbols = symbol_lines_in(&result.value);
    assert_eq!(symbols, vec!["\"C\" *  \"Am\" * |".to_string()]);
}

#[test]
fn s_lines_emit_no_skipping_warnings() {
    let abc = "C D E F|\ns: !p! * !mf! *\n";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    let skip_warnings: Vec<_> = result
        .feedback
        .iter()
        .filter(|f| f.message.contains("Skipping unknown character"))
        .collect();
    assert!(
        skip_warnings.is_empty(),
        "symbol-line content should not warn per-character, got: {:?}",
        skip_warnings,
    );
}

#[test]
fn s_line_does_not_collide_with_note_s() {
    // There is no `s` as a single-letter note name, but guard against
    // an over-eager match that consumes mid-line `s:` as a symbol line.
    // This input has a pure-music fragment with no `s:` line.
    let abc = "CDEF GABc|\n";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    assert!(symbol_lines_in(&result.value).is_empty());
}

#[test]
fn s_and_w_lines_coexist() {
    let abc = "C D E F|\ns: \"C\" * \"F\" *\nw: doh re mi fa\n";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    assert!(!result.has_errors(), "feedback: {:?}", result.feedback);

    let elements: Vec<_> = result.value.voices[0].elements.iter().collect();
    let has_symbol = elements
        .iter()
        .any(|e| matches!(e, Element::SymbolLine(_)));
    let has_lyrics = elements
        .iter()
        .any(|e| matches!(e, Element::Lyrics { .. }));
    assert!(has_symbol && has_lyrics, "elements: {:?}", elements);
}
