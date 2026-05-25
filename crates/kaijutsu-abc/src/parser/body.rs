//! Music body parsing for ABC notation.

use winnow::prelude::*;

use crate::ast::{Bar, Element, InfoField, Tuplet};
use crate::feedback::FeedbackCollector;
use crate::ParseMode;

use super::note::{parse_chord, parse_chord_symbol, parse_note, parse_rest};

/// Characters reserved by ABC v2.1 §8.1 for future extensions. Current
/// software is instructed to ignore them with (at most) a warning.
const RESERVED_CHARS: &[char] = &['#', '*', ';', '?', '@'];

/// Skip whitespace (spaces and tabs) at the start of input, returning count
fn skip_spaces(input: &mut &str) -> usize {
    let start_len = input.len();
    *input = input.trim_start_matches([' ', '\t']);
    start_len - input.len()
}

/// Parse the body section of an ABC tune.
pub fn parse_body(
    input: &str,
    collector: &mut FeedbackCollector,
    mode: ParseMode,
) -> Vec<Element> {
    let mut elements = Vec::new();
    let mut remaining = input;
    let mut line_num = 1;
    // Tracks whether `remaining` is positioned at the start of a body
    // line. Field-style markers like `w:` are only valid at line start,
    // so this flag is what distinguishes the lyric line `w: doh re mi`
    // from a stray `w` mid-music (which would still hit the fallback).
    let mut at_line_start = true;

    while !remaining.is_empty() {
        collector.set_position(line_num, 1);

        // Line-start info fields (w:, W:, s:) and +: continuations must
        // be checked before the generic element parser, otherwise their
        // content gets shredded by the body fallback.
        if at_line_start {
            if remaining.starts_with("+:") {
                let after = &remaining[2..];
                let line_end = after.find('\n').unwrap_or(after.len());
                let content = after[..line_end].trim();
                if !append_continuation(&mut elements, content) {
                    collector.warning(
                        "'+:' continuation has no preceding w:/W:/s: line to extend",
                    );
                }
                remaining = &after[line_end..];
                at_line_start = false;
                continue;
            }
            if let Some(elem) = try_parse_line_start_field(&mut remaining) {
                elements.push(elem);
                at_line_start = false;
                continue;
            }
        }

        // Skip leading whitespace (but not newlines)
        let space_count = skip_spaces(&mut remaining);

        if space_count > 0 {
            elements.push(Element::Space);
            at_line_start = false;
        }

        // Trailing-backslash line continuation per §6.1.1: `\<newline>`
        // joins the next physical line into the current logical line.
        // No LineBreak is emitted; the next line is parsed mid-music
        // (at_line_start stays false).
        if remaining.starts_with("\\\n") {
            remaining = &remaining[2..];
            line_num += 1;
            at_line_start = false;
            continue;
        }
        if remaining.starts_with("\\\r\n") {
            remaining = &remaining[3..];
            line_num += 1;
            at_line_start = false;
            continue;
        }

        // Check for newline
        if remaining.starts_with('\n') {
            remaining = &remaining[1..];
            line_num += 1;
            elements.push(Element::LineBreak);
            at_line_start = true;
            continue;
        }
        if remaining.starts_with("\r\n") {
            remaining = &remaining[2..];
            line_num += 1;
            elements.push(Element::LineBreak);
            at_line_start = true;
            continue;
        }

        // Voice overlay marker (`&` per §7.4). Consume any run of `&`
        // chars and emit a single Overlay element with the count. The
        // AST does not yet route overlay music into separate tracks.
        if remaining.starts_with('&') {
            let layers = remaining.chars().take_while(|c| *c == '&').count();
            let clamped = layers.min(u8::MAX as usize) as u8;
            remaining = &remaining[layers..];
            elements.push(Element::Overlay { layers: clamped });
            at_line_start = false;
            continue;
        }

        // Broken rhythm operators per §4.4. Looks back at the previous
        // note to apply the lengthening half; consumes the operator and
        // the following note for the shortening half. If no previous
        // note or no follow-up note, leaves `remaining` alone so the
        // chars hit the unknown-character fallback below.
        if remaining.starts_with('>') || remaining.starts_with('<') {
            if try_broken_rhythm(&mut remaining, &mut elements) {
                at_line_start = false;
                continue;
            }
        }

        // Check for comment or directive
        if remaining.starts_with('%') {
            // Check for %%MIDI directive in body - warn that it's ignored
            if remaining.starts_with("%%MIDI") {
                collector.warning(
                    "%%MIDI directive found after K: field - move it before K: to take effect",
                );
            }
            // Skip to end of line
            if let Some(newline_pos) = remaining.find('\n') {
                remaining = &remaining[newline_pos..];
            } else {
                break;
            }
            at_line_start = false;
            continue;
        }

        // Try to parse an element
        if let Some(element) = try_parse_element(&mut remaining, collector) {
            elements.push(element);
            at_line_start = false;
        } else if !remaining.is_empty() {
            let c = remaining.chars().next().unwrap();
            if !c.is_whitespace() {
                if RESERVED_CHARS.contains(&c) {
                    // Reserved-for-future-use char per §8.1. Always a
                    // warning regardless of mode — it's spec-legal input
                    // we just don't have a meaning for.
                    collector.warning(format!(
                        "Reserved character '{}' ignored (ABC v2.1 §8.1)",
                        c
                    ));
                } else {
                    // The parser doesn't recognise this character as the
                    // start of any construct. In Strict mode that's a
                    // hard error — the input is either invalid ABC or
                    // uses a feature the parser doesn't yet support, and
                    // both cases want to surface loudly. In Generous and
                    // Fragment modes we keep the historical warning so
                    // existing callers aren't broken.
                    let msg = format!("Unrecognized construct '{}'", c);
                    match mode {
                        ParseMode::Strict => collector.error(msg),
                        ParseMode::Generous | ParseMode::Fragment => {
                            collector.warning(format!(
                                "Skipping unknown character '{}'",
                                c
                            ))
                        }
                    }
                }
            }
            remaining = &remaining[c.len_utf8()..];
            at_line_start = false;
        }
    }

    elements
}

