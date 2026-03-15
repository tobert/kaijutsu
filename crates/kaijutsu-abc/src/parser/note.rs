//! Note, chord, and rest parsing using winnow combinators.

use winnow::combinator::{alt, opt, repeat};
use winnow::prelude::*;
use winnow::token::{one_of, take_while};

use crate::ast::{Accidental, Chord, Duration, Note, NoteName, Rest};

type PResult<T> = winnow::ModalResult<T>;

/// Parse a note pitch and base octave from ABC notation.
/// Uppercase = octave 0, lowercase = octave 1
pub fn parse_pitch(input: &mut &str) -> PResult<(NoteName, i8)> {
    // Use one_of to only consume valid pitch characters
    let c = one_of([
        'C', 'D', 'E', 'F', 'G', 'A', 'B', 'c', 'd', 'e', 'f', 'g', 'a', 'b',
    ])
    .parse_next(input)?;
    match c {
        'C' => Ok((NoteName::C, 0)),
        'D' => Ok((NoteName::D, 0)),
        'E' => Ok((NoteName::E, 0)),
        'F' => Ok((NoteName::F, 0)),
        'G' => Ok((NoteName::G, 0)),
        'A' => Ok((NoteName::A, 0)),
        'B' => Ok((NoteName::B, 0)),
        'c' => Ok((NoteName::C, 1)),
        'd' => Ok((NoteName::D, 1)),
        'e' => Ok((NoteName::E, 1)),
        'f' => Ok((NoteName::F, 1)),
        'g' => Ok((NoteName::G, 1)),
        'a' => Ok((NoteName::A, 1)),
        'b' => Ok((NoteName::B, 1)),
        _ => unreachable!(), // one_of already validated the character
    }
}

/// Parse an accidental (^, ^^, _, __, =)
pub fn parse_accidental(input: &mut &str) -> PResult<Accidental> {
    alt((
        "^^".map(|_| Accidental::DoubleSharp),
        "^".map(|_| Accidental::Sharp),
        "__".map(|_| Accidental::DoubleFlat),
        "_".map(|_| Accidental::Flat),
        "=".map(|_| Accidental::Natural),
    ))
    .parse_next(input)
}

/// Parse octave modifiers (', ,)
pub fn parse_octave_modifier(input: &mut &str) -> PResult<i8> {
    let ups: Vec<_> = repeat(0.., '\'').parse_next(input)?;
    let downs: Vec<_> = repeat(0.., ',').parse_next(input)?;
    Ok(ups.len() as i8 - downs.len() as i8)
}

/// Parse a duration (2, /2, 3/2, etc.)
pub fn parse_duration(input: &mut &str) -> PResult<Duration> {
    // Try to parse multiplier
    let multiplier_str: &str = take_while(0.., |c: char| c.is_ascii_digit()).parse_next(input)?;
    let multiplier: Option<u16> = if multiplier_str.is_empty() {
        None
    } else {
        multiplier_str.parse().ok()
    };

    // Try to parse divisor
    let divisor = opt(parse_divisor).parse_next(input)?;

    let num = multiplier.unwrap_or(1);
    let den = match divisor {
        Some(Some(d)) => d,
        Some(None) => 2, // A/ means A/2
        None => 1,
    };

    Ok(Duration {
        numerator: num,
        denominator: den,
    })
}

/// Parse the divisor part of a duration (/2, /, /4)
fn parse_divisor(input: &mut &str) -> PResult<Option<u16>> {
    '/'.parse_next(input)?;
    let den_str: &str = take_while(0.., |c: char| c.is_ascii_digit()).parse_next(input)?;
    if den_str.is_empty() {
        Ok(None)
    } else {
        Ok(den_str.parse().ok())
    }
}

/// Parse a complete note
pub fn parse_note(input: &mut &str) -> PResult<Note> {
    let accidental = opt(parse_accidental).parse_next(input)?;
    let (pitch, base_octave) = parse_pitch(input)?;
    let octave_mod = parse_octave_modifier(input)?;
    let duration = parse_duration(input)?;
    let tie = opt('-').parse_next(input)?.is_some();

    Ok(Note {
        pitch,
        octave: base_octave + octave_mod,
        accidental,
        duration,
        tie,
        decorations: Vec::new(),
    })
}

/// Parse a rest (z, x, Z)
pub fn parse_rest(input: &mut &str) -> PResult<Rest> {
    let rest_char = one_of(['z', 'x', 'Z']).parse_next(input)?;

    match rest_char {
        'Z' => {
            // Multi-measure rest
            let count_str: &str =
                take_while(0.., |c: char| c.is_ascii_digit()).parse_next(input)?;
            let count: u16 = count_str.parse().unwrap_or(1);
            Ok(Rest {
                duration: Duration::unit(),
                visible: true,
                multi_measure: Some(count),
            })
        }
        c => {
            let duration = parse_duration(input)?;
            Ok(Rest {
                duration,
                visible: c == 'z',
                multi_measure: None,
            })
        }
    }
}

