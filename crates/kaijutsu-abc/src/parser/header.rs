//! Header field parsing for ABC notation.

use crate::ast::{Clef, Header, InfoField, Meter, StemDirection, Tempo, UnitLength, VoiceDef};
use crate::feedback::FeedbackCollector;

use super::key::parse_key_field;

/// Parse the header section of an ABC tune.
///
/// Returns the remaining input after the header (starting at body).
pub fn parse_header<'a>(input: &'a str, collector: &mut FeedbackCollector) -> (&'a str, Header) {
    let mut header = Header::default();
    let mut remaining = input;
    let mut found_x = false;
    let mut found_k = false;
    let mut line_num = 1;

    for line in input.lines() {
        collector.set_position(line_num, 1);

        let trimmed = line.trim();

        // Handle %%MIDI directives before skipping comments
        if let Some(directive) = trimmed.strip_prefix("%%MIDI") {
            parse_midi_directive(directive.trim(), &mut header, collector);
            line_num += 1;
            remaining = &remaining[line.len()..];
            if remaining.starts_with('\n') {
                remaining = &remaining[1..];
            } else if remaining.starts_with("\r\n") {
                remaining = &remaining[2..];
            }
            continue;
        }

        // Skip empty lines and comments in header
        if trimmed.is_empty() || trimmed.starts_with('%') {
            line_num += 1;
            remaining = &remaining[line.len()..];
            if remaining.starts_with('\n') {
                remaining = &remaining[1..];
            } else if remaining.starts_with("\r\n") {
                remaining = &remaining[2..];
            }
            continue;
        }

        // Check for field format
        if trimmed.len() >= 2 && trimmed.chars().nth(1) == Some(':') {
            let field_char = trimmed.chars().next().unwrap();
            let value = trimmed[2..].trim();

            match field_char {
                'X' => {
                    found_x = true;
                    header.reference = value.parse().unwrap_or_else(|_| {
                        collector.warning("Invalid X: value, using 1");
                        1
                    });
                }
                'T' => {
                    if header.title.is_empty() {
                        header.title = value.to_string();
                    } else {
                        header.titles.push(value.to_string());
                    }
                }
                'M' => {
                    header.meter = Some(parse_meter(value, collector));
                }
                'L' => {
                    header.unit_length = Some(parse_unit_length(value, collector));
                }
                'Q' => {
                    header.tempo = Some(parse_tempo(value, collector));
                }
                'K' => {
                    header.key = parse_key_field(value, collector);
                    found_k = true;

                    // Advance past this line
                    remaining = &remaining[line.len()..];
                    if remaining.starts_with('\n') {
                        remaining = &remaining[1..];
                    } else if remaining.starts_with("\r\n") {
                        remaining = &remaining[2..];
                    }
                    break;
                }
                'C' => {
                    header.composer = Some(value.to_string());
                }
                'R' => {
                    header.rhythm = Some(value.to_string());
                }
                'S' => {
                    header.source = Some(value.to_string());
                }
                'N' => {
                    header.notes = Some(value.to_string());
                }
                'V' => {
                    header.voice_defs.push(parse_voice_def(value, collector));
                }
                _ => {
                    header.other_fields.push(InfoField {
                        field_type: field_char,
                        value: value.to_string(),
                    });
                }
            }
        } else {
            // Not a field line - must be body starting
            if !found_k {
                collector.warning_with_suggestion(
                    "Body started before K: field",
                    "Add a K: field before the music (e.g., K:C for C major)",
                );
            }
            break;
        }

        line_num += 1;
        remaining = &remaining[line.len()..];
        if remaining.starts_with('\n') {
            remaining = &remaining[1..];
        } else if remaining.starts_with("\r\n") {
            remaining = &remaining[2..];
        }
    }

    // Emit warnings for missing fields
    if !found_x {
        collector.set_position(1, 1);
        collector.warning_with_suggestion(
            "Missing X: field, assuming X:1",
            "Add X:1 at the start of the tune",
        );
    }

    if !found_k {
        collector.warning_with_suggestion(
            "Missing K: field, assuming K:C",
            "Add a K: field to specify the key signature",
        );
    }

    if header.meter.is_none() {
        collector.warning_with_suggestion(
            "Missing M: field, assuming 4/4",
            "Add M:4/4 or appropriate meter",
        );
        header.meter = Some(Meter::Simple {
            numerator: 4,
            denominator: 4,
        });
    }

    if header.unit_length.is_none() {
        // Infer from meter per ABC standard
        let inferred = infer_unit_length(&header.meter);
        collector.info(format!(
            "No L: field, inferring L:{}/{}",
            inferred.numerator, inferred.denominator
        ));
        header.unit_length = Some(inferred);
    }

    (remaining, header)
}

