//! ABC notation parser and MIDI generator.
//!
//! This crate provides tools for parsing ABC music notation into a structured
//! AST, and converting that AST to MIDI.
//!
//! # Example
//!
//! ```
//! use kaijutsu_abc::{parse, to_midi, MidiParams};
//!
//! let abc = r#"
//! X:1
//! T:Test Tune
//! M:4/4
//! L:1/8
//! K:G
//! GABc dedB|cBAG D2D2|
//! "#;
//!
//! let result = parse(abc);
//! if !result.has_errors() {
//!     let midi_bytes = to_midi(&result.value, &MidiParams::default());
//!     // midi_bytes is a valid SMF format 0 MIDI file
//! }
//! ```

pub mod ast;
pub mod engrave;
pub mod feedback;
pub mod midi;
pub mod parser;

pub use ast::*;
pub use feedback::{Feedback, FeedbackLevel, ParseResult};

/// Parse ABC notation into a Tune AST.
///
/// This is a generous parser that will attempt to continue parsing
/// even when encountering issues, collecting feedback along the way.
pub fn parse(input: &str) -> ParseResult<Tune> {
    parser::parse(input)
}

/// Parameters for MIDI generation
#[derive(Debug, Clone)]
pub struct MidiParams {
    /// MIDI velocity for notes (1-127)
    pub velocity: u8,
    /// Ticks per quarter note (typically 480)
    pub ticks_per_beat: u16,
    /// MIDI channel (0-15, default 0). Use 9 for GM drums.
    pub channel: u8,
    /// MIDI program number (0-127). If Some, a program change is emitted at track start.
    /// See General MIDI for standard mappings (e.g., 0=Piano, 33=Bass, 56=Trumpet).
    pub program: Option<u8>,
}

impl Default for MidiParams {
    fn default() -> Self {
        MidiParams {
            velocity: 80,
            ticks_per_beat: 480,
            channel: 0,
            program: None, // No program change by default (uses synth's default)
        }
    }
}

/// Convert a parsed Tune to MIDI bytes (SMF format 0)
pub fn to_midi(tune: &Tune, params: &MidiParams) -> Vec<u8> {
    midi::generate(tune, params)
}

/// Transpose a tune by the given number of semitones
pub fn transpose(tune: &Tune, semitones: i8) -> Tune {
    let mut result = tune.clone();

    // Transpose the key signature
    let key_semitone = result.header.key.root.to_semitone()
        + result
            .header
            .key
            .accidental
            .map(|a| a.to_semitone_offset())
            .unwrap_or(0);
    let new_key_semitone = key_semitone + semitones;
    let (new_root, new_acc) = NoteName::from_semitone(new_key_semitone);
    result.header.key.root = new_root;
    result.header.key.accidental = new_acc;

    // Transpose all notes in all voices
    for voice in &mut result.voices {
        for element in &mut voice.elements {
            transpose_element(element, semitones);
        }
    }

    result
}

fn transpose_element(element: &mut Element, semitones: i8) {
    match element {
        Element::Note(note) => transpose_note(note, semitones),
        Element::Chord(chord) => {
            for note in &mut chord.notes {
                transpose_note(note, semitones);
            }
        }
        Element::Tuplet(tuplet) => {
            for elem in &mut tuplet.elements {
                transpose_element(elem, semitones);
            }
        }
        Element::GraceNotes { notes, .. } => {
            for note in notes {
                transpose_note(note, semitones);
            }
        }
        _ => {}
    }
}

fn transpose_note(note: &mut Note, semitones: i8) {
    let base = note.pitch.to_semitone();
    let acc_offset = note.accidental.map(|a| a.to_semitone_offset()).unwrap_or(0);
    let current_pitch = (base + acc_offset) as i16 + (note.octave as i16 * 12);
    let new_pitch = current_pitch + semitones as i16;

    let new_octave = new_pitch.div_euclid(12) as i8;
    let new_semitone = new_pitch.rem_euclid(12) as i8;

    let (new_note, new_acc) = NoteName::from_semitone(new_semitone);
    note.pitch = new_note;
    note.accidental = new_acc;
    note.octave = new_octave;
}