/// Attempt to consume a broken-rhythm group: looks back at `elements` for
/// the most recent Note, peeks ahead through `remaining` for the operator
/// chars + a following note. On a complete match, scales both durations
/// per §4.4 and appends the second note. Returns true if consumed.
fn try_broken_rhythm(remaining: &mut &str, elements: &mut Vec<Element>) -> bool {
    let op = remaining.chars().next();
    let op_char = match op {
        Some('>') => '>',
        Some('<') => '<',
        _ => return false,
    };
    let chevrons = remaining.chars().take_while(|c| *c == op_char).count();
    if chevrons == 0 || chevrons > 3 {
        return false;
    }
    // Peek past operator + optional whitespace and try to parse a note.
    let after_op = &remaining[chevrons..];
    let mut peek = after_op;
    let _ = skip_spaces(&mut peek);
    let next_note = match super::note::parse_note.parse_next(&mut peek) {
        Ok(n) => n,
        Err(_) => return false,
    };
    // Find the previous note in the element stream (skip Spaces /
    // LineBreaks / ornamental elements that don't alter rhythm). If the
    // intervening element is a Bar / Rest / Chord / etc., bail.
    let prev_idx = elements.iter().enumerate().rev().find_map(|(i, e)| match e {
        Element::Note(_) => Some(Ok(i)),
        Element::Space
        | Element::LineBreak
        | Element::ChordSymbol(_)
        | Element::Decoration(_)
        | Element::Slur(_) => None,
        _ => Some(Err(())),
    });
    let prev_idx = match prev_idx {
        Some(Ok(i)) => i,
        _ => return false,
    };

    // Ratios per §4.4. Signed chevron count: positive = lengthen left,
    // negative = lengthen right.
    let signed: i32 = chevrons as i32 * if op_char == '>' { 1 } else { -1 };
    let (a_num, a_den, b_num, b_den) = broken_rhythm_ratios(signed);

    if let Element::Note(prev) = &mut elements[prev_idx] {
        prev.duration.numerator = prev.duration.numerator.saturating_mul(a_num);
        prev.duration.denominator = prev.duration.denominator.saturating_mul(a_den);
    }
    let mut next_note = next_note;
    next_note.duration.numerator = next_note.duration.numerator.saturating_mul(b_num);
    next_note.duration.denominator = next_note.duration.denominator.saturating_mul(b_den);
    elements.push(Element::Note(next_note));
    *remaining = peek;
    true
}