/// Parse meter field value (e.g., "4/4", "C", "C|", "6/8")
fn parse_meter(value: &str, collector: &mut FeedbackCollector) -> Meter {
    let trimmed = value.trim();

    match trimmed {
        "C" => Meter::Common,
        "C|" => Meter::Cut,
        "none" | "free" => Meter::None,
        _ => {
            // Try to parse as fraction
            if let Some((num, den)) = parse_fraction(trimmed) {
                Meter::Simple {
                    numerator: num,
                    denominator: den,
                }
            } else {
                collector.warning(format!("Invalid meter '{}', assuming 4/4", trimmed));
                Meter::Simple {
                    numerator: 4,
                    denominator: 4,
                }
            }
        }
    }
}

/// Parse unit length field value (e.g., "1/8", "1/16")
fn parse_unit_length(value: &str, collector: &mut FeedbackCollector) -> UnitLength {
    if let Some((num, den)) = parse_fraction(value.trim()) {
        UnitLength {
            numerator: num,
            denominator: den,
        }
    } else {
        collector.warning(format!("Invalid unit length '{}', assuming 1/8", value));
        UnitLength::default()
    }
}

/// Parse tempo field value (e.g., "1/4=120", "120", "\"Allegro\" 1/4=120")
fn parse_tempo(value: &str, collector: &mut FeedbackCollector) -> Tempo {
    let trimmed = value.trim();

    // Check for text in quotes
    let (text, rest) = if let Some(stripped) = trimmed.strip_prefix('"') {
        if let Some(end) = stripped.find('"') {
            let text = stripped[..end].to_string();
            let rest = stripped[end + 1..].trim();
            (Some(text), rest)
        } else {
            (None, trimmed)
        }
    } else {
        (None, trimmed)
    };

    // Parse beat unit and BPM
    if let Some(eq_pos) = rest.find('=') {
        let beat_part = rest[..eq_pos].trim();
        let bpm_part = rest[eq_pos + 1..].trim();

        let beat_unit = if let Some((num, den)) = parse_fraction(beat_part) {
            (num, den)
        } else {
            collector.warning("Invalid tempo beat unit, assuming 1/4");
            (1, 4)
        };

        let bpm = bpm_part.parse().unwrap_or_else(|_| {
            collector.warning("Invalid BPM, assuming 120");
            120
        });

        Tempo {
            beat_unit,
            bpm,
            text,
        }
    } else if let Ok(bpm) = rest.parse::<u16>() {
        // Just a number - assume quarter note
        Tempo {
            beat_unit: (1, 4),
            bpm,
            text,
        }
    } else {
        collector.warning(format!("Invalid tempo '{}', assuming 120 BPM", trimmed));
        Tempo {
            beat_unit: (1, 4),
            bpm: 120,
            text,
        }
    }
}

/// Parse a fraction like "4/4" or "1/8"
fn parse_fraction(s: &str) -> Option<(u8, u8)> {
    let parts: Vec<&str> = s.split('/').collect();
    if parts.len() == 2 {
        let num = parts[0].trim().parse().ok()?;
        let den = parts[1].trim().parse().ok()?;
        Some((num, den))
    } else {
        None
    }
}

