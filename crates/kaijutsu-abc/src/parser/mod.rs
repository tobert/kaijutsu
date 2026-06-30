//! ABC notation parser using winnow.
//!
//! The parser is designed to be generous - it will attempt to continue
//! parsing even when encountering issues, collecting feedback for the user.

mod body;
mod header;
pub(crate) mod key;
mod note;

use crate::ast::{Element, Header, InfoField, LinebreakMode, Tune, Voice};
use crate::feedback::{FeedbackCollector, ParseResult};
use crate::ParseMode;
use std::collections::HashMap;

/// Parse a `U:` assignment value like `T = !trill!` into its letter
/// and expansion. Per spec §4.16 the LHS is a single letter (any case).
/// Returns None if the value isn't a well-formed assignment.
pub(super) fn parse_u_assignment(value: &str) -> Option<(char, String)> {
    let eq = value.find('=')?;
    let lhs = value[..eq].trim();
    let rhs = value[eq + 1..].trim().to_string();
    let mut chars = lhs.chars();
    let key = chars.next()?;
    if chars.next().is_some() || !key.is_ascii_alphabetic() {
        return None;
    }
    Some((key, rhs))
}

/// Build the initial U: redefinable-symbol map for a tune. Reads
/// `U:` info fields from both the file header and the tune header,
/// with tune-header assignments winning on collision (per §2.2).
pub(super) fn build_user_symbols(
    file_header: &Header,
    tune_header: &Header,
) -> HashMap<char, String> {
    let mut map = HashMap::new();
    for f in file_header.other_fields.iter().chain(tune_header.other_fields.iter()) {
        if f.field_type == 'U' {
            if let Some((k, v)) = parse_u_assignment(&f.value) {
                map.insert(k, v);
            }
        }
    }
    map
}

/// Derive [`LinebreakMode`] from `I:linebreak …` entries in `header`.
/// Searches both `other_fields` (where header `I:` fields live today).
pub(super) fn linebreak_mode_from_header(header: &Header) -> LinebreakMode {
    for f in &header.other_fields {
        if f.field_type != 'I' {
            continue;
        }
        if let Some(rest) = f.value.trim().strip_prefix("linebreak") {
            let marker = rest.trim();
            return match marker {
                "$" => LinebreakMode::Dollar,
                "!" => LinebreakMode::Bang,
                "<none>" | "none" => LinebreakMode::None,
                _ => LinebreakMode::Eol,
            };
        }
    }
    LinebreakMode::Eol
}

/// Strip an inline `%` comment tail from a line per spec §3.1.
///
/// `%` introduces an end-of-line comment anywhere on a line, including
/// inside field values, so `M:3/4    % meter` must read `M:3/4` not
/// `3/4 % meter`. `\%` is an escape sequence for a literal `%` and is
/// preserved. Trailing whitespace from the stripped tail is removed.
pub(super) fn strip_inline_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && (i == 0 || bytes[i - 1] != b'\\') {
            return line[..i].trim_end();
        }
        i += 1;
    }
    line.trim_end()
}

/// Parse ABC notation into a list of Tune ASTs.
///
/// A `.abc` source may contain a file-level header (anything before the
/// first `X:`) followed by one or more tunes delimited by `X:N` lines.
/// Per spec §2.2, info fields in the file header apply as defaults to
/// every tune in the file; fields set on a tune itself win on conflict.
pub fn parse(input: &str, mode: ParseMode) -> ParseResult<Vec<Tune>> {
    let mut collector = FeedbackCollector::new();

    let (file_header_text, tune_segments) = split_file_and_tunes(input);

    // Parse the pre-X: file header as a bag of info fields. No defaults
    // are filled here — we only want to capture explicit fields.
    let file_header = parse_file_header_fields(file_header_text);

    let tunes: Vec<Tune> = if tune_segments.is_empty() {
        // No X: in input — fall back to single-fragment parsing on the
        // whole input.
        if input.trim().is_empty() {
            Vec::new()
        } else {
            vec![parse_one_tune(input, &mut collector, mode, &file_header)]
        }
    } else {
        tune_segments
            .into_iter()
            .map(|segment| {
                let mut tune = parse_one_tune(segment, &mut collector, mode, &file_header);
                inherit_from_file_header(&mut tune.header, &file_header);
                tune
            })
            .collect()
    };

    ParseResult::new(tunes, collector.into_feedback())
}