/// Returns (left_num, left_den, right_num, right_den) — the per-side
/// numerator/denominator multipliers for broken rhythm. `signed` is
/// chevron count with sign: +N for `>`*N, -N for `<`*N.
fn broken_rhythm_ratios(signed: i32) -> (u16, u16, u16, u16) {
    // |n| determines magnitude: 1 → 3/2 & 1/2, 2 → 7/4 & 1/4, 3 → 15/8 & 1/8.
    let (long_num, long_den, short_num, short_den) = match signed.unsigned_abs() {
        1 => (3, 2, 1, 2),
        2 => (7, 4, 1, 4),
        3 => (15, 8, 1, 8),
        _ => (1, 1, 1, 1),
    };
    if signed > 0 {
        (long_num, long_den, short_num, short_den)
    } else {
        (short_num, short_den, long_num, long_den)
    }
}

/// Append `+:` continuation content to the most recent field-line
/// element (skipping any trailing LineBreaks/Spaces). Returns true on
/// success. Returns false if no compatible field-line is found earlier
/// in the stream — caller should warn.
fn append_continuation(elements: &mut [Element], content: &str) -> bool {
    for elem in elements.iter_mut().rev() {
        match elem {
            Element::LineBreak | Element::Space => continue,
            Element::Lyrics { text, .. } | Element::SymbolLine(text) => {
                text.push('\n');
                text.push_str(content);
                return true;
            }
            _ => return false,
        }
    }
    false
}

/// Try to consume a line-start info field. Currently handles:
///   `w:` — aligned lyrics (§5)
///   `W:` — words after the tune (§5)
///   `s:` — symbol line (§4.15)
/// Advances `remaining` past the line content (up to but not including the
/// terminating newline). Returns None if the input doesn't begin with one
/// of these markers.
fn try_parse_line_start_field(remaining: &mut &str) -> Option<Element> {
    let (constructor, prefix_len): (fn(String) -> Element, usize) =
        if remaining.starts_with("w:") {
            (|t| Element::Lyrics { aligned: true, text: t }, 2)
        } else if remaining.starts_with("W:") {
            (|t| Element::Lyrics { aligned: false, text: t }, 2)
        } else if remaining.starts_with("s:") {
            (Element::SymbolLine, 2)
        } else {
            return None;
        };
    let after_prefix = &remaining[prefix_len..];
    let line_end = after_prefix.find('\n').unwrap_or(after_prefix.len());
    let text = after_prefix[..line_end].trim().to_string();
    *remaining = &after_prefix[line_end..];
    Some(constructor(text))
}