/// Infer unit length from meter per ABC standard
fn infer_unit_length(meter: &Option<Meter>) -> UnitLength {
    match meter {
        Some(Meter::Simple {
            numerator,
            denominator,
        }) => {
            let ratio = *numerator as f32 / *denominator as f32;
            if ratio < 0.75 {
                UnitLength {
                    numerator: 1,
                    denominator: 16,
                }
            } else {
                UnitLength {
                    numerator: 1,
                    denominator: 8,
                }
            }
        }
        Some(Meter::Cut) => UnitLength {
            numerator: 1,
            denominator: 8,
        },
        _ => UnitLength::default(),
    }
}

/// Parse a V: voice definition field.
///
/// Format: `V:id [name="..."] [clef=...] [octave=...] [transpose=...] [stem=...]`
///
/// Examples:
/// - `V:1`
/// - `V:Melody name="Lead Melody" clef=treble`
/// - `V:Bass clef=bass octave=-1`
fn parse_voice_def(value: &str, _collector: &mut FeedbackCollector) -> VoiceDef {
    let trimmed = value.trim();

    // Split into tokens - first is the ID, rest are key=value pairs
    let mut parts = trimmed.split_whitespace();

    let id = parts.next().unwrap_or("1").to_string();
    let mut voice = VoiceDef::new(id);

    // Parse remaining key=value pairs
    let remaining: String = parts.collect::<Vec<_>>().join(" ");

    // Parse name="..." (quoted string)
    if let Some(start) = remaining.find("name=\"") {
        let after_name = &remaining[start + 6..];
        if let Some(end) = after_name.find('"') {
            voice.name = Some(after_name[..end].to_string());
        }
    }

    // Parse clef=...
    if let Some(start) = remaining.find("clef=") {
        let after_clef = &remaining[start + 5..];
        let clef_str = after_clef.split_whitespace().next().unwrap_or("");
        voice.clef = Some(parse_clef(clef_str));
    }

    // Parse octave=...
    if let Some(start) = remaining.find("octave=") {
        let after_octave = &remaining[start + 7..];
        let octave_str = after_octave.split_whitespace().next().unwrap_or("0");
        voice.octave = octave_str.parse().ok();
    }

    // Parse transpose=...
    if let Some(start) = remaining.find("transpose=") {
        let after_transpose = &remaining[start + 10..];
        let transpose_str = after_transpose.split_whitespace().next().unwrap_or("0");
        voice.transpose = transpose_str.parse().ok();
    }

    // Parse stem=...
    if let Some(start) = remaining.find("stem=") {
        let after_stem = &remaining[start + 5..];
        let stem_str = after_stem.split_whitespace().next().unwrap_or("");
        voice.stem = Some(parse_stem_direction(stem_str));
    }

    voice
}

/// Parse clef name to Clef enum
fn parse_clef(s: &str) -> Clef {
    match s.to_lowercase().as_str() {
        "treble" | "g" | "g2" => Clef::Treble,
        "bass" | "f" | "f4" => Clef::Bass,
        "alto" | "c" | "c3" => Clef::Alto,
        "tenor" | "c4" => Clef::Tenor,
        "perc" | "percussion" | "drum" => Clef::Percussion,
        _ => Clef::Treble,
    }
}

/// Parse stem direction
fn parse_stem_direction(s: &str) -> StemDirection {
    match s.to_lowercase().as_str() {
        "up" => StemDirection::Up,
        "down" => StemDirection::Down,
        _ => StemDirection::Auto,
    }
}

