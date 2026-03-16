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

    let abs_diatonic = octave as i32 * 7 + diatonic as i32;
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
        let _unit_length = tune.header.unit_length.unwrap_or_default();
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
                        span,
                    );
                }
                Element::Chord(chord) => {
                    let span = (0usize, 0usize);
                    // Emit stacked noteheads with shared stem
                    let dur_width =
                        duration_to_width(&chord.duration, unit_width);

                    for note in &chord.notes {
                        let pos = note_to_staff_position(&note.pitch, note.octave);
                        let y = staff_top + pos * sp;
                        let cp = notehead_codepoint(&chord.duration);
                        elements.push(EngravingElement::Glyph {
                            codepoint: cp,
                            x: cursor_x,
                            y,
                            scale,
                            source_span: span,
                        });
                        emit_ledger_lines(
                            &mut elements,
                            pos,
                            cursor_x,
                            staff_top,
                            sp,
                            span,
                        );
                    }

                    // Stem on the chord (use highest and lowest notes)
                    if chord.duration.denominator > 0
                        && !(chord.duration.numerator >= 4 && chord.duration.denominator == 1)
                    {
                        // Not a whole note: draw stem
                        if let (Some(first), Some(last)) =
                            (chord.notes.first(), chord.notes.last())
                        {
                            let top_pos = note_to_staff_position(&first.pitch, first.octave);
                            let bot_pos = note_to_staff_position(&last.pitch, last.octave);
                            let (stem_top, stem_bot) = if top_pos < bot_pos {
                                (top_pos, bot_pos)
                            } else {
                                (bot_pos, top_pos)
                            };
                            let cp = notehead_codepoint(&chord.duration);
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
                    let cp = rest_codepoint(&rest.duration);
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
                    let scale_factor =
                        tuplet.q as f64 / tuplet.p as f64;
                    for elem in &tuplet.elements {
                        if let Element::Note(note) = elem {
                            // Scale the width by the tuplet ratio
                            let orig_width =
                                duration_to_width(&note.duration, unit_width);
                            let scaled_width = orig_width * scale_factor;
                            let pos = note_to_staff_position(&note.pitch, note.octave);
                            let y = staff_top + pos * sp;
                            let cp = notehead_codepoint(&note.duration);
                            elements.push(EngravingElement::Glyph {
                                codepoint: cp,
                                x: cursor_x,
                                y,
                                scale,
                                source_span: span,
                            });
                            emit_ledger_lines(
                                &mut elements,
                                pos,
                                cursor_x,
                                staff_top,
                                sp,
                                span,
                            );
                            emit_stem(
                                &mut elements,
                                pos,
                                cursor_x,
                                staff_top,
                                sp,
                                scale,
                                &note.duration,
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
    for i in staff_line_start_idx..staff_line_start_idx + 5 {
        if let EngravingElement::Line { x2, .. } = &mut elements[i] {
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
    let cp = notehead_codepoint(&note.duration);
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
    emit_stem(elements, pos, cursor_x, staff_top, sp, scale, &note.duration, span);

    // Flag for 8th and 16th notes
    emit_flag(elements, pos, cursor_x, staff_top, sp, scale, &note.duration, span);

    let dur_width = duration_to_width(&note.duration, unit_width);
    cursor_x + dur_width
}

fn notehead_codepoint(duration: &Duration) -> u32 {
    // whole note: numerator >= 4 and denominator == 1 (i.e. 4/1 = whole, but in ABC
    // duration is relative to unit length). For simplicity:
    // Duration 4/1 or more → whole; 2/1 → half; else filled
    let ratio = duration.numerator as f64 / duration.denominator as f64;
    if ratio >= 4.0 {
        0xE0A2 // whole
    } else if ratio >= 2.0 {
        0xE0A3 // half
    } else {
        0xE0A4 // quarter/filled
    }
}

fn rest_codepoint(duration: &Duration) -> u32 {
    let ratio = duration.numerator as f64 / duration.denominator as f64;
    if ratio >= 4.0 {
        0xE4E3 // whole rest
    } else if ratio >= 2.0 {
        0xE4E4 // half rest
    } else if ratio >= 1.0 {
        0xE4E5 // quarter rest
    } else if ratio >= 0.5 {
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
    span: SourceSpan,
) {
    let ratio = duration.numerator as f64 / duration.denominator as f64;
    if ratio >= 4.0 {
        return; // Whole notes have no stem
    }

    let font = font_cache();
    let cp = notehead_codepoint(duration);
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
    span: SourceSpan,
) {
    let ratio = duration.numerator as f64 / duration.denominator as f64;

    let flag_cp = if ratio <= 0.25 {
        // 16th note
        if pos <= 2.0 {
            Some(0xE243u32) // 16th down
        } else {
            Some(0xE242u32) // 16th up
        }
    } else if ratio <= 0.5 {
        // 8th note
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
        let notehead_cp = notehead_codepoint(duration);
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
        assert!((pos - 5.0).abs() < 0.01, "Middle C should be at pos 5.0, got {}", pos);
    }

    #[test]
    fn b4_is_middle_line() {
        // B4 = ABC (B, octave 1) = lowercase b → middle line
        let pos = note_to_staff_position(&NoteName::B, 1);
        assert!((pos - 2.0).abs() < 0.01, "B4 should be at position 2.0, got {}", pos);
    }

    #[test]
    fn f5_is_top_line() {
        // F5 = ABC (F, octave 2) = f' → top line
        let pos = note_to_staff_position(&NoteName::F, 2);
        assert!((pos - 0.0).abs() < 0.01, "F5 should be at position 0.0, got {}", pos);
    }

    #[test]
    fn e4_is_bottom_line() {
        // E4 = ABC (E, octave 1) = lowercase e → bottom line
        let pos = note_to_staff_position(&NoteName::E, 1);
        assert!((pos - 4.0).abs() < 0.01, "E4 should be at position 4.0, got {}", pos);
    }
}