/// Try to parse a single element from the input
fn try_parse_element(input: &mut &str, collector: &mut FeedbackCollector) -> Option<Element> {
    // Try standalone voice switch V:id (at start of line typically)
    if input.starts_with("V:") {
        let rest = &input[2..];
        // Get voice ID (up to whitespace or end of line)
        let id_end = rest
            .find(|c: char| c.is_whitespace() || c == '|')
            .unwrap_or(rest.len());
        let voice_id = rest[..id_end].trim().to_string();
        *input = &rest[id_end..];
        return Some(Element::VoiceSwitch(voice_id));
    }

    // Try bar lines first (they can be multi-character)
    if let Some(bar) = try_parse_bar(input) {
        return Some(Element::Bar(bar));
    }

    // Try tuplet
    if let Some(tuplet) = try_parse_tuplet(input, collector) {
        return Some(Element::Tuplet(tuplet));
    }

    // Try chord symbol "G"
    if input.starts_with('"') {
        if let Ok(symbol) = parse_chord_symbol.parse_next(input) {
            return Some(Element::ChordSymbol(symbol));
        }
    }

    // Try chord [CEG]
    if input.starts_with('[') {
        // Could be chord or inline field
        if input.len() >= 3 && input.chars().nth(2) == Some(':') {
            // Check for voice switch [V:id]
            if input.starts_with("[V:") {
                if let Some(field) = try_parse_inline_field(input) {
                    return Some(Element::VoiceSwitch(field.value));
                }
            }
            // Other inline field [M:3/4]
            if let Some(field) = try_parse_inline_field(input) {
                return Some(Element::InlineField(field));
            }
        }
        if let Ok(chord) = parse_chord.parse_next(input) {
            return Some(Element::Chord(chord));
        }
    }

    // Try rest
    if input.starts_with('z') || input.starts_with('x') || input.starts_with('Z') {
        if let Ok(rest) = parse_rest.parse_next(input) {
            return Some(Element::Rest(rest));
        }
    }

    // Try grace notes
    if input.starts_with('{') {
        if let Some(grace) = try_parse_grace_notes(input) {
            return Some(grace);
        }
    }

    // Try decoration
    if let Some(dec) = try_parse_decoration(input) {
        return Some(Element::Decoration(dec));
    }

    // Try slur
    if input.starts_with('(') && !input.chars().nth(1).is_some_and(|c| c.is_ascii_digit()) {
        *input = &input[1..];
        return Some(Element::Slur(crate::ast::SlurBoundary::Start));
    }
    if input.starts_with(')') {
        *input = &input[1..];
        return Some(Element::Slur(crate::ast::SlurBoundary::End));
    }

    // Try note (must be last as it's most general for single chars)
    if let Ok(note) = parse_note.parse_next(input) {
        return Some(Element::Note(note));
    }

    None
}

/// Try to parse a bar line
fn try_parse_bar(input: &mut &str) -> Option<Bar> {
    // Order matters - try longer patterns first
    if input.starts_with("|]") {
        *input = &input[2..];
        return Some(Bar::End);
    }
    if input.starts_with("[|") {
        *input = &input[2..];
        return Some(Bar::Start);
    }
    if input.starts_with("||") {
        *input = &input[2..];
        return Some(Bar::Double);
    }
    if input.starts_with("|:") {
        *input = &input[2..];
        return Some(Bar::RepeatStart);
    }
    if input.starts_with(":|") {
        // Check for :|2 etc.
        if input.len() >= 3 && input.chars().nth(2).is_some_and(|c| c.is_ascii_digit()) {
            *input = &input[2..];
            // Parse the number
            let num_str: String = input.chars().take_while(|c| c.is_ascii_digit()).collect();
            *input = &input[num_str.len()..];
            return Some(Bar::SecondEnding);
        }
        *input = &input[2..];
        return Some(Bar::RepeatEnd);
    }
    if input.starts_with("::") {
        *input = &input[2..];
        return Some(Bar::RepeatBoth);
    }
    if input.starts_with("|1") || input.starts_with("|2") {
        *input = &input[2..];
        return Some(Bar::FirstEnding);
    }
    if input.starts_with('|') {
        *input = &input[1..];
        return Some(Bar::Single);
    }

    None
}