fn parse_one_tune(
    input: &str,
    collector: &mut FeedbackCollector,
    mode: ParseMode,
    file_header: &Header,
) -> Tune {
    let (remaining, mut header) = header::parse_header(input, collector, mode);
    let linebreak = linebreak_mode_from_header(&header);
    let mut user_symbols = build_user_symbols(file_header, &header);
    let (elements, body_voice_defs) =
        body::parse_body_with_voices(remaining, collector, mode, linebreak, &mut user_symbols);
    // Voices declared inline in the body (`V:T clef=bass …`) are merged
    // into the header's voice defs so the layout resolves their clef. A
    // header def wins on conflict, but fills any field it left unset.
    merge_body_voice_defs(&mut header.voice_defs, body_voice_defs);
    let voices = route_elements_to_voices(&header.voice_defs, elements);
    Tune { header, voices }
}

/// Split the input into (pre-X file header text, list of tune segments).
/// Each tune segment starts at an `X:` line. If the input has no `X:`,
/// returns ("", []) — caller falls back to single-fragment parsing.
fn split_file_and_tunes(input: &str) -> (&str, Vec<&str>) {
    let mut x_positions: Vec<usize> = Vec::new();
    if input.starts_with("X:") {
        x_positions.push(0);
    }
    let mut cursor = if input.starts_with("X:") { 2 } else { 0 };
    while let Some(rel) = input[cursor..].find("\nX:") {
        let x_pos = cursor + rel + 1;
        x_positions.push(x_pos);
        cursor = x_pos + 2;
    }
    if x_positions.is_empty() {
        return ("", Vec::new());
    }
    let file_header = &input[..x_positions[0]];
    let tunes: Vec<&str> = x_positions
        .iter()
        .enumerate()
        .map(|(i, &start)| {
            let end = x_positions.get(i + 1).copied().unwrap_or(input.len());
            &input[start..end]
        })
        .collect();
    (file_header, tunes)
}

/// Minimal parse over pre-X: content: collect each `<letter>:` line as
/// raw InfoField. Doesn't fill defaults, doesn't emit warnings — just
/// captures what was explicitly written at file level.
fn parse_file_header_fields(input: &str) -> Header {
    let mut header = Header::default();
    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('%') {
            continue;
        }
        let trimmed = strip_inline_comment(trimmed);
        if trimmed.len() < 2 || trimmed.chars().nth(1) != Some(':') {
            continue;
        }
        let field_char = match trimmed.chars().next() {
            Some(c) if c.is_ascii_alphabetic() => c,
            _ => continue,
        };
        let value = trimmed[2..].trim().to_string();
        match field_char {
            'T' => {
                if header.title.is_empty() {
                    header.title = value;
                } else {
                    header.titles.push(value);
                }
            }
            'C' => header.composer = Some(value),
            'R' => header.rhythm = Some(value),
            'S' => header.source = Some(value),
            'N' => header.notes = Some(value),
            _ => header.other_fields.push(InfoField {
                field_type: field_char,
                value,
            }),
        }
    }
    header
}