/// Convert a Tune back to ABC notation string
pub fn to_abc(tune: &Tune) -> String {
    let mut output = String::new();

    // Header fields
    output.push_str(&format!("X:{}\n", tune.header.reference));
    output.push_str(&format!("T:{}\n", tune.header.title));

    for title in &tune.header.titles {
        output.push_str(&format!("T:{}\n", title));
    }

    if let Some(composer) = &tune.header.composer {
        output.push_str(&format!("C:{}\n", composer));
    }
    if let Some(rhythm) = &tune.header.rhythm {
        output.push_str(&format!("R:{}\n", rhythm));
    }

    if let Some(meter) = &tune.header.meter {
        output.push_str("M:");
        match meter {
            Meter::Simple {
                numerator,
                denominator,
            } => {
                output.push_str(&format!("{}/{}\n", numerator, denominator));
            }
            Meter::Common => output.push_str("C\n"),
            Meter::Cut => output.push_str("C|\n"),
            Meter::None => output.push_str("none\n"),
        }
    }

    if let Some(unit_length) = &tune.header.unit_length {
        output.push_str(&format!(
            "L:{}/{}\n",
            unit_length.numerator, unit_length.denominator
        ));
    }

    if let Some(tempo) = &tune.header.tempo {
        output.push_str(&format!(
            "Q:{}/{}={}\n",
            tempo.beat_unit.0, tempo.beat_unit.1, tempo.bpm
        ));
    }

    // Key (must be last in header)
    output.push_str("K:");
    output.push_str(format_note_name(&tune.header.key.root));
    if let Some(acc) = tune.header.key.accidental {
        output.push_str(format_accidental(&acc));
    }
    if tune.header.key.mode != Mode::Major {
        output.push_str(format_mode(&tune.header.key.mode));
    }
    output.push('\n');

    // Voice bodies
    for voice in &tune.voices {
        for element in &voice.elements {
            format_element(&mut output, element);
        }
    }

    output.push('\n');
    output
}

fn format_note_name(note: &NoteName) -> &'static str {
    match note {
        NoteName::C => "C",
        NoteName::D => "D",
        NoteName::E => "E",
        NoteName::F => "F",
        NoteName::G => "G",
        NoteName::A => "A",
        NoteName::B => "B",
    }
}

fn format_accidental(acc: &Accidental) -> &'static str {
    match acc {
        Accidental::Sharp => "#",
        Accidental::Flat => "b",
        Accidental::DoubleSharp => "##",
        Accidental::DoubleFlat => "bb",
        Accidental::Natural => "=",
    }
}

fn format_mode(mode: &Mode) -> &'static str {
    match mode {
        Mode::Major => "",
        Mode::Minor => "m",
        Mode::Ionian => "Ion",
        Mode::Dorian => "Dor",
        Mode::Phrygian => "Phr",
        Mode::Lydian => "Lyd",
        Mode::Mixolydian => "Mix",
        Mode::Aeolian => "Aeo",
        Mode::Locrian => "Loc",
    }
}

fn format_note(output: &mut String, note: &Note) {
    if let Some(acc) = note.accidental {
        output.push_str(format_accidental(&acc));
    }
    let note_name = format_note_name(&note.pitch);
    if note.octave >= 1 {
        output.push_str(&note_name.to_lowercase());
        for _ in 1..note.octave {
            output.push('\'');
        }
    } else {
        output.push_str(note_name);
        for _ in note.octave..0 {
            output.push(',');
        }
    }
    format_duration(output, &note.duration);
    if note.tie {
        output.push('-');
    }
}

fn format_element(output: &mut String, element: &Element) {
    match element {
        Element::Note(note) => format_note(output, note),
        Element::Chord(chord) => {
            output.push('[');
            for note in &chord.notes {
                if let Some(acc) = note.accidental {
                    output.push_str(format_accidental(&acc));
                }
                let note_name = format_note_name(&note.pitch);
                if note.octave >= 1 {
                    output.push_str(&note_name.to_lowercase());
                    for _ in 1..note.octave {
                        output.push('\'');
                    }
                } else {
                    output.push_str(note_name);
                    for _ in note.octave..0 {
                        output.push(',');
                    }
                }
            }
            output.push(']');
            format_duration(output, &chord.duration);
        }
        Element::Rest(rest) => {
            if rest.visible {
                if let Some(bars) = rest.multi_measure {
                    output.push_str(&format!("Z{}", bars));
                } else {
                    output.push('z');
                    format_duration(output, &rest.duration);
                }
            } else {
                output.push('x');
                format_duration(output, &rest.duration);
            }
        }
        Element::Bar(bar) => match bar {
            Bar::Single => output.push('|'),
            Bar::Double => output.push_str("||"),
            Bar::End => output.push_str("|]"),
            Bar::Start => output.push_str("[|"),
            Bar::RepeatStart => output.push_str("|:"),
            Bar::RepeatEnd => output.push_str(":|"),
            Bar::RepeatBoth => output.push_str("::"),
            Bar::FirstEnding => output.push_str("|1"),
            Bar::SecondEnding => output.push_str(":|2"),
            Bar::NthEnding(nums) => {
                output.push('|');
                for n in nums {
                    output.push_str(&n.to_string());
                }
            }
        },
        Element::LineBreak => output.push('\n'),
        Element::Space => output.push(' '),
        Element::ChordSymbol(symbol) => {
            output.push('"');
            output.push_str(symbol);
            output.push('"');
        }
        Element::Tuplet(tuplet) => {
            output.push_str(&format!("({}:{}", tuplet.p, tuplet.q));
            for elem in &tuplet.elements {
                format_element(output, elem);
            }
        }
        Element::GraceNotes { acciaccatura, notes } => {
            if *acciaccatura {
                output.push_str("{/");
            } else {
                output.push('{');
            }
            for note in notes {
                format_note(output, note);
            }
            output.push('}');
        }
        Element::InlineField(_)
        | Element::Decoration(_)
        | Element::Slur(_)
        | Element::VoiceSwitch(_) => {}
    }
}