/// Try to parse a tuplet (3abc
fn try_parse_tuplet(input: &mut &str, collector: &mut FeedbackCollector) -> Option<Tuplet> {
    if !input.starts_with('(') {
        return None;
    }

    // Check if followed by a digit
    if input.len() < 2 || !input.chars().nth(1).is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }

    *input = &input[1..]; // consume '('

    // Parse p (number of notes)
    let p_str: String = input.chars().take_while(|c| c.is_ascii_digit()).collect();
    *input = &input[p_str.len()..];
    let p: u8 = p_str.parse().unwrap_or(3);

    // Default q based on p per ABC standard
    let default_q = match p {
        2 => 3,
        3 => 2,
        4 => 3,
        5 => 2, // or 3 depending on meter
        6 => 2,
        7 => 2, // or 3
        8 => 3,
        9 => 2, // or 3
        _ => 2,
    };

    // Check for explicit :q:r
    let (q, r) = if input.starts_with(':') {
        *input = &input[1..];
        let q_str: String = input.chars().take_while(|c| c.is_ascii_digit()).collect();
        *input = &input[q_str.len()..];
        let q: u8 = if q_str.is_empty() {
            default_q
        } else {
            q_str.parse().unwrap_or(default_q)
        };

        let r = if input.starts_with(':') {
            *input = &input[1..];
            let r_str: String = input.chars().take_while(|c| c.is_ascii_digit()).collect();
            *input = &input[r_str.len()..];
            if r_str.is_empty() {
                p
            } else {
                r_str.parse().unwrap_or(p)
            }
        } else {
            p
        };

        (q, r)
    } else {
        (default_q, p)
    };

    // Parse r elements
    let mut elements = Vec::new();
    for _ in 0..r {
        // Skip spaces
        skip_spaces(input);

        if let Some(elem) = try_parse_element(input, collector) {
            elements.push(elem);
        } else {
            break;
        }
    }

    Some(Tuplet { p, q, elements })
}

/// Try to parse grace notes {g} or {/g}
fn try_parse_grace_notes(input: &mut &str) -> Option<Element> {
    if !input.starts_with('{') {
        return None;
    }

    *input = &input[1..]; // consume '{'

    let acciaccatura = if input.starts_with('/') {
        *input = &input[1..];
        true
    } else {
        false
    };

    let mut notes = Vec::new();
    while !input.starts_with('}') && !input.is_empty() {
        if let Ok(note) = parse_note.parse_next(input) {
            notes.push(note);
        } else {
            // Skip unknown char
            if let Some(c) = input.chars().next() {
                *input = &input[c.len_utf8()..];
            } else {
                break;
            }
        }
    }

    if input.starts_with('}') {
        *input = &input[1..];
    }

    if notes.is_empty() {
        None
    } else {
        Some(Element::GraceNotes {
            acciaccatura,
            notes,
        })
    }
}

/// Try to parse an inline field [M:3/4]
fn try_parse_inline_field(input: &mut &str) -> Option<InfoField> {
    if !input.starts_with('[') {
        return None;
    }

    if let Some(end) = input.find(']') {
        let content = &input[1..end];
        if content.len() >= 2 && content.chars().nth(1) == Some(':') {
            let field_type = content.chars().next().unwrap();
            let value = content[2..].to_string();
            *input = &input[end + 1..];
            return Some(InfoField { field_type, value });
        }
    }

    None
}