/// Per spec §2.2: copy fields set explicitly in the file header into a
/// tune's header where the tune doesn't set them. Tune-level wins on
/// conflict. We can only inherit fields where None signals "unset" —
/// M:/L:/Q: are filled with defaults by parse_header so we can't tell
/// "missing in tune" from "explicit in tune"; those are not inherited
/// today.
fn inherit_from_file_header(tune: &mut Header, file: &Header) {
    if tune.composer.is_none() {
        tune.composer = file.composer.clone();
    }
    if tune.rhythm.is_none() {
        tune.rhythm = file.rhythm.clone();
    }
    if tune.source.is_none() {
        tune.source = file.source.clone();
    }
    if tune.notes.is_none() {
        tune.notes = file.notes.clone();
    }
    // Prepend file-level other_fields so the tune's appear later
    // (tune wins if a downstream consumer reads in order).
    if !file.other_fields.is_empty() {
        let mut combined: Vec<InfoField> = file.other_fields.clone();
        combined.append(&mut tune.other_fields);
        tune.other_fields = combined;
    }
}

/// Merge voice definitions discovered inline in the body into the header's
/// list. A header definition takes precedence but inherits any field the
/// body set that the header left unset (e.g. a header `V:T` with no clef
/// picks up `clef=bass` from the body switch). Body-only voices are
/// appended in encounter order so they render after header-declared ones.
fn merge_body_voice_defs(
    header_defs: &mut Vec<crate::ast::VoiceDef>,
    body_defs: Vec<crate::ast::VoiceDef>,
) {
    for bd in body_defs {
        if let Some(existing) = header_defs.iter_mut().find(|d| d.id == bd.id) {
            existing.name = existing.name.take().or(bd.name);
            existing.clef = existing.clef.or(bd.clef);
            existing.octave = existing.octave.or(bd.octave);
            existing.transpose = existing.transpose.or(bd.transpose);
            existing.stem = existing.stem.or(bd.stem);
        } else {
            header_defs.push(bd);
        }
    }
}

/// Route parsed elements to their respective voices based on VoiceSwitch markers.
///
/// Handles three cases:
/// 1. No voice definitions and no voice switches: single default voice
/// 2. Voice definitions in header: use those as the voice list
/// 3. Voice switches without header definitions: create voices from switches
fn route_elements_to_voices(
    voice_defs: &[crate::ast::VoiceDef],
    elements: Vec<Element>,
) -> Vec<Voice> {
    // Check if there are any VoiceSwitch elements
    let has_voice_switches = elements
        .iter()
        .any(|e| matches!(e, Element::VoiceSwitch(_)));

    // If no voice definitions AND no voice switches, put everything in a single default voice
    if voice_defs.is_empty() && !has_voice_switches {
        return vec![Voice {
            id: None,
            name: None,
            elements,
        }];
    }

    // Create a voice for each definition (if any)
    let mut voice_map: HashMap<String, Vec<Element>> = HashMap::new();
    for def in voice_defs {
        voice_map.insert(def.id.clone(), Vec::new());
    }

    // Start with the first defined voice, or "1" if using dynamic voice switches
    let mut current_voice_id = voice_defs
        .first()
        .map(|d| d.id.clone())
        .unwrap_or_else(|| "1".to_string());

    // Ensure we have a default voice for elements before the first VoiceSwitch
    if !voice_map.contains_key(&current_voice_id) {
        voice_map.insert(current_voice_id.clone(), Vec::new());
    }

    for element in elements {
        match element {
            Element::VoiceSwitch(ref voice_id) => {
                // Switch to the specified voice (create if not exists)
                current_voice_id = voice_id.clone();
                if !voice_map.contains_key(&current_voice_id) {
                    voice_map.insert(current_voice_id.clone(), Vec::new());
                }
            }
            _ => {
                // Add element to current voice
                if let Some(voice_elements) = voice_map.get_mut(&current_voice_id) {
                    voice_elements.push(element);
                }
            }
        }
    }

    // Build voices in order of definitions first
    let mut voices = Vec::new();
    for def in voice_defs {
        let elements = voice_map.remove(&def.id).unwrap_or_default();
        voices.push(Voice {
            id: Some(def.id.clone()),
            name: def.name.clone(),
            elements,
        });
    }

    // Add any voices that were created by VoiceSwitch but not in defs
    // Sort by ID for deterministic output
    let mut extra_voices: Vec<_> = voice_map.into_iter().collect();
    extra_voices.sort_by(|a, b| a.0.cmp(&b.0));

    for (id, elements) in extra_voices {
        if !elements.is_empty() {
            voices.push(Voice {
                id: Some(id),
                name: None,
                elements,
            });
        }
    }

    voices
}