fn format_duration(output: &mut String, duration: &Duration) {
    if duration.numerator == 1 && duration.denominator == 1 {
        return;
    }
    if duration.denominator == 1 {
        output.push_str(&duration.numerator.to_string());
    } else if duration.numerator == 1 {
        output.push('/');
        if duration.denominator != 2 {
            output.push_str(&duration.denominator.to_string());
        }
    } else {
        output.push_str(&format!("{}/{}", duration.numerator, duration.denominator));
    }
}

/// Calculate semitones needed to transpose from source key to target key
pub fn semitones_to_key(source: &Key, target: &str) -> Result<i8, String> {
    let target = target.trim();
    if target.is_empty() {
        return Err("Target key cannot be empty".to_string());
    }

    let chars: Vec<char> = target.chars().collect();
    let first_char = chars[0];
    let target_note = NoteName::parse(&first_char.to_string())
        .ok_or_else(|| format!("Invalid note name: {}", first_char))?;

    let mut pos = 1;
    let target_accidental = if pos < chars.len() && (chars[pos] == '#' || chars[pos] == 'b') {
        let acc_str = if pos + 1 < chars.len() && chars[pos] == chars[pos + 1] {
            pos += 2;
            if chars[pos - 1] == '#' {
                "##"
            } else {
                "bb"
            }
        } else {
            pos += 1;
            if chars[pos - 1] == '#' {
                "#"
            } else {
                "b"
            }
        };
        Accidental::parse(acc_str)
    } else {
        None
    };

    let source_semitone = source.root.to_semitone()
        + source
            .accidental
            .map(|a| a.to_semitone_offset())
            .unwrap_or(0);

    let target_semitone = target_note.to_semitone()
        + target_accidental
            .map(|a| a.to_semitone_offset())
            .unwrap_or(0);

    let diff = target_semitone - source_semitone;
    let normalized = if diff > 6 {
        diff - 12
    } else if diff < -6 {
        diff + 12
    } else {
        diff
    };

    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(abc: &str) -> String {
        let result = parse(abc);
        assert!(!result.has_errors(), "parse errors: {:?}", result.errors().collect::<Vec<_>>());
        to_abc(&result.value)
    }

    #[test]
    fn tuplet_round_trip_preserves_ratio() {
        let abc = "X:1\nT:Test\nM:4/4\nL:1/8\nK:C\n(3:2ABC\n";
        let output = round_trip(abc);
        assert!(output.contains("(3:2"), "expected tuplet (3:2, got: {}", output);
    }

    #[test]
    fn grace_notes_round_trip() {
        let abc = "X:1\nT:Test\nM:4/4\nL:1/8\nK:C\n{AB}c\n";
        let output = round_trip(abc);
        assert!(output.contains("{AB}"), "expected grace notes {{AB}}, got: {}", output);
    }

    #[test]
    fn acciaccatura_round_trip() {
        let abc = "X:1\nT:Test\nM:4/4\nL:1/8\nK:C\n{/A}B\n";
        let output = round_trip(abc);
        assert!(output.contains("{/A}"), "expected acciaccatura {{/A}}, got: {}", output);
    }

    #[test]
    fn chord_symbol_round_trip() {
        let abc = "X:1\nT:Test\nM:4/4\nL:1/8\nK:C\n\"Am\"A2\n";
        let output = round_trip(abc);
        assert!(output.contains("\"Am\""), "expected chord symbol \"Am\", got: {}", output);
    }
}