/// Try to parse a decoration
fn try_parse_decoration(input: &mut &str) -> Option<crate::ast::Decoration> {
    use crate::ast::Decoration;

    // Short form decorations
    if input.starts_with('.') {
        *input = &input[1..];
        return Some(Decoration::Staccato);
    }
    if input.starts_with('~') {
        *input = &input[1..];
        return Some(Decoration::Roll);
    }
    if input.starts_with('H') && !input.chars().nth(1).is_some_and(|c| c.is_ascii_lowercase()) {
        // H not followed by lowercase (which would be a note like Ha - invalid anyway)
        // Actually H alone is fermata, but H followed by uppercase might be note
        // Let's be conservative
        if input.len() == 1
            || !input
                .chars()
                .nth(1)
                .is_some_and(|c| c.is_ascii_alphabetic())
        {
            *input = &input[1..];
            return Some(Decoration::Fermata);
        }
    }
    if input.starts_with('T')
        && !input.chars().nth(1).is_some_and(|c| c.is_ascii_lowercase())
        && (input.len() == 1
            || !input
                .chars()
                .nth(1)
                .is_some_and(|c| c.is_ascii_alphabetic()))
    {
        *input = &input[1..];
        return Some(Decoration::Trill);
    }
    if input.starts_with('u')
        && !input
            .chars()
            .nth(1)
            .is_some_and(|c| c.is_ascii_alphabetic())
    {
        *input = &input[1..];
        return Some(Decoration::UpBow);
    }
    if input.starts_with('v')
        && !input
            .chars()
            .nth(1)
            .is_some_and(|c| c.is_ascii_alphabetic())
    {
        *input = &input[1..];
        return Some(Decoration::DownBow);
    }

    // Long form decorations !trill!, !fermata!, etc. — and the
    // equivalent +trill+, +fermata+ form per §4.14 (older syntax used
    // when `!` collided with line-break markers in some dialects).
    if let Some(name_end) = matched_long_form_decoration(input) {
        let name = &input[1..name_end];
        *input = &input[name_end + 1..];
        return Some(decoration_from_name(name));
    }

    None
}

/// If `input` starts with `!…!` or `+…+`, returns the index of the
/// closing delimiter (i.e. the index of the second `!` / `+`). Returns
/// None otherwise.
fn matched_long_form_decoration(input: &str) -> Option<usize> {
    let delim = match input.chars().next()? {
        '!' => '!',
        '+' => '+',
        _ => return None,
    };
    // Find the closing delimiter. Restrict the search to the current
    // line — if there's no close before end-of-line, this isn't a
    // decoration, just a stray `+` (e.g. an inline math char) and we
    // bail so it can hit other handlers.
    let after = &input[1..];
    let end = after.find(|c| c == delim || c == '\n')?;
    if after.as_bytes()[end] != delim as u8 {
        return None;
    }
    Some(end + 1)
}

