//! Tests for note decoration rendering — staccato, trill, fermata,
//! up/down bow, dynamics.

use kaijutsu_abc::engrave::{layout, EngravingElement, EngravingOptions};
use kaijutsu_abc::parse;

fn engrave(abc: &str) -> Vec<EngravingElement> {
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

// SMuFL codepoints (subset we expect Bravura to have):
const STACCATO: u32 = 0xE4A2;       // articStaccatoAbove
const ACCENT: u32 = 0xE4A0;         // articAccentAbove
const FERMATA: u32 = 0xE4C0;        // fermataAbove
const TRILL: u32 = 0xE566;          // ornamentTrill
const UP_BOW: u32 = 0xE612;         // stringsUpBow
const DOWN_BOW: u32 = 0xE610;       // stringsDownBow
const DYN_P: u32 = 0xE520;          // dynamicPiano
const DYN_F: u32 = 0xE522;          // dynamicForte
const DYN_FF: u32 = 0xE52F;         // dynamicFF

// --- Single-character short-form decorations -------------------------------

#[test]
fn staccato_emits_dot_glyph() {
    // `.C` puts a staccato on the following C.
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n.c|\n");
    assert_eq!(glyphs(&els, STACCATO).len(), 1, "expected staccato glyph");
}

// The short forms `H`, `T`, `u`, `v` are blocked by a parser-side quirk:
// the body parser refuses to recognise them when the following character
// is alphabetic (i.e. always, in real use). Long-form `!fermata!`,
// `!trill!`, `!upbow!`, `!downbow!` work fine and are exercised below.
// Until the parser is loosened, these short forms are unreachable by the
// renderer regardless of what we do here.
#[test]
#[ignore = "parser: short-form H/T/u/v reject any alphabetic follower"]
fn fermata_short_form_emits_glyph() {
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\nHc|\n");
    assert_eq!(glyphs(&els, FERMATA).len(), 1);
}

#[test]
#[ignore = "parser: short-form H/T/u/v reject any alphabetic follower"]
fn trill_short_form_emits_glyph() {
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\nTc|\n");
    assert_eq!(glyphs(&els, TRILL).len(), 1);
}

#[test]
#[ignore = "parser: short-form H/T/u/v reject any alphabetic follower"]
fn up_bow_emits_glyph() {
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\nuc|\n");
    assert_eq!(glyphs(&els, UP_BOW).len(), 1);
}

#[test]
#[ignore = "parser: short-form H/T/u/v reject any alphabetic follower"]
fn down_bow_emits_glyph() {
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\nvc|\n");
    assert_eq!(glyphs(&els, DOWN_BOW).len(), 1);
}

// Long-form aliases for the same decorations exercise the renderer path
// for bow marks and confirm they reach the SMuFL codepoints.
#[test]
fn bang_upbow_emits_glyph() {
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n!upbow!c|\n");
    assert_eq!(glyphs(&els, UP_BOW).len(), 1);
}

#[test]
fn bang_downbow_emits_glyph() {
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n!downbow!c|\n");
    assert_eq!(glyphs(&els, DOWN_BOW).len(), 1);
}

// --- Long-form !name! decorations ------------------------------------------

#[test]
fn bang_trill_emits_glyph() {
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n!trill!c|\n");
    assert_eq!(glyphs(&els, TRILL).len(), 1);
}

#[test]
fn bang_fermata_emits_glyph() {
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n!fermata!c|\n");
    assert_eq!(glyphs(&els, FERMATA).len(), 1);
}

#[test]
fn bang_accent_emits_glyph() {
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n!accent!c|\n");
    assert_eq!(glyphs(&els, ACCENT).len(), 1);
}

// --- Dynamics --------------------------------------------------------------

#[test]
fn forte_dynamic_emits_glyph() {
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n!f!c|\n");
    assert_eq!(glyphs(&els, DYN_F).len(), 1, "expected dynamic forte glyph");
}

#[test]
fn piano_dynamic_emits_glyph() {
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n!p!c|\n");
    assert_eq!(glyphs(&els, DYN_P).len(), 1);
}

#[test]
fn ff_dynamic_emits_glyph() {
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n!ff!c|\n");
    assert_eq!(glyphs(&els, DYN_FF).len(), 1);
}

#[test]
fn dynamic_sits_below_staff() {
    // Dynamics conventionally go below the staff (y > bottom line = 40).
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n!f!c|\n");
    let (_, y) = glyphs(&els, DYN_F)[0];
    assert!(
        y > 40.0,
        "dynamic should be below the staff (y > 40), got {}",
        y
    );
}

// --- Placement -------------------------------------------------------------

#[test]
fn decoration_on_high_note_goes_above() {
    // c is below middle line (pos 5.0) → stem up → decoration goes above.
    // Wait, convention: decorations go *opposite* of stem. Stem-up means
    // decoration above. So .c — staccato above the c notehead, but c is
    // BELOW the staff, so the staccato is between staff and notehead.
    // For a clearer test use a high note like g' which is on the top
    // line — stem-down → staccato above the top line.
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n.g'|\n");
    let (_, y) = glyphs(&els, STACCATO)[0];
    // g' is at pos -0.5 (just above top line) → y = -5. Staccato should
    // sit further above, so y < -5.
    assert!(
        y < -5.0,
        "staccato on high note should be above the note (y < -5), got {}",
        y
    );
}

#[test]
fn multiple_decorations_stack() {
    // !p!.c → both piano and staccato on c. Both glyphs should appear.
    let els = engrave("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n!p!.c|\n");
    assert_eq!(glyphs(&els, DYN_P).len(), 1, "piano present");
    assert_eq!(glyphs(&els, STACCATO).len(), 1, "staccato present");
}
