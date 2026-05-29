//! Tests for clef-aware engraving and multi-voice rendering.

use kaijutsu_abc::engrave::{layout, EngravingElement, EngravingOptions};
use kaijutsu_abc::parse;

const TREBLE_CLEF: u32 = 0xE050;
const BASS_CLEF: u32 = 0xE062;
const C_CLEF: u32 = 0xE05C;
const FILLED_NOTEHEAD: u32 = 0xE0A4;
const SHARP: u32 = 0xE262;

fn engrave_default(abc: &str) -> Vec<EngravingElement> {
    let result = parse(abc);
    assert!(
        !result.has_errors(),
        "parse errors: {:?}",
        result.errors().collect::<Vec<_>>()
    );
    layout::engrave(&result.value[0], &EngravingOptions::default())
}

fn glyphs(elements: &[EngravingElement], cp: u32) -> Vec<(f64, f64)> {
    elements
        .iter()
        .filter_map(|e| match e {
            EngravingElement::Glyph { codepoint, x, y, .. } if *codepoint == cp => {
                Some((*x, *y))
            }
            _ => None,
        })
        .collect()
}

fn horizontal_staff_lines(elements: &[EngravingElement]) -> Vec<f64> {
    elements
        .iter()
        .filter_map(|e| match e {
            EngravingElement::Line { x1, y1, x2, y2, .. } if (y1 - y2).abs() < 0.01 && (x2 - x1).abs() > 1.0 => {
                Some(*y1)
            }
            _ => None,
        })
        .collect()
}

// --- Clef glyph selection ---------------------------------------------------

#[test]
fn default_clef_is_treble() {
    let els = engrave_default("X:1\nT:Test\nK:C\nC|\n");
    assert_eq!(glyphs(&els, TREBLE_CLEF).len(), 1, "default to treble");
    assert_eq!(glyphs(&els, BASS_CLEF).len(), 0);
    assert_eq!(glyphs(&els, C_CLEF).len(), 0);
}

#[test]
fn bass_clef_from_k_attribute() {
    let els = engrave_default("X:1\nT:Test\nK:C clef=bass\nC|\n");
    assert_eq!(glyphs(&els, BASS_CLEF).len(), 1, "should render F-clef");
    assert_eq!(glyphs(&els, TREBLE_CLEF).len(), 0);
}

#[test]
fn alto_clef_uses_c_clef_glyph() {
    let els = engrave_default("X:1\nT:Test\nK:C clef=alto\nC|\n");
    assert_eq!(glyphs(&els, C_CLEF).len(), 1, "should render C-clef");
}

#[test]
fn tenor_clef_uses_c_clef_glyph() {
    let els = engrave_default("X:1\nT:Test\nK:C clef=tenor\nC|\n");
    assert_eq!(glyphs(&els, C_CLEF).len(), 1, "should render C-clef");
}

// --- Clef glyph Y placement -------------------------------------------------
//
// Default sp = 10.0. Staff top y = 0.0, bottom y = 4*sp = 40.0.
// Conventional clef placements (the clef's reference line):
//   Treble (G clef wraps G4): 2nd line from bottom = pos 3.0 → y = 30.0
//   Bass (F clef sits on F3): 4th line from bottom = pos 1.0 → y = 10.0
//   Alto (C clef centers C4): middle line = pos 2.0 → y = 20.0
//   Tenor (C clef centers C4): 4th line from bottom = pos 1.0 → y = 10.0

#[test]
fn treble_clef_sits_at_g4_line() {
    let els = engrave_default("X:1\nT:Test\nK:C\nC|\n");
    let (_x, y) = glyphs(&els, TREBLE_CLEF)[0];
    assert!((y - 30.0).abs() < 0.01, "treble at y=30.0, got {}", y);
}

#[test]
fn bass_clef_sits_at_f3_line() {
    let els = engrave_default("X:1\nT:Test\nK:C clef=bass\nC|\n");
    let (_x, y) = glyphs(&els, BASS_CLEF)[0];
    assert!((y - 10.0).abs() < 0.01, "bass at y=10.0, got {}", y);
}