/// Parse %%MIDI directives
///
/// Currently supports:
/// - `%%MIDI program N` - Set MIDI program number (0-127)
fn parse_midi_directive(directive: &str, header: &mut Header, collector: &mut FeedbackCollector) {
    let parts: Vec<&str> = directive.split_whitespace().collect();

    if parts.is_empty() {
        return;
    }

    match parts[0].to_lowercase().as_str() {
        "program" => {
            if parts.len() >= 2 {
                match parts[1].parse::<u8>() {
                    Ok(program) if program <= 127 => {
                        header.midi_program = Some(program);
                    }
                    Ok(program) => {
                        collector.warning(format!(
                            "MIDI program {} out of range (0-127), ignoring",
                            program
                        ));
                    }
                    Err(_) => {
                        collector.warning(format!(
                            "Invalid MIDI program '{}', expected number 0-127",
                            parts[1]
                        ));
                    }
                }
            } else {
                collector.warning("%%MIDI program requires a number (0-127)");
            }
        }
        other => {
            // Silently ignore other MIDI directives for now
            collector.info(format!("Ignoring %%MIDI {} directive", other));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_meter_common() {
        let mut collector = FeedbackCollector::new();
        assert_eq!(parse_meter("C", &mut collector), Meter::Common);
        assert!(!collector.has_errors());
    }

    #[test]
    fn test_parse_meter_cut() {
        let mut collector = FeedbackCollector::new();
        assert_eq!(parse_meter("C|", &mut collector), Meter::Cut);
    }

    #[test]
    fn test_parse_meter_fraction() {
        let mut collector = FeedbackCollector::new();
        assert_eq!(
            parse_meter("6/8", &mut collector),
            Meter::Simple {
                numerator: 6,
                denominator: 8
            }
        );
    }

    #[test]
    fn test_parse_tempo_full() {
        let mut collector = FeedbackCollector::new();
        let tempo = parse_tempo("1/4=120", &mut collector);
        assert_eq!(tempo.bpm, 120);
        assert_eq!(tempo.beat_unit, (1, 4));
    }

    #[test]
    fn test_parse_tempo_with_text() {
        let mut collector = FeedbackCollector::new();
        let tempo = parse_tempo("\"Allegro\" 1/4=144", &mut collector);
        assert_eq!(tempo.bpm, 144);
        assert_eq!(tempo.text, Some("Allegro".to_string()));
    }

    #[test]
    fn test_parse_tempo_just_bpm() {
        let mut collector = FeedbackCollector::new();
        let tempo = parse_tempo("100", &mut collector);
        assert_eq!(tempo.bpm, 100);
        assert_eq!(tempo.beat_unit, (1, 4)); // Default quarter note
    }

    #[test]
    fn test_infer_unit_length() {
        // 6/8 = 0.75, NOT < 0.75, so should be 1/8
        let meter_6_8 = Some(Meter::Simple {
            numerator: 6,
            denominator: 8,
        });
        assert_eq!(
            infer_unit_length(&meter_6_8),
            UnitLength {
                numerator: 1,
                denominator: 8
            }
        );

        // 4/4 = 1.0 >= 0.75, should be 1/8
        let meter_4_4 = Some(Meter::Simple {
            numerator: 4,
            denominator: 4,
        });
        assert_eq!(
            infer_unit_length(&meter_4_4),
            UnitLength {
                numerator: 1,
                denominator: 8
            }
        );

        // 2/4 = 0.5 < 0.75, should be 1/16
        let meter_2_4 = Some(Meter::Simple {
            numerator: 2,
            denominator: 4,
        });
        assert_eq!(
            infer_unit_length(&meter_2_4),
            UnitLength {
                numerator: 1,
                denominator: 16
            }
        );
    }

    #[test]
    fn test_parse_midi_program() {
        let mut collector = FeedbackCollector::new();
        let abc = "X:1\nT:Test\n%%MIDI program 33\nM:4/4\nK:C\n";
        let (_, header) = parse_header(abc, &mut collector);
        assert_eq!(header.midi_program, Some(33));
    }

    #[test]
    fn test_parse_midi_program_trumpet() {
        let mut collector = FeedbackCollector::new();
        let abc = "X:1\nT:Fanfare\n%%MIDI program 56\nM:4/4\nK:C\n";
        let (_, header) = parse_header(abc, &mut collector);
        assert_eq!(header.midi_program, Some(56)); // Trumpet
    }
}
