//! Layout engine: walks ABC AST and produces positioned EngravingElements.

use crate::ast::*;
use crate::engrave::font::font_cache;
use crate::engrave::ir::{EngravingElement, EngravingOptions, SourceSpan};

/// Key signature accidental counts — same logic as midi.rs.
fn key_signature_info(key: &Key) -> (i8, bool) {
    let base = match (&key.root, &key.accidental) {
        (NoteName::C, None) => 0,
        (NoteName::G, None) => 1,
        (NoteName::D, None) => 2,
        (NoteName::A, None) => 3,
        (NoteName::E, None) => 4,
        (NoteName::B, None) => 5,
        (NoteName::F, Some(Accidental::Sharp)) => 6,
        (NoteName::C, Some(Accidental::Sharp)) => 7,
        (NoteName::F, None) => -1,
        (NoteName::B, Some(Accidental::Flat)) => -2,
        (NoteName::E, Some(Accidental::Flat)) => -3,
        (NoteName::A, Some(Accidental::Flat)) => -4,
        (NoteName::D, Some(Accidental::Flat)) => -5,
        (NoteName::G, Some(Accidental::Flat)) => -6,
        (NoteName::C, Some(Accidental::Flat)) => -7,
        _ => 0,
    };

    let mode_offset = match key.mode {
        Mode::Major | Mode::Ionian => 0,
        Mode::Minor | Mode::Aeolian => -3,
        Mode::Dorian => -2,
        Mode::Phrygian => -4,
        Mode::Lydian => 1,
        Mode::Mixolydian => -1,
        Mode::Locrian => -5,
    };

    let total = base + mode_offset;
    if total >= 0 {
        (total, true)
    } else {
        (-total, false)
    }
}

/// Staff positions for sharp key signatures (treble clef).
/// Each value is the staff position offset from the top line.
const SHARP_POSITIONS: &[f64] = &[0.0, 1.5, -0.5, 1.0, 2.5, 0.5, 2.0]; // F C G D A E B

/// Staff positions for flat key signatures (treble clef).
const FLAT_POSITIONS: &[f64] = &[2.0, 0.5, 2.5, 1.0, 3.0, 1.5, 3.5]; // B E A D G C F

/// Convert a note pitch + octave to a staff position (treble clef).
/// Position 0 = top line (F5), each increment = one half-step down on staff.
/// B4 = middle line = position 2.0.
fn note_to_staff_position(pitch: &NoteName, octave: i8) -> f64 {
    // Diatonic position within an octave (C=0, D=1, E=2, F=3, G=4, A=5, B=6)
    let diatonic = match pitch {
        NoteName::C => 0,
        NoteName::D => 1,
        NoteName::E => 2,
        NoteName::F => 3,
        NoteName::G => 4,
        NoteName::A => 5,
        NoteName::B => 6,
    };

    // Using Note::to_midi_pitch convention:
    //   octave 0 = uppercase (C3–B3), octave 1 = lowercase (C4–B4)
    // B4 (treble clef middle line) = (B, octave 1) → abs diatonic = 1*7+6 = 13
    // F5 (top line) = (F, octave 2) → abs diatonic = 2*7+3 = 17
    // E4 (bottom line) = (E, octave 1) → abs diatonic = 1*7+2 = 9
    // Middle C (C4) = (C, octave 1) → abs diatonic = 7 → pos 5.0 (ledger line below)

    let abs_diatonic = octave as i32 * 7 + diatonic;
    let b4_abs = 13i32; // B4 = (B, octave 1)

    // Each diatonic step = half a staff_spacing
    (b4_abs - abs_diatonic) as f64 * 0.5 + 2.0
}

