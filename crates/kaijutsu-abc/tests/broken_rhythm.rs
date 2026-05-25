//! Tests for ABC v2.1 §4.4 broken rhythm operators `>` and `<`.
//!
//! `a>b`   → a×3/2, b×1/2  (single dot)
//! `a<b`   → a×1/2, b×3/2
//! `a>>b`  → a×7/4, b×1/4  (double dot)
//! `a<<b`  → a×1/4, b×7/4
//! `a>>>b` → a×15/8, b×1/8 (triple dot)
//! `a<<<b` → a×1/8, b×15/8

use kaijutsu_abc::{parse_with_mode, Duration, Element, ParseMode};

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

fn dur(num: u16, den: u16) -> Duration {
    Duration {
        numerator: num,
        denominator: den,
    }
}

#[test]
fn single_chevron_lengthens_left_shortens_right() {
    let result = parse_with_mode("a>b", ParseMode::Fragment);
    assert!(!result.has_errors(), "feedback: {:?}", result.feedback);
    let ns = notes(&result.value[0]);
    assert_eq!(ns.len(), 2, "elements: {:?}", result.value[0].voices[0].elements);
    assert_eq!(ns[0].duration, dur(3, 2));
    assert_eq!(ns[1].duration, dur(1, 2));
}

#[test]
fn single_chevron_reverse() {
    let result = parse_with_mode("a<b", ParseMode::Fragment);
    assert!(!result.has_errors(), "feedback: {:?}", result.feedback);
    let ns = notes(&result.value[0]);
    assert_eq!(ns[0].duration, dur(1, 2));
    assert_eq!(ns[1].duration, dur(3, 2));
}

#[test]
fn double_chevron() {
    let result = parse_with_mode("a>>b", ParseMode::Fragment);
    let ns = notes(&result.value[0]);
    assert_eq!(ns[0].duration, dur(7, 4));
    assert_eq!(ns[1].duration, dur(1, 4));
}

#[test]
fn triple_chevron() {
    let result = parse_with_mode("a>>>b", ParseMode::Fragment);
    let ns = notes(&result.value[0]);
    assert_eq!(ns[0].duration, dur(15, 8));
    assert_eq!(ns[1].duration, dur(1, 8));
}

#[test]
fn triple_chevron_reverse() {
    let result = parse_with_mode("a<<<b", ParseMode::Fragment);
    let ns = notes(&result.value[0]);
    assert_eq!(ns[0].duration, dur(1, 8));
    assert_eq!(ns[1].duration, dur(15, 8));
}

#[test]
fn whitespace_around_operator_allowed() {
    let result = parse_with_mode("a > b", ParseMode::Fragment);
    assert!(!result.has_errors(), "feedback: {:?}", result.feedback);
    let ns = notes(&result.value[0]);
    assert_eq!(ns.len(), 2);
    assert_eq!(ns[0].duration, dur(3, 2));
    assert_eq!(ns[1].duration, dur(1, 2));
}

#[test]
fn chained_pairs() {
    // From spec §4.4 fixture: each pair is independent
    let result = parse_with_mode("a>b c<d", ParseMode::Fragment);
    let ns = notes(&result.value[0]);
    assert_eq!(ns.len(), 4);
    assert_eq!(ns[0].duration, dur(3, 2));
    assert_eq!(ns[1].duration, dur(1, 2));
    assert_eq!(ns[2].duration, dur(1, 2));
    assert_eq!(ns[3].duration, dur(3, 2));
}

#[test]
fn broken_rhythm_scales_explicit_durations() {
    // §4.4: rhythm scales whatever duration the notes already had
    let result = parse_with_mode("a2>b", ParseMode::Fragment);
    let ns = notes(&result.value[0]);
    assert_eq!(ns[0].duration, dur(6, 2)); // 2 * 3/2 = 3 (kept as 6/2)
    assert_eq!(ns[1].duration, dur(1, 2));
}

#[test]
fn broken_rhythm_emits_no_skipping_warnings() {
    let result = parse_with_mode("a>b c<d", ParseMode::Fragment);
    let skip = result
        .feedback
        .iter()
        .filter(|f| f.message.contains("Skipping unknown character"))
        .count();
    assert_eq!(skip, 0, "feedback: {:?}", result.feedback);
}

#[test]
fn lone_chevron_without_following_note_falls_through() {
    // `a>` with no follow-up — operator should fall through to the
    // unknown-character path so we know something is malformed.
    let result = parse_with_mode("a>", ParseMode::Fragment);
    let ns = notes(&result.value[0]);
    assert_eq!(ns.len(), 1, "should only have the leading note");
    // Should warn about the stray operator
    let has_warning = result
        .feedback
        .iter()
        .any(|f| f.message.contains("Skipping unknown character"));
    assert!(has_warning, "stray > should warn, got: {:?}", result.feedback);
}