/// Parse a chord [CEG]
pub fn parse_chord(input: &mut &str) -> PResult<Chord> {
    '['.parse_next(input)?;

    let mut notes = Vec::new();
    loop {
        // Skip spaces
        *input = input.trim_start_matches(' ');

        // Try to parse a note
        if let Ok(note) = parse_note.parse_next(input) {
            notes.push(note);
        } else {
            break;
        }
    }

    ']'.parse_next(input)?;

    // Duration after chord applies to the whole chord
    let duration = parse_duration(input)?;

    Ok(Chord { notes, duration })
}

/// Parse a chord symbol "G", "Am7", etc.
pub fn parse_chord_symbol(input: &mut &str) -> PResult<String> {
    '"'.parse_next(input)?;
    let symbol: &str = take_while(1.., |c: char| c != '"').parse_next(input)?;
    '"'.parse_next(input)?;
    Ok(symbol.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_pitch() {
        let mut input = "C";
        let result = parse_pitch(&mut input).unwrap();
        assert_eq!(result, (NoteName::C, 0));

        let mut input = "c";
        let result = parse_pitch(&mut input).unwrap();
        assert_eq!(result, (NoteName::C, 1));
    }

    #[test]
    fn test_parse_accidental() {
        let mut input = "^";
        assert_eq!(parse_accidental(&mut input).unwrap(), Accidental::Sharp);

        let mut input = "^^";
        assert_eq!(
            parse_accidental(&mut input).unwrap(),
            Accidental::DoubleSharp
        );

        let mut input = "_";
        assert_eq!(parse_accidental(&mut input).unwrap(), Accidental::Flat);

        let mut input = "__";
        assert_eq!(
            parse_accidental(&mut input).unwrap(),
            Accidental::DoubleFlat
        );

        let mut input = "=";
        assert_eq!(parse_accidental(&mut input).unwrap(), Accidental::Natural);
    }

    #[test]
    fn test_parse_octave_modifier() {
        let mut input = "'";
        assert_eq!(parse_octave_modifier(&mut input).unwrap(), 1);

        let mut input = "''";
        assert_eq!(parse_octave_modifier(&mut input).unwrap(), 2);

        let mut input = ",";
        assert_eq!(parse_octave_modifier(&mut input).unwrap(), -1);

        let mut input = ",,";
        assert_eq!(parse_octave_modifier(&mut input).unwrap(), -2);

        let mut input = "";
        assert_eq!(parse_octave_modifier(&mut input).unwrap(), 0);
    }

    #[test]
    fn test_parse_duration() {
        let mut input = "2";
        assert_eq!(parse_duration(&mut input).unwrap(), Duration::new(2, 1));

        let mut input = "/2";
        assert_eq!(parse_duration(&mut input).unwrap(), Duration::new(1, 2));

        let mut input = "/";
        assert_eq!(parse_duration(&mut input).unwrap(), Duration::new(1, 2));

        let mut input = "3/2";
        assert_eq!(parse_duration(&mut input).unwrap(), Duration::new(3, 2));

        let mut input = "";
        assert_eq!(parse_duration(&mut input).unwrap(), Duration::new(1, 1));
    }

    #[test]
    fn test_parse_note() {
        let mut input = "C";
        let note = parse_note(&mut input).unwrap();
        assert_eq!(note.pitch, NoteName::C);
        assert_eq!(note.octave, 0);
        assert_eq!(note.duration, Duration::unit());

        let mut input = "c'";
        let note = parse_note(&mut input).unwrap();
        assert_eq!(note.octave, 2);

        let mut input = "^C2";
        let note = parse_note(&mut input).unwrap();
        assert_eq!(note.accidental, Some(Accidental::Sharp));
        assert_eq!(note.duration, Duration::new(2, 1));

        let mut input = "C-";
        let note = parse_note(&mut input).unwrap();
        assert!(note.tie);
    }

    #[test]
    fn test_parse_rest() {
        let mut input = "z";
        let rest = parse_rest(&mut input).unwrap();
        assert!(rest.visible);
        assert_eq!(rest.duration, Duration::unit());

        let mut input = "z2";
        let rest = parse_rest(&mut input).unwrap();
        assert_eq!(rest.duration, Duration::new(2, 1));

        let mut input = "x";
        let rest = parse_rest(&mut input).unwrap();
        assert!(!rest.visible);

        let mut input = "Z4";
        let rest = parse_rest(&mut input).unwrap();
        assert_eq!(rest.multi_measure, Some(4));
    }

    #[test]
    fn test_parse_chord() {
        let mut input = "[CEG]";
        let chord = parse_chord(&mut input).unwrap();
        assert_eq!(chord.notes.len(), 3);
        assert_eq!(chord.notes[0].pitch, NoteName::C);
        assert_eq!(chord.notes[1].pitch, NoteName::E);
        assert_eq!(chord.notes[2].pitch, NoteName::G);

        let mut input = "[CEG]2";
        let chord = parse_chord(&mut input).unwrap();
        assert_eq!(chord.duration, Duration::new(2, 1));
    }

    #[test]
    fn test_parse_chord_symbol() {
        let mut input = "\"G\"";
        let symbol = parse_chord_symbol(&mut input).unwrap();
        assert_eq!(symbol, "G");

        let mut input = "\"Am7\"";
        let symbol = parse_chord_symbol(&mut input).unwrap();
        assert_eq!(symbol, "Am7");
    }
}