/// Lay out a tune as engraving elements.
pub fn engrave(tune: &Tune, options: &EngravingOptions) -> Vec<EngravingElement> {
    let mut elements = Vec::new();
    let font = font_cache();
    let sp = options.staff_spacing;
    let scale = sp / font.upem() * 4.0; // Scale font units to staff spacing

    let staff_top = 0.0;
    let mut cursor_x: f64 = 0.0;

    // Title
    if !tune.header.title.is_empty() {
        elements.push(EngravingElement::Text {
            content: tune.header.title.clone(),
            x: 0.0,
            y: -sp * 2.0, // Above the staff
            size: sp * 1.8,
            source_span: (0, 0),
        });
    }

    // 1. Draw 5 staff lines
    // We don't know total width yet, so we'll adjust at the end.
    let staff_line_start_idx = elements.len();
    for i in 0..5 {
        let y = staff_top + i as f64 * sp;
        elements.push(EngravingElement::Line {
            x1: 0.0,
            y1: y,
            x2: 0.0, // placeholder — will be set to total width
            y2: y,
            width: 0.5,
            source_span: (0, 0),
        });
    }

    // 2. Treble clef
    if font.glyph_path(0xE050).is_some() {
        elements.push(EngravingElement::Glyph {
            codepoint: 0xE050,
            x: cursor_x,
            y: staff_top + 3.0 * sp, // Clef sits on 3rd line from top
            scale,
            source_span: (0, 0),
        });
    }
    cursor_x += sp * 3.5;

    // 3. Key signature
    let (count, is_sharp) = key_signature_info(&tune.header.key);
    let acc_codepoint = if is_sharp { 0xE262u32 } else { 0xE260u32 };
    let positions = if is_sharp {
        SHARP_POSITIONS
    } else {
        FLAT_POSITIONS
    };
    for i in 0..count as usize {
        if i >= positions.len() {
            break;
        }
        let y = staff_top + positions[i] * sp;
        elements.push(EngravingElement::Glyph {
            codepoint: acc_codepoint,
            x: cursor_x,
            y,
            scale,
            source_span: (0, 0),
        });
        cursor_x += sp * 1.0;
    }
    if count > 0 {
        cursor_x += sp * 0.5;
    }

    // 4. Time signature
    if let Some(meter) = &tune.header.meter {
        let (num, den) = meter.to_fraction();
        // Numerator centered between lines 1 and 2
        emit_time_sig_digit(&mut elements, num, cursor_x, staff_top + sp, scale);
        // Denominator centered between lines 3 and 4
        emit_time_sig_digit(&mut elements, den, cursor_x, staff_top + 3.0 * sp, scale);
        cursor_x += sp * 2.5;
    }

    cursor_x += sp * 0.5; // padding before first note

    // 5. Walk voice elements
    // Only handle first voice for v1
    if let Some(voice) = tune.voices.first() {
        let unit_length = tune.header.unit_length.unwrap_or_default();
        let unit_width = sp * 2.5; // Base width for one unit duration

        for element in &voice.elements {
            match element {
                Element::Note(note) => {
                    let span = (0usize, 0usize); // TODO: track source spans in parser
                    cursor_x = emit_note(
                        &mut elements,
                        note,
                        None, // no explicit accidental override
                        cursor_x,
                        staff_top,
                        sp,
                        scale,
                        unit_width,
                        &unit_length,
                        span,
                    );
                }
                Element::Chord(chord) => {
                    let span = (0usize, 0usize);
                    // Emit stacked noteheads with shared stem
                    let dur_width = duration_to_width(&chord.duration, unit_width);

                    for note in &chord.notes {
                        let pos = note_to_staff_position(&note.pitch, note.octave);
                        let y = staff_top + pos * sp;
                        let cp = notehead_codepoint(&chord.duration, &unit_length);
                        elements.push(EngravingElement::Glyph {
                            codepoint: cp,
                            x: cursor_x,
                            y,
                            scale,
                            source_span: span,
                        });
                        emit_ledger_lines(&mut elements, pos, cursor_x, staff_top, sp, span);
                    }

                    // Stem on the chord (use highest and lowest notes)
                    if absolute_ratio(&chord.duration, &unit_length) < 1.0 {
                        // Not a whole note: draw stem
                        if let (Some(first), Some(last)) = (chord.notes.first(), chord.notes.last())
                        {
                            let top_pos = note_to_staff_position(&first.pitch, first.octave);
                            let bot_pos = note_to_staff_position(&last.pitch, last.octave);
                            let (stem_top, stem_bot) = if top_pos < bot_pos {
                                (top_pos, bot_pos)
                            } else {
                                (bot_pos, top_pos)
                            };
                            let cp = notehead_codepoint(&chord.duration, &unit_length);
                            let nw = font.glyph_advance(cp).unwrap_or(500.0) * scale;
                            // Use average position to decide stem direction
                            let avg_pos = (stem_top + stem_bot) / 2.0;
                            let stem_x = if avg_pos <= 2.0 {
                                cursor_x // left edge, stem down
                            } else {
                                cursor_x + nw // right edge, stem up
                            };
                            let stem_dir = if avg_pos <= 2.0 { 1.0 } else { -1.0 };
                            elements.push(EngravingElement::Line {
                                x1: stem_x,
                                y1: staff_top + stem_top * sp,
                                x2: stem_x,
                                y2: staff_top + stem_bot * sp + stem_dir * sp * 3.5,
                                width: 0.8,
                                source_span: span,
                            });
                        }
                    }

                    cursor_x += dur_width;
                }
                Element::Rest(rest) => {
                    let span = (0usize, 0usize);
                    let cp = rest_codepoint(&rest.duration, &unit_length);
                    // Rest centered on the staff
                    let y = staff_top + 2.0 * sp;
                    elements.push(EngravingElement::Glyph {
                        codepoint: cp,
                        x: cursor_x,
                        y,
                        scale,
                        source_span: span,
                    });
                    let dur_width = if let Some(bars) = rest.multi_measure {
                        unit_width * bars as f64 * 4.0
                    } else {
                        duration_to_width(&rest.duration, unit_width)
                    };
                    cursor_x += dur_width;
                }
                Element::Bar(_) => {
                    let span = (0usize, 0usize);
                    // Vertical barline
                    elements.push(EngravingElement::Line {
                        x1: cursor_x,
                        y1: staff_top,
                        x2: cursor_x,
                        y2: staff_top + 4.0 * sp,
                        width: 1.0,
                        source_span: span,
                    });
                    cursor_x += sp * 1.0;
                }
                Element::Tuplet(tuplet) => {
                    let span = (0usize, 0usize);
                    let scale_factor = tuplet.q as f64 / tuplet.p as f64;
                    for elem in &tuplet.elements {
                        if let Element::Note(note) = elem {
                            // Scale the width by the tuplet ratio
                            let orig_width = duration_to_width(&note.duration, unit_width);
                            let scaled_width = orig_width * scale_factor;
                            let pos = note_to_staff_position(&note.pitch, note.octave);
                            let y = staff_top + pos * sp;
                            let cp = notehead_codepoint(&note.duration, &unit_length);
                            elements.push(EngravingElement::Glyph {
                                codepoint: cp,
                                x: cursor_x,
                                y,
                                scale,
                                source_span: span,
                            });
                            emit_ledger_lines(&mut elements, pos, cursor_x, staff_top, sp, span);
                            emit_stem(
                                &mut elements,
                                pos,
                                cursor_x,
                                staff_top,
                                sp,
                                scale,
                                &note.duration,
                                &unit_length,
                                span,
                            );
                            cursor_x += scaled_width;
                        }
                    }
                }
                Element::ChordSymbol(text) => {
                    elements.push(EngravingElement::Text {
                        content: text.clone(),
                        x: cursor_x,
                        y: staff_top - sp * 0.5,
                        size: sp * 1.2,
                        source_span: (0, 0),
                    });
                }
                // Space, LineBreak, decorations, slurs, etc. — skip for v1
                _ => {}
            }
        }
    }

    // Final barline
    elements.push(EngravingElement::Line {
        x1: cursor_x,
        y1: 0.0,
        x2: cursor_x,
        y2: 4.0 * sp,
        width: 2.0,
        source_span: (0, 0),
    });

    // Fix staff line widths
    for element in &mut elements[staff_line_start_idx..staff_line_start_idx + 5] {
        if let EngravingElement::Line { x2, .. } = element {
            *x2 = cursor_x;
        }
    }

    elements
}