#[test]
fn alto_and_tenor_share_glyph_but_differ_in_y() {
    let alto = engrave_default("X:1\nT:Test\nK:C clef=alto\nC|\n");
    let tenor = engrave_default("X:1\nT:Test\nK:C clef=tenor\nC|\n");
    let (_, alto_y) = glyphs(&alto, C_CLEF)[0];
    let (_, tenor_y) = glyphs(&tenor, C_CLEF)[0];
    assert!((alto_y - 20.0).abs() < 0.01, "alto at y=20.0, got {}", alto_y);
    assert!((tenor_y - 10.0).abs() < 0.01, "tenor at y=10.0, got {}", tenor_y);
}

// --- Clef-aware notehead positioning ----------------------------------------
//
// D3 in ABC is `D,` — uppercase D is the middle-C-octave D4 (ABC octave 0 =
// C4–B4), and a single comma drops it to octave -1 = D3. (Feeding bare `D`
// here is a D4, an octave too high — the trap these tests fell into before
// the octave-mapping correction.)
//
// In bass clef, D3 sits exactly on the middle line: pos 2.0 → y = 2*sp = 20.0.
// In treble clef the same D3 lands six positions below the middle line (B4) —
// well below the bottom line: pos 8.0 → y = 80.0.

#[test]
fn d3_lands_on_bass_middle_line() {
    // `D,` = (D, octave -1) = D3. K:none clef=bass keeps the key signature
    // out of the way so we only get the notehead glyph at the expected y.
    let els = engrave_default("X:1\nT:Test\nK:none clef=bass\nD,\n");
    let notes = glyphs(&els, FILLED_NOTEHEAD);
    assert_eq!(notes.len(), 1, "expected one notehead");
    let (_, y) = notes[0];
    assert!(
        (y - 20.0).abs() < 0.01,
        "D3 should be on bass middle line (y=20.0), got {}",
        y
    );
}

#[test]
fn d3_lands_below_treble_staff() {
    // Same note (D3), treble clef — should be well below the 40.0 bottom line.
    let els = engrave_default("X:1\nT:Test\nK:none\nD,\n");
    let notes = glyphs(&els, FILLED_NOTEHEAD);
    assert_eq!(notes.len(), 1);
    let (_, y) = notes[0];
    assert!(
        y > 50.0,
        "D3 in treble should be well below staff (y > 50.0), got {}",
        y
    );
}

// --- Clef-aware key signature ----------------------------------------------
//
// G major has one sharp on F. In treble, F# sits on the top line (F5, pos 0.0).
// In bass clef, F# sits on the 4th line from bottom (F3, pos 1.0 → y = 10.0).

#[test]
fn g_major_treble_sharp_on_top_line() {
    let els = engrave_default("X:1\nT:Test\nK:G\nG|\n");
    let sharps = glyphs(&els, SHARP);
    // Filter out any in-line accidentals — there shouldn't be any here.
    assert!(!sharps.is_empty(), "expected at least 1 sharp");
    assert!(
        (sharps[0].1 - 0.0).abs() < 0.01,
        "F# in treble at y=0.0 (top line), got {}",
        sharps[0].1
    );
}

#[test]
fn g_major_bass_sharp_on_f3_line() {
    let els = engrave_default("X:1\nT:Test\nK:G clef=bass\nG,|\n");
    let sharps = glyphs(&els, SHARP);
    assert!(!sharps.is_empty(), "expected at least 1 sharp");
    assert!(
        (sharps[0].1 - 10.0).abs() < 0.01,
        "F# in bass at y=10.0 (4th line from bottom), got {}",
        sharps[0].1
    );
}

// --- Multi-voice rendering --------------------------------------------------

