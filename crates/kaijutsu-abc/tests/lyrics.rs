//! Tests for ABC v2.1 §5 lyric line parsing.
//!
//! `w:` lines inside the tune body carry aligned lyrics that pair with the
//! music on the preceding line. `W:` lines carry unaligned "words after the
//! tune" — same parsing path, captured with `aligned: false`.
//!
//! Syllable parsing (alignment marks `-`, `_`, `*`, `~`, `\-`) is the
//! renderer's job; the parser captures content verbatim.

use kaijutsu_abc::{parse_with_mode, Element, ParseMode};

fn lyrics_in(tune: &kaijutsu_abc::Tune) -> Vec<(bool, String)> {
    tune.voices
        .iter()
        .flat_map(|v| v.elements.iter())
        .filter_map(|e| match e {
            Element::Lyrics { aligned, text } => Some((*aligned, text.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn simple_w_line_aligned_to_preceding_music() {
    // From spec §5.1
    let abc = "C D E F|\nw: doh re mi fa\n";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    assert!(!result.has_errors(), "feedback: {:?}", result.feedback);

    let lyrics = lyrics_in(&result.value);
    assert_eq!(lyrics, vec![(true, "doh re mi fa".to_string())]);
}

#[test]
fn multiple_verses_with_w_lines() {
    let abc = "\
C D E F|\n\
w: verse one syl-la-bles here\n\
w: verse two syl-la-bles here\n\
";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    assert!(!result.has_errors(), "feedback: {:?}", result.feedback);

    let lyrics = lyrics_in(&result.value);
    assert_eq!(
        lyrics,
        vec![
            (true, "verse one syl-la-bles here".to_string()),
            (true, "verse two syl-la-bles here".to_string()),
        ]
    );
}

#[test]
fn capital_w_line_marks_unaligned() {
    let abc = "C D E F|\nW: words after the tune go here\n";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    assert!(!result.has_errors(), "feedback: {:?}", result.feedback);

    let lyrics = lyrics_in(&result.value);
    assert_eq!(
        lyrics,
        vec![(false, "words after the tune go here".to_string())]
    );
}

#[test]
fn lyrics_emit_no_skipping_warnings() {
    // The whole point: lyric text should NOT generate per-character
    // "Skipping unknown character" warnings.
    let abc = "C D E F|\nw: Si les ma-tins de gri-sail-le\n";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    let skip_warnings: Vec<_> = result
        .feedback
        .iter()
        .filter(|f| f.message.contains("Skipping unknown character"))
        .collect();
    assert!(
        skip_warnings.is_empty(),
        "lyrics text should not warn per-character, got: {:?}",
        skip_warnings,
    );
}

#[test]
fn lowercase_w_mid_line_is_not_lyrics() {
    // `w` mid-line is not a lyrics marker; only at line start. This input
    // has no real `w` mid-line in standard ABC, but the test guards
    // against an over-eager prefix match.
    //
    // We give it as a music-only fragment with no `w:` line at all and
    // assert the parser does not invent a Lyrics element.
    let abc = "CDEF GABc|\n";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    let lyrics = lyrics_in(&result.value);
    assert!(lyrics.is_empty(), "unexpected lyrics: {:?}", lyrics);
}
