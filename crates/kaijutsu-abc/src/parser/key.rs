//! Key signature parsing for ABC notation.

use crate::ast::{Accidental, Clef, Key, Mode, NoteName};
use crate::feedback::FeedbackCollector;

/// Parse a K: field value (e.g., "G", "Am", "D dorian", "F#m", "Bb")
pub fn parse_key_field(value: &str, collector: &mut FeedbackCollector) -> Key {
    let trimmed = value.trim();

    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") {
        return Key::default();
    }

    // Handle special keys
    if trimmed.eq_ignore_ascii_case("Hp") {
        return parse_highland_bagpipe_key();
    }

    let mut chars = trimmed.chars().peekable();

    // Parse root note (A-G)
    let root = match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => match c.to_ascii_uppercase() {
            'C' => NoteName::C,
            'D' => NoteName::D,
            'E' => NoteName::E,
            'F' => NoteName::F,
            'G' => NoteName::G,
            'A' => NoteName::A,
            'B' => NoteName::B,
            _ => {
                collector.warning(format!("Invalid key root '{}', assuming C", c));
                NoteName::C
            }
        },
        _ => {
            collector.warning("Empty or invalid key, assuming C");
            return Key::default();
        }
    };

    // Parse optional accidental (#, b)
    let accidental = if chars.peek() == Some(&'#') {
        chars.next();
        Some(Accidental::Sharp)
    } else if chars.peek() == Some(&'b') {
        // 'b' is flat only if not followed by a letter (which would be mode like "bm")
        // Check what comes after the 'b'
        let mut lookahead = chars.clone();
        lookahead.next(); // skip the 'b'
        let next_char = lookahead.next();
        if !matches!(next_char, Some('a'..='z' | 'A'..='Z')) {
            chars.next();
            Some(Accidental::Flat)
        } else {
            None
        }
    } else {
        None
    };

    // Collect remaining for mode/accidentals/clef parsing
    let remaining: String = chars.collect();
    let remaining = remaining.trim();

    // Parse mode - need to find where mode ends and accidentals might begin
    let (mode, mode_end_pos) = if remaining.is_empty() {
        (Mode::Major, 0)
    } else {
        // Try to parse the first token as a mode
        let first_token = remaining.split_whitespace().next().unwrap_or("");

        // Check if this token is a valid mode
        if let Some(parsed_mode) = Mode::parse(first_token) {
            // Mode found - calculate where it ends in the original string
            let pos = remaining.find(first_token).unwrap_or(0) + first_token.len();
            (parsed_mode, pos)
        } else {
            // No mode found - could be accidentals or other content
            (Mode::Major, 0)
        }
    };

    // Parse optional clef (clef=bass, etc.) - for future use
    let clef = if remaining.contains("clef=") {
        parse_clef_from_key(remaining)
    } else {
        None
    };

    // Parse explicit accidentals (exp ^f _b =c) - start from after the mode
    let accidentals_str = &remaining[mode_end_pos..];
    let explicit_accidentals = parse_explicit_accidentals(accidentals_str, collector);

    Key {
        root,
        accidental,
        mode,
        explicit_accidentals,
        clef,
    }
}

/// Parse Highland bagpipe key (K:Hp)
/// The Highland bagpipe scale has the following notes:
/// G A B ^c d e ^f g a
fn parse_highland_bagpipe_key() -> Key {
    Key {
        root: NoteName::D,
        accidental: None,
        mode: Mode::Mixolydian,
        explicit_accidentals: vec![
            (Accidental::Sharp, NoteName::C),
            (Accidental::Sharp, NoteName::F),
        ],
        clef: None,
    }
}