fn emit_note(
    elements: &mut Vec<EngravingElement>,
    note: &Note,
    _acc_override: Option<Accidental>,
    cursor_x: f64,
    staff_top: f64,
    sp: f64,
    scale: f64,
    unit_width: f64,
    unit: &UnitLength,
    span: SourceSpan,
) -> f64 {
    let pos = note_to_staff_position(&note.pitch, note.octave);
    let y = staff_top + pos * sp;

    // Accidental glyph
    if let Some(acc) = note.accidental {
        let acc_cp = match acc {
            Accidental::Sharp | Accidental::DoubleSharp => 0xE262,
            Accidental::Flat | Accidental::DoubleFlat => 0xE260,
            Accidental::Natural => 0xE261,
        };
        elements.push(EngravingElement::Glyph {
            codepoint: acc_cp,
            x: cursor_x - sp * 0.8,
            y,
            scale,
            source_span: span,
        });
    }

    // Notehead
    let cp = notehead_codepoint(&note.duration, unit);
    elements.push(EngravingElement::Glyph {
        codepoint: cp,
        x: cursor_x,
        y,
        scale,
        source_span: span,
    });

    // Ledger lines
    emit_ledger_lines(elements, pos, cursor_x, staff_top, sp, span);

    // Stem
    emit_stem(
        elements,
        pos,
        cursor_x,
        staff_top,
        sp,
        scale,
        &note.duration,
        unit,
        span,
    );

    // Flag for 8th and 16th notes
    emit_flag(
        elements,
        pos,
        cursor_x,
        staff_top,
        sp,
        scale,
        &note.duration,
        unit,
        span,
    );

    let dur_width = duration_to_width(&note.duration, unit_width);
    cursor_x + dur_width
}