#[cfg(test)]
mod tests {
    use crate::ast::*;

    #[test]
    fn strip_inline_comment_basic() {
        assert_eq!(super::strip_inline_comment("3/4"), "3/4");
        assert_eq!(super::strip_inline_comment("3/4 % meter"), "3/4");
        assert_eq!(super::strip_inline_comment("3/4% meter"), "3/4");
        assert_eq!(super::strip_inline_comment("% only comment"), "");
        assert_eq!(super::strip_inline_comment("a\\%b % real"), "a\\%b");
    }

    #[test]
    fn header_strips_inline_comments_in_field_values() {
        // Spec §3.1: `%` starts a comment even inside field values.
        let abc = "X:1                   % tune no 1\nT:Test         % the title\nM:3/4                 % meter\nL:1/8 % unit\nK:C   % key\nCDE|\n";
        let result = crate::parse(abc);

        // No warnings about invalid values for X:/M:/L:/K:
        for fb in &result.feedback {
            assert!(
                !fb.message.contains("Invalid"),
                "unexpected complaint about field value: {:?}",
                fb,
            );
        }
        assert_eq!(result.value[0].header.reference, 1);
        assert_eq!(result.value[0].header.title, "Test");
        assert_eq!(
            result.value[0].header.meter,
            Some(Meter::Simple {
                numerator: 3,
                denominator: 4,
            })
        );
        assert_eq!(
            result.value[0].header.unit_length,
            Some(UnitLength {
                numerator: 1,
                denominator: 8,
            })
        );
        assert_eq!(result.value[0].header.key.root, NoteName::C);
    }

    #[test]
    fn parse_u_assignment_basic() {
        assert_eq!(
            super::parse_u_assignment("T = !trill!"),
            Some(('T', "!trill!".to_string())),
        );
        assert_eq!(
            super::parse_u_assignment("J = ~"),
            Some(('J', "~".to_string())),
        );
        assert_eq!(super::parse_u_assignment("not an assignment"), None);
        assert_eq!(super::parse_u_assignment("AB = !accent!"), None); // LHS not single char
    }

    #[test]
    fn u_field_remaps_letter_in_body() {
        // U:J = !trill! makes `J` produce a trill instead of hitting
        // the unknown-char fallback.
        let abc = "X:1\nT:Test\nU:J = !trill!\nK:C\nJCDE|\n";
        let result = crate::parse(abc);
        assert!(!result.has_errors(), "feedback: {:?}", result.feedback);
        let has_trill = result.value[0].voices[0].elements.iter().any(|e| {
            matches!(e, Element::Decoration(Decoration::Trill))
        });
        assert!(
            has_trill,
            "expected U:J to expand to a trill, got: {:?}",
            result.value[0].voices[0].elements,
        );
    }

    #[test]
    fn u_field_overrides_default_decoration() {
        // T defaults to Trill; U:T = !lowermordent! overrides.
        let abc = "X:1\nT:Test\nU:T = !lowermordent!\nK:C\nTCDE|\n";
        let result = crate::parse(abc);
        assert!(!result.has_errors(), "feedback: {:?}", result.feedback);
        let decorations: Vec<_> = result.value[0].voices[0]
            .elements
            .iter()
            .filter_map(|e| match e {
                Element::Decoration(d) => Some(d),
                _ => None,
            })
            .collect();
        // Should contain a lower mordent, NOT a trill.
        assert!(
            decorations
                .iter()
                .any(|d| matches!(d, Decoration::Mordent { upper: false })),
            "expected lower mordent from U:T override, got: {:?}",
            decorations,
        );
        assert!(
            !decorations.iter().any(|d| matches!(d, Decoration::Trill)),
            "U: override should suppress default Trill",
        );
    }

