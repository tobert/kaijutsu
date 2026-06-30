//! Round 2 reliability tests: the parser/MIDI/round-trip pipeline must never
//! panic, must emit structurally-valid SMF, and must not leave hung notes —
//! regardless of how malformed or adversarial the input is. These guard the
//! "completely reliable" bar before we touch rendering.

use kaijutsu_abc::midi::events;
use kaijutsu_abc::{parse, parse_with_mode, to_abc, to_midi, MidiParams, ParseMode};

/// Inputs chosen to probe the ugly corners: empty/partial headers, bad fields,
/// huge counts, unbalanced brackets, zero denominators (division traps), control
/// characters and non-ASCII. None of these may panic anywhere in the pipeline.
const ADVERSARIAL: &[&str] = &[
    "",
    "X:",
    "X:1",
    "X:1\nK:C\n",
    "K:Q\nCDEF",                      // invalid key root
    "X:1\nT:t\nK:C\nZ999999|",        // enormous multi-measure rest
    "X:1\nT:t\nK:C\n(99999abc|",      // enormous tuplet p
    "X:1\nT:t\nK:C\n[[[[[[CEG",       // unbalanced chord brackets
    "X:1\nT:t\nK:C\n((((((CDE",       // unbalanced slurs
    "X:1\nT:t\nK:C\n{{{{{ga",         // unbalanced grace braces
    "X:1\nT:t\nK:C\nA////////////|",  // many slash-divisors
    "X:1\nT:t\nK:C\n^^^^^^^^A|",      // accidental pile-up
    "X:1\nT:t\nL:1/0\nK:C\nA|",       // zero unit-length denominator
    "X:1\nT:t\nM:0/0\nK:C\nZ|",       // zero meter — Z bar-ticks division
    "X:1\nT:t\nQ:0/0=0\nK:C\nA|",     // zero tempo / beat unit
    "X:1\nT:t\nK:C\nA0 B0|",          // zero-length notes
    "X:1\nT:t\nK:C\n|::::::|::::|",   // repeat-marker soup
    "X:1\nT:t\nK:C\n|1|2|3|4|5|",     // endings with no bodies
    "X:1\nT:日本語\nK:C\nCDEF|",      // non-ASCII title
    "X:1\nT:t\nK:C\nC\0D\x01E\x02|",  // embedded control chars
    "X:1\nT:t\nV:1\nV:2\nV:3\nK:C\n[V:1]C[V:2]E[V:3]G|", // many voices
];

fn all_modes() -> [ParseMode; 3] {
    [ParseMode::Strict, ParseMode::Generous, ParseMode::Fragment]
}

#[test]
fn pipeline_never_panics_on_adversarial_input() {
    let params = MidiParams::default();
    for input in ADVERSARIAL {
        for mode in all_modes() {
            let result = parse_with_mode(input, mode);
            // Round-trip every parsed tune through MIDI and back to ABC. The
            // point is that none of these calls panic or hang.
            for tune in &result.value {
                let midi = to_midi(tune, &params);
                assert!(midi.len() >= 14, "MIDI shorter than a header for {input:?}");
                let _ = events(tune, &params);
                let abc = to_abc(tune);
                // The rendered ABC must itself be parseable without panicking.
                let _ = parse(&abc);
            }
        }
    }
}

/// Decode a single MidiEvent stream and assert NoteOns and NoteOffs balance per
/// (channel, pitch) — i.e. no note is left ringing (the hung-note class).
fn assert_notes_balanced(tune: &kaijutsu_abc::Tune, label: &str) {
    use std::collections::HashMap;
    let evs = events(tune, &MidiParams::default());
    let mut open: HashMap<(u8, u8), i32> = HashMap::new();
    for e in &evs {
        let status = e.data[0] & 0xF0;
        let chan = e.data[0] & 0x0F;
        match status {
            0x90 if e.data.get(2).copied().unwrap_or(0) > 0 => {
                *open.entry((chan, e.data[1])).or_default() += 1;
            }
            0x80 | 0x90 => {
                let slot = open.entry((chan, e.data[1])).or_default();
                *slot -= 1;
                assert!(*slot >= 0, "{label}: NoteOff with no matching NoteOn");
            }
            _ => {}
        }
    }
    for (k, v) in open {
        assert_eq!(v, 0, "{label}: note {k:?} left hanging (count {v})");
    }
}