/// Compute absolute duration in whole notes: (note_dur * unit_length).
fn absolute_ratio(duration: &Duration, unit: &UnitLength) -> f64 {
    (duration.numerator as f64 * unit.numerator as f64)
        / (duration.denominator as f64 * unit.denominator as f64)
}

fn notehead_codepoint(duration: &Duration, unit: &UnitLength) -> u32 {
    let abs = absolute_ratio(duration, unit);
    if abs >= 1.0 {
        0xE0A2 // whole
    } else if abs >= 0.5 {
        0xE0A3 // half
    } else {
        0xE0A4 // quarter/filled
    }
}

fn rest_codepoint(duration: &Duration, unit: &UnitLength) -> u32 {
    let abs = absolute_ratio(duration, unit);
    if abs >= 1.0 {
        0xE4E3 // whole rest
    } else if abs >= 0.5 {
        0xE4E4 // half rest
    } else if abs >= 0.25 {
        0xE4E5 // quarter rest
    } else if abs >= 0.125 {
        0xE4E6 // eighth rest
    } else {
        0xE4E7 // sixteenth rest
    }
}

fn duration_to_width(duration: &Duration, unit_width: f64) -> f64 {
    let ratio = duration.numerator as f64 / duration.denominator as f64;
    (unit_width * ratio).max(unit_width * 0.25)
}

fn emit_ledger_lines(
    elements: &mut Vec<EngravingElement>,
    pos: f64,
    x: f64,
    staff_top: f64,
    sp: f64,
    span: SourceSpan,
) {
    let ledger_width = sp * 1.4;
    let lx1 = x - sp * 0.2;
    let lx2 = lx1 + ledger_width;

    // Above staff (pos < 0)
    let mut lp = -0.5;
    while lp >= pos {
        // Ledger lines at integer positions above: -0.5 rounds to 0 but we want
        // lines at positions ..., -1.0, -0.5 above top line
        // Actually: ledger lines at each full line position above/below staff
        if (lp * 2.0).round() as i32 % 2 == 0 {
            let y = staff_top + lp * sp;
            elements.push(EngravingElement::Line {
                x1: lx1,
                y1: y,
                x2: lx2,
                y2: y,
                width: 0.5,
                source_span: span,
            });
        }
        lp -= 0.5;
    }

    // Below staff (pos > 4.0, since staff spans positions 0..4 in line-unit)
    let mut lp = 4.5;
    while lp <= pos {
        if (lp * 2.0).round() as i32 % 2 == 0 {
            let y = staff_top + lp * sp;
            elements.push(EngravingElement::Line {
                x1: lx1,
                y1: y,
                x2: lx2,
                y2: y,
                width: 0.5,
                source_span: span,
            });
        }
        lp += 0.5;
    }
}