#[test]
fn two_voices_produce_two_staves() {
    let abc = "\
X:1
T:Two Voices
M:4/4
L:1/4
V:1 clef=treble
V:2 clef=bass
K:C
V:1
cdef|
V:2
C,D,E,F,|
";
    let els = engrave_default(abc);
    let staff_ys = horizontal_staff_lines(&els);

    // Each staff has 5 horizontal lines (plus possible ledgers — those are
    // shorter and filtered by the >1.0 length check, but ledger lines for
    // single notes are short enough that they're still counted here).
    // Group nearby Y values into staves: any two staff lines on the same
    // staff are within 4*sp = 40.0 of each other.
    //
    // Easier check: there should be exactly one treble clef glyph and one
    // bass clef glyph, at different Y values.
    let treble = glyphs(&els, TREBLE_CLEF);
    let bass = glyphs(&els, BASS_CLEF);
    assert_eq!(treble.len(), 1, "one treble for V:1");
    assert_eq!(bass.len(), 1, "one bass for V:2");

    // Bass staff must sit below treble staff.
    assert!(
        bass[0].1 > treble[0].1,
        "bass y ({}) should be greater than treble y ({})",
        bass[0].1,
        treble[0].1
    );

    // And there should be at least 10 horizontal staff lines (5 per voice).
    let long_lines = staff_ys.len();
    assert!(
        long_lines >= 10,
        "expected ≥10 horizontal lines for two staves, got {}",
        long_lines
    );
}

/// A 4-part hymn written with the voice declared *inline in the body*
/// (`V:T clef=bass name="Tenor"` on its own line, music on the next),
/// and no `K:` field at all — the shape models give us most often.
///
/// Regression: the standalone body `V:` handler used to consume only the
/// voice id and leave `clef=… name="…"` in the stream, where it was lexed
/// as stray notes ("clef" → C,E,F …). That corrupted the first voice and
/// swallowed every voice after it, so the tenor/bass lines surfaced as raw
/// text instead of staves. The attribute tail must be consumed *and* its
/// clef applied, so all four voices render with the right clef.
#[test]
fn four_part_hymn_inline_voice_attributes() {
    let abc = "\
X:1
M:4/4
L:1/4
%%score (S A T B)
V:S clef=treble name=\"Soprano\"
| G A B c |
V:A clef=treble name=\"Alto\"
| E F G A |
V:T clef=bass name=\"Tenor\"
| C D E F |
V:B clef=bass name=\"Bass\"
| C, D, E, F, |
";
    let result = parse(abc);
    let tune = &result.value[0];

    // All four voices must materialize, in declaration order.
    let ids: Vec<_> = tune
        .voices
        .iter()
        .filter_map(|v| v.id.clone())
        .collect();
    assert_eq!(ids, vec!["S", "A", "T", "B"], "four voices in order");

    // No attribute text leaked as music: the soprano's first note is G4,
    // not the C/E/F that "clef" would lex into.
    let first_note = tune.voices[0]
        .elements
        .iter()
        .find_map(|e| match e {
            kaijutsu_abc::ast::Element::Note(n) => Some(n.pitch),
            _ => None,
        })
        .expect("soprano has a note");
    assert_eq!(
        first_note,
        kaijutsu_abc::ast::NoteName::G,
        "soprano's first note is G, not leaked attribute text"
    );

    // Clefs are honored: S/A treble, T/B bass.
    let els = engrave_default(abc);
    assert_eq!(
        glyphs(&els, TREBLE_CLEF).len(),
        2,
        "soprano + alto on treble"
    );
    assert_eq!(glyphs(&els, BASS_CLEF).len(), 2, "tenor + bass on bass clef");
}

#[test]
fn voice_clef_overrides_header_clef() {
    // Header says treble, V:2 overrides to bass.
    let abc = "\
X:1
T:Mixed
M:4/4
L:1/4
V:1
V:2 clef=bass
K:C
V:1
c|
V:2
C,|
";
    let els = engrave_default(abc);
    assert_eq!(glyphs(&els, TREBLE_CLEF).len(), 1, "V:1 stays treble");
    assert_eq!(glyphs(&els, BASS_CLEF).len(), 1, "V:2 is bass");
}
