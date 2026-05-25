//! Tests for ABC v2.1 §6.1.1 trailing-backslash line continuation.
//!
//! A `\` immediately before a newline joins the following physical
//! line into the current logical line — no LineBreak is emitted into
//! the element stream, and field-line markers (`w:`, `s:`, `+:`) on
//! the next line are NOT treated as line-start markers.

use kaijutsu_abc::{parse_with_mode, Element, ParseMode};

fn elements(tune: &kaijutsu_abc::Tune) -> &[Element] {
    &tune.voices[0].elements
}

#[test]
fn trailing_backslash_consumes_newline() {
    let abc = "CDEF\\\nGABc";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    assert!(!result.has_errors(), "feedback: {:?}", result.feedback);
    let has_linebreak = elements(&result.value)
        .iter()
        .any(|e| matches!(e, Element::LineBreak));
    assert!(
        !has_linebreak,
        "trailing \\ should suppress LineBreak. elements: {:?}",
        elements(&result.value),
    );
}

#[test]
fn trailing_backslash_with_crlf() {
    let abc = "CDEF\\\r\nGABc";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    assert!(!result.has_errors(), "feedback: {:?}", result.feedback);
    let has_linebreak = elements(&result.value)
        .iter()
        .any(|e| matches!(e, Element::LineBreak));
    assert!(!has_linebreak);
}

#[test]
fn trailing_backslash_emits_no_skipping_warnings() {
    let abc = "CDEF&\\\nGABc";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    let skip = result
        .feedback
        .iter()
        .filter(|f| f.message.contains("Skipping unknown character"))
        .count();
    assert_eq!(skip, 0, "feedback: {:?}", result.feedback);
}

#[test]
fn backslash_not_at_end_still_warns() {
    // A stray `\` mid-line is still unknown.
    let abc = "C\\D";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    let has_warning = result
        .feedback
        .iter()
        .any(|f| f.message.contains("Skipping unknown character '\\'"));
    assert!(
        has_warning,
        "stray \\ mid-line should warn, got: {:?}",
        result.feedback,
    );
}

#[test]
fn continuation_disables_line_start_handlers_on_next_line() {
    // After a `\` continuation, the next physical line is mid-music —
    // a "w:" prefix there is just text, not a lyrics marker.
    let abc = "CDEF\\\nw:not_a_lyrics_line";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    let lyrics_count = elements(&result.value)
        .iter()
        .filter(|e| matches!(e, Element::Lyrics { .. }))
        .count();
    assert_eq!(
        lyrics_count, 0,
        "should not parse as lyrics after continuation, elements: {:?}",
        elements(&result.value),
    );
}