fn emit_stem(
    elements: &mut Vec<EngravingElement>,
    pos: f64,
    x: f64,
    staff_top: f64,
    sp: f64,
    scale: f64,
    duration: &Duration,
    unit: &UnitLength,
    span: SourceSpan,
) {
    let abs = absolute_ratio(duration, unit);
    if abs >= 1.0 {
        return; // Whole notes have no stem
    }

    let font = font_cache();
    let cp = notehead_codepoint(duration, unit);
    let notehead_width = font.glyph_advance(cp).unwrap_or(500.0) * scale;

    // Stem direction: notes on or above middle line get stems down, below get stems up
    let stem_length = sp * 3.5;
    let note_y = staff_top + pos * sp;

    if pos <= 2.0 {
        // Stem down: hangs from left side of notehead
        let stem_x = x;
        elements.push(EngravingElement::Line {
            x1: stem_x,
            y1: note_y,
            x2: stem_x,
            y2: note_y + stem_length,
            width: 0.8,
            source_span: span,
        });
    } else {
        // Stem up: rises from right side of notehead
        let stem_x = x + notehead_width;
        elements.push(EngravingElement::Line {
            x1: stem_x,
            y1: note_y,
            x2: stem_x,
            y2: note_y - stem_length,
            width: 0.8,
            source_span: span,
        });
    }
}

fn emit_flag(
    elements: &mut Vec<EngravingElement>,
    pos: f64,
    x: f64,
    staff_top: f64,
    sp: f64,
    scale: f64,
    duration: &Duration,
    unit: &UnitLength,
    span: SourceSpan,
) {
    let abs = absolute_ratio(duration, unit);

    let flag_cp = if abs <= 0.0625 {
        // 16th note (1/16 of a whole)
        if pos <= 2.0 {
            Some(0xE243u32) // 16th down
        } else {
            Some(0xE242u32) // 16th up
        }
    } else if abs <= 0.125 {
        // 8th note (1/8 of a whole)
        if pos <= 2.0 {
            Some(0xE241u32) // 8th down
        } else {
            Some(0xE240u32) // 8th up
        }
    } else {
        None
    };

    if let Some(cp) = flag_cp {
        let font = font_cache();
        let notehead_cp = notehead_codepoint(duration, unit);
        let notehead_width = font.glyph_advance(notehead_cp).unwrap_or(500.0) * scale;

        let stem_length = sp * 3.5;
        let note_y = staff_top + pos * sp;
        let stem_x = if pos <= 2.0 {
            x // left edge for stems down
        } else {
            x + notehead_width // right edge for stems up
        };
        let flag_y = if pos <= 2.0 {
            note_y + stem_length
        } else {
            note_y - stem_length
        };
        elements.push(EngravingElement::Glyph {
            codepoint: cp,
            x: stem_x,
            y: flag_y,
            scale,
            source_span: span,
        });
    }
}

