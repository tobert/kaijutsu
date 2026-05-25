//! Tests for ABC v2.1 §3.3 `+:` continuation lines (in tune body).
//!
//! `+:` at line start appends its content to the previous field-line
//! element of the same kind. In the body, that means w:/W:/s:.

use kaijutsu_abc::{parse_with_mode, Element, ParseMode};

fn first_lyrics_text(tune: &kaijutsu_abc::Tune) -> Option<String> {
    tune.voices
        .iter()
        .flat_map(|v| v.elements.iter())
        .find_map(|e| match e {
            Element::Lyrics { text, .. } => Some(text.clone()),
            _ => None,
        })
}

#[test]
fn plus_colon_appends_to_lyrics() {
    let abc = "\
C D E F|\n\
w: line one\n\
+: line two\n\
";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    assert!(!result.has_errors(), "feedback: {:?}", result.feedback);

    // The two lines should be joined into a single Lyrics element.
    let lyrics_count = result.value.voices[0]
        .elements
        .iter()
        .filter(|e| matches!(e, Element::Lyrics { .. }))
        .count();
    assert_eq!(lyrics_count, 1, "expected one merged Lyrics element");
    assert_eq!(
        first_lyrics_text(&result.value),
        Some("line one\nline two".to_string())
    );
}

#[test]
fn plus_colon_appends_to_symbol_line() {
    let abc = "\
C D E F|\n\
s: \"C\" * \"F\" *\n\
+: \"G\" * \"C\" *\n\
";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    assert!(!result.has_errors(), "feedback: {:?}", result.feedback);

    let symbol_lines: Vec<_> = result.value.voices[0]
        .elements
        .iter()
        .filter_map(|e| match e {
            Element::SymbolLine(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(symbol_lines.len(), 1);
    assert!(symbol_lines[0].contains("\"C\""));
    assert!(symbol_lines[0].contains("\"G\""));
}

#[test]
fn plus_colon_emits_no_skipping_warnings() {
    let abc = "\
C D E F|\n\
w: verse one\n\
+: continued lyrics here\n\
";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    let skip_warnings: Vec<_> = result
        .feedback
        .iter()
        .filter(|f| f.message.contains("Skipping unknown character"))
        .collect();
    assert!(
        skip_warnings.is_empty(),
        "+: should consume the whole line, got: {:?}",
        skip_warnings,
    );
}

#[test]
fn plus_colon_without_preceding_field_warns() {
    let abc = "\
CDEF|\n\
+: nothing to continue\n\
";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    // We expect a warning, no Lyrics created, and no crash.
    assert!(!result.has_errors());
    let has_warning = result
        .feedback
        .iter()
        .any(|f| f.message.contains("'+:' continuation"));
    assert!(
        has_warning,
        "expected a continuation warning, got: {:?}",
        result.feedback,
    );
    assert!(first_lyrics_text(&result.value).is_none());
}

#[test]
fn multiple_plus_colon_continuations() {
    let abc = "\
C D E F|\n\
w: one\n\
+: two\n\
+: three\n\
";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    assert!(!result.has_errors(), "feedback: {:?}", result.feedback);
    assert_eq!(
        first_lyrics_text(&result.value),
        Some("one\ntwo\nthree".to_string())
    );
}
