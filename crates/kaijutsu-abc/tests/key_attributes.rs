//! Tests for ABC v2.1 §4.6 K: field attributes.
//!
//! K: accepts attribute clauses after the key signature:
//!   clef=<name>, transpose=<int>, octave=<int>, stafflines=<int>,
//!   middle=<pitch>. Bare clef shorthands (bass, treble, alto, tenor)
//!   are also accepted. Special cases: `K: clef=alto` (no root), `K:perc`
//!   (percussion shorthand).

use kaijutsu_abc::{parse_with_mode, Clef, ParseMode};

fn header_key(abc: &str) -> kaijutsu_abc::Key {
    let result = parse_with_mode(abc, ParseMode::Fragment);
    assert!(
        !result.has_errors(),
        "parse errors: {:?}",
        result.errors().collect::<Vec<_>>()
    );
    result.value[0].header.key.clone()
}

#[test]
fn k_transpose_minus_2() {
    let key = header_key("X:1\nT:Test\nK:Am transpose=-2\n");
    assert_eq!(key.transpose, -2);
    assert_eq!(key.root, kaijutsu_abc::NoteName::A);
    assert_eq!(key.mode, kaijutsu_abc::Mode::Minor);
}

#[test]
fn k_transpose_positive() {
    let key = header_key("X:1\nT:Test\nK:G transpose=5\n");
    assert_eq!(key.transpose, 5);
}

#[test]
fn k_clef_alto_with_no_root() {
    // The §4.6 sample fixture uses `K: clef=alto` to change clef without
    // changing the key signature. Defaults to C major.
    let key = header_key("X:1\nT:Test\nK: clef=alto\n");
    assert_eq!(key.clef, Some(Clef::Alto));
    assert_eq!(key.root, kaijutsu_abc::NoteName::C);
}

#[test]
fn k_perc_shorthand() {
    // K:perc is a percussion shorthand. We capture clef=Percussion.
    let key = header_key("X:1\nT:Test\nK: perc stafflines=1\n");
    assert_eq!(key.clef, Some(Clef::Percussion));
    assert_eq!(key.stafflines, Some(1));
}

#[test]
fn k_octave_attribute() {
    let key = header_key("X:1\nT:Test\nK:G octave=1\n");
    assert_eq!(key.octave, 1);
}

#[test]
fn k_middle_attribute() {
    let key = header_key("X:1\nT:Test\nK:F middle=B\n");
    assert_eq!(key.middle.as_deref(), Some("B"));
}

#[test]
fn k_bare_bass_clef_shorthand() {
    // `bass` alone (no `clef=`) is recognized as the bass clef.
    let key = header_key("X:1\nT:Test\nK:G bass\n");
    assert_eq!(key.clef, Some(Clef::Bass));
}

#[test]
fn k_combined_attributes() {
    let key = header_key("X:1\nT:Test\nK:Am transpose=-2 clef=alto\n");
    assert_eq!(key.transpose, -2);
    assert_eq!(key.clef, Some(Clef::Alto));
    assert_eq!(key.root, kaijutsu_abc::NoteName::A);
    assert_eq!(key.mode, kaijutsu_abc::Mode::Minor);
}

#[test]
fn k_attributes_emit_no_skipping_warnings() {
    let abc = "X:1\nT:Test\nK: clef=alto\nCDEF|\n";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    let skip: Vec<_> = result
        .feedback
        .iter()
        .filter(|f| f.message.contains("Skipping unknown character"))
        .collect();
    assert!(
        skip.is_empty(),
        "K: attributes should not produce per-char warnings, got: {:?}",
        skip,
    );
}