#[test]
fn no_hung_notes_across_constructs() {
    // Ties, slurs, chords, tuplets, repeats and variant endings all share the
    // note-on/off bookkeeping; none may leave a note ringing.
    let cases = [
        ("plain", "CDEF|"),
        ("tie", "C-C|"),
        ("tie across bar", "C-|C|"),
        ("tie+accidental across bar", "^C-|C|"),
        ("chord", "[CEG][FAc]|"),
        ("chord with length", "[c4a4]G|"),
        ("slur", "(CDE)|"),
        ("nested slur", "(C(DE)F)|"),
        ("tuplet", "(3CDE F|"),
        ("tuplet with rest", "(3zDE|"),
        ("simple repeat", "|:CD:|"),
        ("variant endings", "|:CD|1E:|2F|"),
        ("inline key", "C|[K:G]F|"),
    ];
    for (label, body) in cases {
        let abc = format!("X:1\nT:t\nM:4/4\nL:1/4\nK:C\n{body}\n");
        let tune = &parse(&abc).value[0];
        assert_notes_balanced(tune, label);
    }
}

/// Walk an SMF byte blob and assert its chunk framing is self-consistent:
/// an MThd header of the right size, then exactly the declared number of MTrk
/// chunks whose lengths add up to the whole blob.
fn assert_smf_well_framed(midi: &[u8], label: &str) {
    assert!(midi.len() >= 14, "{label}: too short for an MThd header");
    assert_eq!(&midi[0..4], b"MThd", "{label}: bad MThd magic");
    let header_len = u32::from_be_bytes([midi[4], midi[5], midi[6], midi[7]]);
    assert_eq!(header_len, 6, "{label}: MThd length must be 6");
    let ntracks = u16::from_be_bytes([midi[10], midi[11]]) as usize;
    assert!(ntracks >= 1, "{label}: at least one track expected");

    let mut pos = 8 + header_len as usize;
    for t in 0..ntracks {
        assert!(pos + 8 <= midi.len(), "{label}: track {t} header runs off the end");
        assert_eq!(&midi[pos..pos + 4], b"MTrk", "{label}: track {t} bad MTrk magic");
        let len = u32::from_be_bytes([
            midi[pos + 4],
            midi[pos + 5],
            midi[pos + 6],
            midi[pos + 7],
        ]) as usize;
        pos += 8 + len;
        assert!(pos <= midi.len(), "{label}: track {t} length overruns the blob");
    }
    assert_eq!(pos, midi.len(), "{label}: trailing bytes after the last track");
}

#[test]
fn smf_framing_valid_including_multivoice() {
    let cases = [
        ("single voice", "X:1\nT:t\nM:4/4\nL:1/4\nK:C\nCDEF|\n"),
        (
            "two voices",
            "X:1\nT:t\nM:4/4\nL:1/4\nV:1\nV:2\nK:C\n[V:1]CEGc|\n[V:2]C,E,G,C|\n",
        ),
        ("compound meter Z", "X:1\nT:t\nM:6/8\nL:1/8\nK:C\nZ2 GAB|\n"),
        ("repeat+endings", "X:1\nT:t\nM:4/4\nL:1/4\nK:C\n|:CD|1E:|2F|\n"),
    ];
    let params = MidiParams::default();
    for (label, abc) in cases {
        let tune = &parse(abc).value[0];
        assert_smf_well_framed(&to_midi(tune, &params), label);
    }
}

/// Pitch sequence produced by a tune's MIDI, for round-trip comparison.
fn pitch_sequence(tune: &kaijutsu_abc::Tune) -> Vec<u8> {
    events(tune, &MidiParams::default())
        .iter()
        .filter(|e| e.data[0] & 0xF0 == 0x90 && e.data.get(2).copied().unwrap_or(0) > 0)
        .map(|e| e.data[1])
        .collect()
}

#[test]
fn round_trip_preserves_pitch_sequence() {
    // parse → to_abc → parse must preserve the sounding pitches. This is where
    // an asymmetry between the writer and the parser (e.g. accidental spelling,
    // octave marks, chord/duration syntax) would show up.
    let tunes = [
        "X:1\nT:t\nM:4/4\nL:1/8\nK:G\nGABc dedB|cBAG D2D2|\n",
        "X:1\nT:t\nM:4/4\nL:1/4\nK:C\n^C _D =E ^^F __G|\n",
        "X:1\nT:t\nM:4/4\nL:1/4\nK:D\nDEFG ABcd|\n",
        "X:1\nT:t\nM:4/4\nL:1/4\nK:C\nC,D,E,F, CDEF cdef c'd'e'f'|\n",
        "X:1\nT:t\nM:4/4\nL:1/4\nK:Bb\nBcde|\n",
    ];
    for abc in tunes {
        let tune = &parse(abc).value[0];
        let before = pitch_sequence(tune);
        let rendered = to_abc(tune);
        let reparsed = &parse(&rendered).value[0];
        let after = pitch_sequence(reparsed);
        assert_eq!(
            before, after,
            "round-trip changed pitches\n  src: {abc:?}\n  rendered: {rendered:?}"
        );
    }
}