/// Parse explicit accidentals from key field (e.g., "exp ^f _b =c")
fn parse_explicit_accidentals(
    remaining: &str,
    collector: &mut FeedbackCollector,
) -> Vec<(Accidental, NoteName)> {
    let mut result = Vec::new();

    // Look for "exp" keyword or just start parsing accidentals
    let accidental_start = if let Some(pos) = remaining.find("exp") {
        pos + 3
    } else {
        // Check if there are any accidental characters in the remaining string
        if !remaining.chars().any(|c| matches!(c, '^' | '_' | '=')) {
            return result;
        }
        0
    };

    let accidental_str = &remaining[accidental_start..];
    let mut chars = accidental_str.chars().peekable();

    while chars.peek().is_some() {
        // Skip whitespace
        while chars.peek() == Some(&' ') || chars.peek() == Some(&'\t') {
            chars.next();
        }

        // Check if we've reached clef or other markers
        if chars.peek() == Some(&'c') {
            let lookahead: String = chars.clone().take(5).collect();
            if lookahead.starts_with("clef=") {
                break;
            }
        }

        // Parse accidental symbol
        let accidental = match chars.peek() {
            Some('^') => {
                chars.next();
                if chars.peek() == Some(&'^') {
                    chars.next();
                    Accidental::DoubleSharp
                } else {
                    Accidental::Sharp
                }
            }
            Some('_') => {
                chars.next();
                if chars.peek() == Some(&'_') {
                    chars.next();
                    Accidental::DoubleFlat
                } else {
                    Accidental::Flat
                }
            }
            Some('=') => {
                chars.next();
                Accidental::Natural
            }
            Some(c) if c.is_ascii_alphabetic() => {
                // If we hit a letter without an accidental, might be mode or other field
                break;
            }
            Some(_) => {
                chars.next();
                continue;
            }
            None => break,
        };

        // Skip whitespace before note
        while chars.peek() == Some(&' ') || chars.peek() == Some(&'\t') {
            chars.next();
        }

        // Parse note name
        if let Some(note_char) = chars.next() {
            if let Some(note) = NoteName::parse(&note_char.to_string()) {
                result.push((accidental, note));
            } else {
                collector.warning(format!(
                    "Invalid note '{}' in explicit accidentals",
                    note_char
                ));
            }
        } else {
            collector.warning("Accidental without note in key signature".to_string());
        }
    }

    result
}