fn emit_time_sig_digit(
    elements: &mut Vec<EngravingElement>,
    digit: u8,
    x: f64,
    y: f64,
    scale: f64,
) {
    // Time sig digits: U+E080 (0) through U+E089 (9)
    // For multi-digit numbers, we'd need to iterate, but meters rarely exceed 9
    if digit <= 9 {
        elements.push(EngravingElement::Glyph {
            codepoint: 0xE080 + digit as u32,
            x,
            y,
            scale,
            source_span: (0, 0),
        });
    } else {
        // Two-digit: emit tens then ones
        let tens = digit / 10;
        let ones = digit % 10;
        let font = font_cache();
        let advance = font.glyph_advance(0xE080).unwrap_or(500.0) * scale;
        elements.push(EngravingElement::Glyph {
            codepoint: 0xE080 + tens as u32,
            x,
            y,
            scale,
            source_span: (0, 0),
        });
        elements.push(EngravingElement::Glyph {
            codepoint: 0xE080 + ones as u32,
            x: x + advance,
            y,
            scale,
            source_span: (0, 0),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn middle_c_is_below_staff() {
        // Middle C (C4) = ABC (C, octave 1) → pos 5.0, one ledger line below
        let pos = note_to_staff_position(&NoteName::C, 1);
        assert!(
            (pos - 5.0).abs() < 0.01,
            "Middle C should be at pos 5.0, got {}",
            pos
        );
    }

    #[test]
    fn b4_is_middle_line() {
        // B4 = ABC (B, octave 1) = lowercase b → middle line
        let pos = note_to_staff_position(&NoteName::B, 1);
        assert!(
            (pos - 2.0).abs() < 0.01,
            "B4 should be at position 2.0, got {}",
            pos
        );
    }

    #[test]
    fn f5_is_top_line() {
        // F5 = ABC (F, octave 2) = f' → top line
        let pos = note_to_staff_position(&NoteName::F, 2);
        assert!(
            (pos - 0.0).abs() < 0.01,
            "F5 should be at position 0.0, got {}",
            pos
        );
    }

    #[test]
    fn e4_is_bottom_line() {
        // E4 = ABC (E, octave 1) = lowercase e → bottom line
        let pos = note_to_staff_position(&NoteName::E, 1);
        assert!(
            (pos - 4.0).abs() < 0.01,
            "E4 should be at position 4.0, got {}",
            pos
        );
    }

    // --- Notehead codepoint tests ---
    // SMuFL codepoints:
    const WHOLE_NOTEHEAD: u32 = 0xE0A2;
    const HALF_NOTEHEAD: u32 = 0xE0A3;
    const FILLED_NOTEHEAD: u32 = 0xE0A4;

    /// Helper: extract all notehead glyph codepoints from engraved output.
    fn notehead_codepoints(elements: &[EngravingElement]) -> Vec<u32> {
        elements
            .iter()
            .filter_map(|e| match e {
                EngravingElement::Glyph { codepoint, .. }
                    if [WHOLE_NOTEHEAD, HALF_NOTEHEAD, FILLED_NOTEHEAD].contains(codepoint) =>
                {
                    Some(*codepoint)
                }
                _ => None,
            })
            .collect()
    }

    /// Helper: check whether any flag glyphs (8th/16th) are present.
    fn has_flag_glyphs(elements: &[EngravingElement]) -> bool {
        elements.iter().any(|e| matches!(e,
            EngravingElement::Glyph { codepoint, .. }
                if (0xE240..=0xE243).contains(codepoint)
        ))
    }

    /// With L:1/8, bare notes (C D E) are eighth notes → filled noteheads + flags.
    #[test]
    fn eighth_notes_get_filled_noteheads() {
        let abc = "X:1\nT:Test\nM:4/4\nL:1/8\nK:C\nC D E\n";
        let tune = crate::parse(abc).value;
        let elements = engrave(&tune, &EngravingOptions::default());
        let heads = notehead_codepoints(&elements);
        assert_eq!(heads.len(), 3, "expected 3 noteheads, got {}", heads.len());
        for (i, &cp) in heads.iter().enumerate() {
            assert_eq!(
                cp, FILLED_NOTEHEAD,
                "note {} should be filled (eighth note), got 0x{:04X}",
                i, cp
            );
        }
        assert!(
            has_flag_glyphs(&elements),
            "eighth notes should have flag glyphs"
        );
    }

    /// With L:1/8, C2 is a quarter note → filled notehead, no flag.
    #[test]
    fn quarter_note_gets_filled_notehead() {
        let abc = "X:1\nT:Test\nM:4/4\nL:1/8\nK:C\nC2\n";
        let tune = crate::parse(abc).value;
        let elements = engrave(&tune, &EngravingOptions::default());
        let heads = notehead_codepoints(&elements);
        assert_eq!(heads.len(), 1, "expected 1 notehead, got {}", heads.len());
        assert_eq!(
            heads[0], FILLED_NOTEHEAD,
            "quarter note (L:1/8, C2) should be filled, got 0x{:04X}",
            heads[0]
        );
        assert!(
            !has_flag_glyphs(&elements),
            "quarter notes should NOT have flag glyphs"
        );
    }

    /// With L:1/8, C4 is a half note → half (hollow) notehead.
    #[test]
    fn half_note_gets_half_notehead() {
        let abc = "X:1\nT:Test\nM:4/4\nL:1/8\nK:C\nC4\n";
        let tune = crate::parse(abc).value;
        let elements = engrave(&tune, &EngravingOptions::default());
        let heads = notehead_codepoints(&elements);
        assert_eq!(heads.len(), 1, "expected 1 notehead, got {}", heads.len());
        assert_eq!(
            heads[0], HALF_NOTEHEAD,
            "half note (L:1/8, C4) should be half notehead, got 0x{:04X}",
            heads[0]
        );
    }

    /// With L:1/8, C8 is a whole note → whole notehead.
    #[test]
    fn whole_note_gets_whole_notehead() {
        let abc = "X:1\nT:Test\nM:4/4\nL:1/8\nK:C\nC8\n";
        let tune = crate::parse(abc).value;
        let elements = engrave(&tune, &EngravingOptions::default());
        let heads = notehead_codepoints(&elements);
        assert_eq!(heads.len(), 1, "expected 1 notehead, got {}", heads.len());
        assert_eq!(
            heads[0], WHOLE_NOTEHEAD,
            "whole note (L:1/8, C8) should be whole notehead, got 0x{:04X}",
            heads[0]
        );
    }

    /// With L:1/4, bare C is a quarter note → filled notehead.
    #[test]
    fn quarter_note_with_l14() {
        let abc = "X:1\nT:Test\nM:4/4\nL:1/4\nK:C\nC\n";
        let tune = crate::parse(abc).value;
        let elements = engrave(&tune, &EngravingOptions::default());
        let heads = notehead_codepoints(&elements);
        assert_eq!(heads.len(), 1);
        assert_eq!(
            heads[0], FILLED_NOTEHEAD,
            "quarter note (L:1/4, C) should be filled, got 0x{:04X}",
            heads[0]
        );
    }

    /// With L:1/4, C2 is a half note → half notehead.
    #[test]
    fn half_note_with_l14() {
        let abc = "X:1\nT:Test\nM:4/4\nL:1/4\nK:C\nC2\n";
        let tune = crate::parse(abc).value;
        let elements = engrave(&tune, &EngravingOptions::default());
        let heads = notehead_codepoints(&elements);
        assert_eq!(heads.len(), 1);
        assert_eq!(
            heads[0], HALF_NOTEHEAD,
            "half note (L:1/4, C2) should be half notehead, got 0x{:04X}",
            heads[0]
        );
    }

    /// Rest glyphs should also respect unit length.
    /// With L:1/8, z2 is a quarter rest (0xE4E5).
    #[test]
    fn quarter_rest_with_l18() {
        let abc = "X:1\nT:Test\nM:4/4\nL:1/8\nK:C\nz2\n";
        let tune = crate::parse(abc).value;
        let elements = engrave(&tune, &EngravingOptions::default());
        let rest_cps: Vec<u32> = elements
            .iter()
            .filter_map(|e| match e {
                EngravingElement::Glyph { codepoint, .. }
                    if (0xE4E3..=0xE4E7).contains(codepoint) =>
                {
                    Some(*codepoint)
                }
                _ => None,
            })
            .collect();
        assert_eq!(rest_cps.len(), 1, "expected 1 rest glyph");
        assert_eq!(
            rest_cps[0], 0xE4E5,
            "quarter rest (L:1/8, z2) should be 0xE4E5, got 0x{:04X}",
            rest_cps[0]
        );
    }
}