fn decoration_from_name(name: &str) -> crate::ast::Decoration {
    use crate::ast::Decoration;
    match name {
        "trill" => Decoration::Trill,
        "fermata" => Decoration::Fermata,
        "accent" => Decoration::Accent,
        "staccato" => Decoration::Staccato,
        "roll" => Decoration::Roll,
        "upbow" => Decoration::UpBow,
        "downbow" => Decoration::DownBow,
        "turn" => Decoration::Turn,
        "mordent" => Decoration::Mordent { upper: true },
        "lowermordent" => Decoration::Mordent { upper: false },
        "p" => Decoration::Dynamic(crate::ast::Dynamic::P),
        "pp" => Decoration::Dynamic(crate::ast::Dynamic::PP),
        "ppp" => Decoration::Dynamic(crate::ast::Dynamic::PPP),
        "mp" => Decoration::Dynamic(crate::ast::Dynamic::MP),
        "mf" => Decoration::Dynamic(crate::ast::Dynamic::MF),
        "f" => Decoration::Dynamic(crate::ast::Dynamic::F),
        "ff" => Decoration::Dynamic(crate::ast::Dynamic::FF),
        "fff" => Decoration::Dynamic(crate::ast::Dynamic::FFF),
        "crescendo(" | "<(" => Decoration::Crescendo { start: true },
        "crescendo)" | "<)" => Decoration::Crescendo { start: false },
        "diminuendo(" | ">(" => Decoration::Diminuendo { start: true },
        "diminuendo)" | ">)" => Decoration::Diminuendo { start: false },
        other => Decoration::Other(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::NoteName;

    #[test]
    fn test_parse_simple_body() {
        let mut collector = FeedbackCollector::new();
        let elements = parse_body("CDEF|", &mut collector, ParseMode::Generous);

        let notes: Vec<_> = elements
            .iter()
            .filter_map(|e| match e {
                Element::Note(n) => Some(n),
                _ => None,
            })
            .collect();

        assert_eq!(notes.len(), 4);
        assert_eq!(notes[0].pitch, NoteName::C);
    }

    #[test]
    fn test_parse_bar_types() {
        let mut collector = FeedbackCollector::new();
        let elements = parse_body("|:C:|D||E|]", &mut collector, ParseMode::Generous);

        let bars: Vec<_> = elements
            .iter()
            .filter_map(|e| match e {
                Element::Bar(b) => Some(b),
                _ => None,
            })
            .collect();

        assert!(bars.contains(&&Bar::RepeatStart));
        assert!(bars.contains(&&Bar::RepeatEnd));
        assert!(bars.contains(&&Bar::Double));
        assert!(bars.contains(&&Bar::End));
    }

    #[test]
    fn test_parse_triplet() {
        let mut collector = FeedbackCollector::new();
        let elements = parse_body("(3CDE", &mut collector, ParseMode::Generous);

        let tuplets: Vec<_> = elements
            .iter()
            .filter_map(|e| match e {
                Element::Tuplet(t) => Some(t),
                _ => None,
            })
            .collect();

        assert_eq!(tuplets.len(), 1);
        assert_eq!(tuplets[0].p, 3);
        assert_eq!(tuplets[0].q, 2);
        assert_eq!(tuplets[0].elements.len(), 3);
    }

    #[test]
    fn test_parse_grace_notes() {
        let mut collector = FeedbackCollector::new();
        let elements = parse_body("{g}A", &mut collector, ParseMode::Generous);

        let graces: Vec<_> = elements
            .iter()
            .filter(|e| matches!(e, Element::GraceNotes { .. }))
            .collect();

        assert_eq!(graces.len(), 1);
    }

    #[test]
    fn test_parse_acciaccatura() {
        let mut collector = FeedbackCollector::new();
        let elements = parse_body("{/g}A", &mut collector, ParseMode::Generous);

        let graces: Vec<_> = elements
            .iter()
            .filter_map(|e| match e {
                Element::GraceNotes { acciaccatura, .. } => Some(*acciaccatura),
                _ => None,
            })
            .collect();

        assert_eq!(graces.len(), 1);
        assert!(graces[0]);
    }

    #[test]
    fn test_parse_decorations() {
        let mut collector = FeedbackCollector::new();
        let elements = parse_body(".C~D!trill!E", &mut collector, ParseMode::Generous);

        let decorations: Vec<_> = elements
            .iter()
            .filter_map(|e| match e {
                Element::Decoration(d) => Some(d),
                _ => None,
            })
            .collect();

        assert!(decorations.len() >= 2);
    }

    #[test]
    fn test_parse_inline_field() {
        let mut collector = FeedbackCollector::new();
        let elements = parse_body("CD[M:3/4]EF", &mut collector, ParseMode::Generous);

        let fields: Vec<_> = elements
            .iter()
            .filter_map(|e| match e {
                Element::InlineField(f) => Some(f),
                _ => None,
            })
            .collect();

        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].field_type, 'M');
        assert_eq!(fields[0].value, "3/4");
    }

    #[test]
    fn test_parse_comments() {
        let mut collector = FeedbackCollector::new();
        let elements = parse_body("CD % comment\nEF", &mut collector, ParseMode::Generous);

        let notes: Vec<_> = elements
            .iter()
            .filter_map(|e| match e {
                Element::Note(n) => Some(n),
                _ => None,
            })
            .collect();

        assert_eq!(notes.len(), 4);
    }

    #[test]
    fn test_midi_directive_in_body_warns() {
        use crate::feedback::FeedbackLevel;

        let mut collector = FeedbackCollector::new();
        let _elements = parse_body("CD\n%%MIDI program 56\nEF", &mut collector, ParseMode::Generous);

        // Should have a warning about %%MIDI in body
        let warnings: Vec<_> = collector
            .feedback()
            .iter()
            .filter(|f| f.level == FeedbackLevel::Warning)
            .collect();
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].message.contains("%%MIDI"),
            "Warning should mention %%MIDI"
        );
        assert!(
            warnings[0].message.contains("before K:"),
            "Warning should suggest moving before K:"
        );
    }
}