    #[test]
    fn inline_u_rebinds_mid_body() {
        // Inline U: changes the binding for the rest of the body.
        let abc = "X:1\nT:Test\nU:J = !trill!\nK:C\nJCD|\nU:J = !accent!\nJEF|\n";
        let result = crate::parse(abc);
        assert!(!result.has_errors(), "feedback: {:?}", result.feedback);
        let decorations: Vec<_> = result.value[0].voices[0]
            .elements
            .iter()
            .filter_map(|e| match e {
                Element::Decoration(d) => Some(d),
                _ => None,
            })
            .collect();
        // First J (before rebind) → Trill; second J → Accent.
        assert!(
            decorations.contains(&&Decoration::Trill),
            "expected trill from first J, got: {:?}",
            decorations,
        );
        assert!(
            decorations.contains(&&Decoration::Accent),
            "expected accent from rebound J, got: {:?}",
            decorations,
        );
    }

    #[test]
    fn u_recursion_depth_capped() {
        // U:A = A — self-referential. Should not stack overflow.
        let abc = "X:1\nT:Test\nU:N = N\nK:C\nNCDE|\n";
        let result = crate::parse(abc);
        // No panic; gets a depth warning.
        let depth_warned = result
            .feedback
            .iter()
            .any(|f| f.message.contains("depth"));
        assert!(
            depth_warned,
            "expected depth warning, got: {:?}",
            result.feedback,
        );
    }

    #[test]
    fn linebreak_dollar_emits_linebreak() {
        // I:linebreak $ in header turns `$` into a score line-break.
        let abc = "X:1\nT:Test\nI:linebreak $\nK:C\nCDE$FGA|\n";
        let result = crate::parse(abc);
        assert!(!result.has_errors(), "feedback: {:?}", result.feedback);
        let break_count = result.value[0].voices[0]
            .elements
            .iter()
            .filter(|e| matches!(e, Element::LineBreak))
            .count();
        // Two breaks: the `$` and the trailing newline.
        assert_eq!(
            break_count, 2,
            "expected 2 LineBreak elements (1 from `$` + 1 from \\n), got {}: {:?}",
            break_count, result.value[0].voices[0].elements,
        );
    }

    #[test]
    fn linebreak_dollar_off_by_default() {
        // Without I:linebreak $, `$` should hit the unknown-char path.
        let abc = "X:1\nT:Test\nK:C\nCDE$FGA|\n";
        let result = crate::parse(abc);
        let warned = result
            .feedback
            .iter()
            .any(|f| f.message.contains("$") || f.message.contains("unknown"));
        assert!(
            warned,
            "expected unknown-char warning for `$` without directive, got: {:?}",
            result.feedback,
        );
    }

    #[test]
    fn linebreak_dollar_inline_in_body_takes_effect() {
        // Inline I:linebreak $ mid-body switches mode for the rest.
        let abc = "X:1\nT:Test\nK:C\nCDE|\nI:linebreak $\nFGA$BCD|\n";
        let result = crate::parse(abc);
        assert!(!result.has_errors(), "feedback: {:?}", result.feedback);
        let has_dollar_break = result.value[0].voices[0]
            .elements
            .iter()
            .any(|e| matches!(e, Element::LineBreak));
        assert!(has_dollar_break, "expected at least one LineBreak");
    }

    #[test]
    fn test_parse_minimal() {
        let abc = "X:1\nT:Test\nK:C\n";
        let result = crate::parse(abc);

        assert!(!result.has_errors());
        assert_eq!(result.value[0].header.reference, 1);
        assert_eq!(result.value[0].header.title, "Test");
        assert_eq!(result.value[0].header.key.root, NoteName::C);
    }