/// Parse clef specification from key field
fn parse_clef_from_key(s: &str) -> Option<Clef> {
    if let Some(pos) = s.find("clef=") {
        let after = &s[pos + 5..];
        let clef_name: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphabetic() || *c == '-')
            .collect();

        match clef_name.to_lowercase().as_str() {
            "treble" | "treble-8" | "treble+8" => Some(Clef::Treble),
            "bass" | "bass-8" | "bass+8" => Some(Clef::Bass),
            "alto" => Some(Clef::Alto),
            "tenor" => Some(Clef::Tenor),
            _ => None,
        }
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_key() {
        let mut collector = FeedbackCollector::new();
        let key = parse_key_field("G", &mut collector);

        assert_eq!(key.root, NoteName::G);
        assert_eq!(key.accidental, None);
        assert_eq!(key.mode, Mode::Major);
        assert!(!collector.has_errors());
    }

    #[test]
    fn test_parse_minor_key() {
        let mut collector = FeedbackCollector::new();
        let key = parse_key_field("Am", &mut collector);

        assert_eq!(key.root, NoteName::A);
        assert_eq!(key.accidental, None);
        assert_eq!(key.mode, Mode::Minor);
    }

    #[test]
    fn test_parse_sharp_key() {
        let mut collector = FeedbackCollector::new();
        let key = parse_key_field("F#m", &mut collector);

        assert_eq!(key.root, NoteName::F);
        assert_eq!(key.accidental, Some(Accidental::Sharp));
        assert_eq!(key.mode, Mode::Minor);
    }

    #[test]
    fn test_parse_flat_key() {
        let mut collector = FeedbackCollector::new();
        let key = parse_key_field("Bb", &mut collector);

        assert_eq!(key.root, NoteName::B);
        assert_eq!(key.accidental, Some(Accidental::Flat));
        assert_eq!(key.mode, Mode::Major);
    }

    #[test]
    fn test_parse_modal_key() {
        let mut collector = FeedbackCollector::new();
        let key = parse_key_field("D dorian", &mut collector);

        assert_eq!(key.root, NoteName::D);
        assert_eq!(key.mode, Mode::Dorian);
    }

    #[test]
    fn test_parse_modal_abbreviated() {
        let mut collector = FeedbackCollector::new();
        let key = parse_key_field("E mix", &mut collector);

        assert_eq!(key.root, NoteName::E);
        assert_eq!(key.mode, Mode::Mixolydian);
    }

    #[test]
    fn test_parse_key_with_clef() {
        let mut collector = FeedbackCollector::new();
        let key = parse_key_field("G clef=bass", &mut collector);

        assert_eq!(key.root, NoteName::G);
        assert_eq!(key.clef, Some(Clef::Bass));
    }

    #[test]
    fn test_parse_empty_key() {
        let mut collector = FeedbackCollector::new();
        let key = parse_key_field("", &mut collector);

        assert_eq!(key.root, NoteName::C);
        assert_eq!(key.mode, Mode::Major);
    }

    #[test]
    fn test_parse_none_key() {
        let mut collector = FeedbackCollector::new();
        let key = parse_key_field("none", &mut collector);

        assert_eq!(key.root, NoteName::C);
    }

    #[test]
    fn test_lowercase_key() {
        let mut collector = FeedbackCollector::new();
        let key = parse_key_field("g", &mut collector);

        assert_eq!(key.root, NoteName::G);
        assert_eq!(key.mode, Mode::Major);
    }

    #[test]
    fn test_highland_bagpipe_key() {
        let mut collector = FeedbackCollector::new();
        let key = parse_key_field("Hp", &mut collector);

        assert_eq!(key.root, NoteName::D);
        assert_eq!(key.mode, Mode::Mixolydian);
        assert_eq!(key.explicit_accidentals.len(), 2);
        assert!(key
            .explicit_accidentals
            .contains(&(Accidental::Sharp, NoteName::C)));
        assert!(key
            .explicit_accidentals
            .contains(&(Accidental::Sharp, NoteName::F)));
        assert!(!collector.has_errors());
    }

    #[test]
    fn test_explicit_accidentals_with_exp() {
        let mut collector = FeedbackCollector::new();
        let key = parse_key_field("C exp ^f =c", &mut collector);

        assert_eq!(key.root, NoteName::C);
        assert_eq!(key.mode, Mode::Major);
        assert_eq!(key.explicit_accidentals.len(), 2);
        assert_eq!(
            key.explicit_accidentals[0],
            (Accidental::Sharp, NoteName::F)
        );
        assert_eq!(
            key.explicit_accidentals[1],
            (Accidental::Natural, NoteName::C)
        );
        assert!(!collector.has_errors());
    }

    #[test]
    fn test_explicit_accidentals_without_exp() {
        let mut collector = FeedbackCollector::new();
        let key = parse_key_field("Am ^g", &mut collector);

        assert_eq!(key.root, NoteName::A);
        assert_eq!(key.mode, Mode::Minor);
        assert_eq!(key.explicit_accidentals.len(), 1);
        assert_eq!(
            key.explicit_accidentals[0],
            (Accidental::Sharp, NoteName::G)
        );
        assert!(!collector.has_errors());
    }

    #[test]
    fn test_explicit_accidentals_multiple() {
        let mut collector = FeedbackCollector::new();
        let key = parse_key_field("D exp ^f ^c _b", &mut collector);

        assert_eq!(key.root, NoteName::D);
        assert_eq!(key.mode, Mode::Major);
        assert_eq!(key.explicit_accidentals.len(), 3);
        assert_eq!(
            key.explicit_accidentals[0],
            (Accidental::Sharp, NoteName::F)
        );
        assert_eq!(
            key.explicit_accidentals[1],
            (Accidental::Sharp, NoteName::C)
        );
        assert_eq!(key.explicit_accidentals[2], (Accidental::Flat, NoteName::B));
    }

    #[test]
    fn test_double_sharp_and_flat() {
        let mut collector = FeedbackCollector::new();
        let key = parse_key_field("C exp ^^f __b", &mut collector);

        assert_eq!(key.root, NoteName::C);
        assert_eq!(key.explicit_accidentals.len(), 2);
        assert_eq!(
            key.explicit_accidentals[0],
            (Accidental::DoubleSharp, NoteName::F)
        );
        assert_eq!(
            key.explicit_accidentals[1],
            (Accidental::DoubleFlat, NoteName::B)
        );
    }

    #[test]
    fn test_explicit_accidentals_with_clef() {
        let mut collector = FeedbackCollector::new();
        let key = parse_key_field("G exp ^f clef=bass", &mut collector);

        assert_eq!(key.root, NoteName::G);
        assert_eq!(key.mode, Mode::Major);
        assert_eq!(key.clef, Some(Clef::Bass));
        assert_eq!(key.explicit_accidentals.len(), 1);
        assert_eq!(
            key.explicit_accidentals[0],
            (Accidental::Sharp, NoteName::F)
        );
    }

    #[test]
    fn test_mode_does_not_interfere_with_accidentals() {
        let mut collector = FeedbackCollector::new();
        let key = parse_key_field("D dorian ^f", &mut collector);

        assert_eq!(key.root, NoteName::D);
        assert_eq!(key.mode, Mode::Dorian);
        assert_eq!(key.explicit_accidentals.len(), 1);
        assert_eq!(
            key.explicit_accidentals[0],
            (Accidental::Sharp, NoteName::F)
        );
    }

    #[test]
    fn test_no_explicit_accidentals() {
        let mut collector = FeedbackCollector::new();
        let key = parse_key_field("G major", &mut collector);

        assert_eq!(key.root, NoteName::G);
        assert_eq!(key.mode, Mode::Major);
        assert_eq!(key.explicit_accidentals.len(), 0);
    }
}
