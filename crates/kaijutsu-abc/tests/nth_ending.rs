//! Tests for ABC v2.1 §4.10 NthEnding bracket form `[1`, `[2`, `[1-3`,
//! `[1,3,5-7`. The `|N` / `:|N` forms were already supported; this adds
//! the square-bracket prefix used in many real-world tunes.

use kaijutsu_abc::{parse_with_mode, Bar, Element, ParseMode};

fn bars(tune: &kaijutsu_abc::Tune) -> Vec<Bar> {
    tune.voices
        .iter()
        .flat_map(|v| v.elements.iter())
        .filter_map(|e| match e {
            Element::Bar(b) => Some(b.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn bracket_one_is_first_ending() {
    let result = parse_with_mode("[1abc", ParseMode::Fragment);
    assert!(!result.has_errors());
    assert_eq!(bars(&result.value[0]), vec![Bar::FirstEnding]);
}

#[test]
fn bracket_two_is_second_ending() {
    let result = parse_with_mode("[2abc", ParseMode::Fragment);
    assert_eq!(bars(&result.value[0]), vec![Bar::SecondEnding]);
}

#[test]
fn bracket_range_emits_nth_ending() {
    // `[1-3` covers endings 1, 2, 3
    let result = parse_with_mode("[1-3abc", ParseMode::Fragment);
    assert_eq!(bars(&result.value[0]), vec![Bar::NthEnding(vec![1, 2, 3])]);
}

#[test]
fn bracket_list_emits_nth_ending() {
    // `[1,3,5` is an explicit set
    let result = parse_with_mode("[1,3,5abc", ParseMode::Fragment);
    assert_eq!(bars(&result.value[0]), vec![Bar::NthEnding(vec![1, 3, 5])]);
}

#[test]
fn bracket_list_with_range() {
    // `[1,3,5-7` combines list + range
    let result = parse_with_mode("[1,3,5-7abc", ParseMode::Fragment);
    assert_eq!(
        bars(&result.value[0]),
        vec![Bar::NthEnding(vec![1, 3, 5, 6, 7])]
    );
}

#[test]
fn bracket_ending_inside_full_tune() {
    let abc = "|: faf gfe |[1 dfe dBA :|[2 d2e dcB |]";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    assert!(!result.has_errors(), "feedback: {:?}", result.feedback);
    let bs = bars(&result.value[0]);
    assert!(bs.contains(&Bar::FirstEnding), "bars: {:?}", bs);
    assert!(bs.contains(&Bar::SecondEnding), "bars: {:?}", bs);
}

#[test]
fn bracket_ending_no_skipping_warnings() {
    let result = parse_with_mode("|[1abc:|[2def|]", ParseMode::Fragment);
    let skip = result
        .feedback
        .iter()
        .filter(|f| f.message.contains("Skipping unknown character"))
        .count();
    assert_eq!(skip, 0, "feedback: {:?}", result.feedback);
}

#[test]
fn bracket_chord_still_works() {
    // `[CEG]` is a chord — guard that NthEnding detection doesn't
    // greedy-match `[` when the next char isn't a digit.
    let result = parse_with_mode("[CEG]", ParseMode::Fragment);
    let chord_count = result.value[0].voices[0]
        .elements
        .iter()
        .filter(|e| matches!(e, Element::Chord(_)))
        .count();
    assert_eq!(chord_count, 1, "expected a Chord, got {:?}", result.value[0].voices[0].elements);
}