    #[test]
    fn test_parse_with_meter() {
        let abc = "X:1\nT:Test\nM:6/8\nK:G\n";
        let result = crate::parse(abc);

        assert!(!result.has_errors());
        assert_eq!(
            result.value[0].header.meter,
            Some(Meter::Simple {
                numerator: 6,
                denominator: 8
            })
        );
    }

    #[test]
    fn test_parse_common_time() {
        let abc = "X:1\nT:Test\nM:C\nK:D\n";
        let result = crate::parse(abc);

        assert!(!result.has_errors());
        assert_eq!(result.value[0].header.meter, Some(Meter::Common));
    }

    #[test]
    fn test_parse_cut_time() {
        let abc = "X:1\nT:Test\nM:C|\nK:D\n";
        let result = crate::parse(abc);

        assert!(!result.has_errors());
        assert_eq!(result.value[0].header.meter, Some(Meter::Cut));
    }

    #[test]
    fn test_parse_unit_length() {
        let abc = "X:1\nT:Test\nL:1/16\nK:C\n";
        let result = crate::parse(abc);

        assert!(!result.has_errors());
        assert_eq!(
            result.value[0].header.unit_length,
            Some(UnitLength {
                numerator: 1,
                denominator: 16
            })
        );
    }

    #[test]
    fn test_parse_tempo() {
        let abc = "X:1\nT:Test\nQ:1/4=120\nK:C\n";
        let result = crate::parse(abc);

        assert!(!result.has_errors());
        let tempo = result.value[0].header.tempo.as_ref().unwrap();
        assert_eq!(tempo.bpm, 120);
        assert_eq!(tempo.beat_unit, (1, 4));
    }

    #[test]
    fn test_parse_simple_notes() {
        let abc = "X:1\nT:Test\nK:C\nCDEF|";
        let result = crate::parse(abc);

        assert!(!result.has_errors());

        let notes: Vec<_> = result.value[0].voices[0]
            .elements
            .iter()
            .filter_map(|e| match e {
                Element::Note(n) => Some(n),
                _ => None,
            })
            .collect();

        assert_eq!(notes.len(), 4);
        assert_eq!(notes[0].pitch, NoteName::C);
        assert_eq!(notes[0].octave, 0);
        assert_eq!(notes[1].pitch, NoteName::D);
        assert_eq!(notes[2].pitch, NoteName::E);
        assert_eq!(notes[3].pitch, NoteName::F);
    }

    #[test]
    fn test_parse_lowercase_notes() {
        let abc = "X:1\nT:Test\nK:C\ncdef|";
        let result = crate::parse(abc);

        let notes: Vec<_> = result.value[0].voices[0]
            .elements
            .iter()
            .filter_map(|e| match e {
                Element::Note(n) => Some(n),
                _ => None,
            })
            .collect();

        assert_eq!(notes.len(), 4);
        assert_eq!(notes[0].pitch, NoteName::C);
        assert_eq!(notes[0].octave, 1); // Lowercase = octave 1
    }

    #[test]
    fn test_parse_octave_modifiers() {
        let abc = "X:1\nT:Test\nK:C\nC,Cc'|";
        let result = crate::parse(abc);

        let notes: Vec<_> = result.value[0].voices[0]
            .elements
            .iter()
            .filter_map(|e| match e {
                Element::Note(n) => Some(n),
                _ => None,
            })
            .collect();

        assert_eq!(notes.len(), 3);
        assert_eq!(notes[0].octave, -1); // C,
        assert_eq!(notes[1].octave, 0); // C
        assert_eq!(notes[2].octave, 2); // c'
    }

    #[test]
    fn test_parse_accidentals() {
        let abc = "X:1\nT:Test\nK:C\n^C_D=E^^F__|";
        let result = crate::parse(abc);

        let notes: Vec<_> = result.value[0].voices[0]
            .elements
            .iter()
            .filter_map(|e| match e {
                Element::Note(n) => Some(n),
                _ => None,
            })
            .collect();

        // Note: this depends on how we parse ^^F__
        // It could be ^^F followed by __, or it could fail
        // For now, let's check what we get
        assert!(notes.len() >= 4);
        assert_eq!(notes[0].accidental, Some(Accidental::Sharp));
        assert_eq!(notes[1].accidental, Some(Accidental::Flat));
        assert_eq!(notes[2].accidental, Some(Accidental::Natural));
    }

    #[test]
    fn test_parse_durations() {
        let abc = "X:1\nT:Test\nK:C\nA A2 A/2 A3/2|";
        let result = crate::parse(abc);

        let notes: Vec<_> = result.value[0].voices[0]
            .elements
            .iter()
            .filter_map(|e| match e {
                Element::Note(n) => Some(n),
                _ => None,
            })
            .collect();

        assert_eq!(notes.len(), 4);
        assert_eq!(notes[0].duration, Duration::new(1, 1));
        assert_eq!(notes[1].duration, Duration::new(2, 1));
        assert_eq!(notes[2].duration, Duration::new(1, 2));
        assert_eq!(notes[3].duration, Duration::new(3, 2));
    }

    #[test]
    fn test_parse_rest() {
        let abc = "X:1\nT:Test\nK:C\nz z2|";
        let result = crate::parse(abc);

        let rests: Vec<_> = result.value[0].voices[0]
            .elements
            .iter()
            .filter_map(|e| match e {
                Element::Rest(r) => Some(r),
                _ => None,
            })
            .collect();

        assert_eq!(rests.len(), 2);
        assert!(rests[0].visible);
        assert_eq!(rests[1].duration, Duration::new(2, 1));
    }

    #[test]
    fn test_parse_chord() {
        let abc = "X:1\nT:Test\nK:C\n[CEG]2|";
        let result = crate::parse(abc);

        let chords: Vec<_> = result.value[0].voices[0]
            .elements
            .iter()
            .filter_map(|e| match e {
                Element::Chord(c) => Some(c),
                _ => None,
            })
            .collect();

        assert_eq!(chords.len(), 1);
        assert_eq!(chords[0].notes.len(), 3);
        assert_eq!(chords[0].duration, Duration::new(2, 1));
    }

    #[test]
    fn test_parse_bar_types() {
        let abc = "X:1\nT:Test\nK:C\nC|D||E|]";
        let result = crate::parse(abc);

        let bars: Vec<_> = result.value[0].voices[0]
            .elements
            .iter()
            .filter_map(|e| match e {
                Element::Bar(b) => Some(b),
                _ => None,
            })
            .collect();

        assert_eq!(bars.len(), 3);
        assert_eq!(bars[0], &Bar::Single);
        assert_eq!(bars[1], &Bar::Double);
        assert_eq!(bars[2], &Bar::End);
    }

    #[test]
    fn test_parse_repeat_bars() {
        let abc = "X:1\nT:Test\nK:C\n|:C:|D::|";
        let result = crate::parse(abc);

        let bars: Vec<_> = result.value[0].voices[0]
            .elements
            .iter()
            .filter_map(|e| match e {
                Element::Bar(b) => Some(b),
                _ => None,
            })
            .collect();

        assert!(bars.contains(&&Bar::RepeatStart));
        assert!(bars.contains(&&Bar::RepeatEnd));
    }

    #[test]
    fn test_parse_tie() {
        let abc = "X:1\nT:Test\nK:C\nC-C|";
        let result = crate::parse(abc);

        let notes: Vec<_> = result.value[0].voices[0]
            .elements
            .iter()
            .filter_map(|e| match e {
                Element::Note(n) => Some(n),
                _ => None,
            })
            .collect();

        assert_eq!(notes.len(), 2);
        assert!(notes[0].tie);
        assert!(!notes[1].tie);
    }

    #[test]
    fn test_parse_missing_x_field_warns() {
        let abc = "T:Test\nK:C\nCDE|";
        let result = crate::parse(abc);

        // Should warn but not error
        assert!(result.feedback.iter().any(|f| f.message.contains("X:")));
        // Should still parse
        assert_eq!(result.value[0].header.title, "Test");
    }

    #[test]
    fn test_parse_key_modes() {
        let abc = "X:1\nT:Test\nK:D dorian\n";
        let result = crate::parse(abc);

        assert_eq!(result.value[0].header.key.root, NoteName::D);
        assert_eq!(result.value[0].header.key.mode, Mode::Dorian);
    }

    #[test]
    fn test_parse_key_with_accidental() {
        let abc = "X:1\nT:Test\nK:F#m\n";
        let result = crate::parse(abc);

        assert_eq!(result.value[0].header.key.root, NoteName::F);
        assert_eq!(result.value[0].header.key.accidental, Some(Accidental::Sharp));
        assert_eq!(result.value[0].header.key.mode, Mode::Minor);
    }

    #[test]
    fn test_parse_chord_symbol() {
        let abc = "X:1\nT:Test\nK:C\n\"G\"GAB|";
        let result = crate::parse(abc);

        let symbols: Vec<_> = result.value[0].voices[0]
            .elements
            .iter()
            .filter_map(|e| match e {
                Element::ChordSymbol(s) => Some(s),
                _ => None,
            })
            .collect();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0], "G");
    }

    #[test]
    fn test_multivoice_without_header_defs() {
        // This is the bug case from session-review-2026-01-05:
        // V:1 and V:2 in the body WITHOUT voice definitions in header
        let abc = "X:1\nT:Test\nM:4/4\nL:1/4\nK:C\nV:1\nCD|\nV:2\nEF|\n";
        let result = crate::parse(abc);

        assert!(!result.has_errors(), "Parse errors: {:?}", result.feedback);

        // Should have TWO voices, not one merged voice
        assert_eq!(
            result.value[0].voices.len(),
            2,
            "Expected 2 voices, got {}. Voices: {:?}",
            result.value[0].voices.len(),
            result.value[0]
                .voices
                .iter()
                .map(|v| v.id.clone())
                .collect::<Vec<_>>()
        );

        // Voice 1 should have C, D
        let v1_notes: Vec<_> = result.value[0].voices[0]
            .elements
            .iter()
            .filter_map(|e| match e {
                Element::Note(n) => Some(n.pitch),
                _ => None,
            })
            .collect();
        assert_eq!(
            v1_notes,
            vec![NoteName::C, NoteName::D],
            "Voice 1 notes wrong"
        );

        // Voice 2 should have E, F
        let v2_notes: Vec<_> = result.value[0].voices[1]
            .elements
            .iter()
            .filter_map(|e| match e {
                Element::Note(n) => Some(n.pitch),
                _ => None,
            })
            .collect();
        assert_eq!(
            v2_notes,
            vec![NoteName::E, NoteName::F],
            "Voice 2 notes wrong"
        );
    }

    #[test]
    fn test_multivoice_midi_simultaneous() {
        // Verify that multi-voice ABC produces MIDI with simultaneous notes
        let abc = "X:1\nT:Test\nM:4/4\nL:1/4\nK:C\nV:1\nc|\nV:2\nC|\n";
        let result = crate::parse(abc);
        assert!(!result.has_errors());
        assert_eq!(result.value[0].voices.len(), 2);

        // Generate MIDI
        let midi = crate::midi::generate(&result.value[0], &crate::MidiParams::default());

        // Should be format 1 (multi-track) - byte 9 should be 0x01
        assert_eq!(&midi[0..4], b"MThd", "Not valid MIDI header");
        assert_eq!(midi[9], 1, "Should be MIDI format 1 (multi-track)");

        // Should have 3 tracks (tempo + 2 voices) - bytes 10-11
        let track_count = u16::from_be_bytes([midi[10], midi[11]]);
        assert_eq!(track_count, 3, "Expected 3 tracks (tempo + 2 voices)");
    }
}
