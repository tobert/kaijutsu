//! Tests for ABC v2.1 §3.2 mid-body info-field lines.
//!
//! Most info fields can appear at body line-start to change state
//! mid-tune. Bracketed inline form `[K:G]` was already supported; this
//! adds the standalone line form (`K:G`, `M:3/4`, etc.).

use kaijutsu_abc::{parse_with_mode, Element, ParseMode};

fn inline_fields_in_body(tune: &kaijutsu_abc::Tune) -> Vec<(char, String)> {
    tune.voices
        .iter()
        .flat_map(|v| v.elements.iter())
        .filter_map(|e| match e {
            Element::InlineField(f) => Some((f.field_type, f.value.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn mid_body_meter_change() {
    let abc = "CDEF|\nM:3/4\nGAB|\n";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    assert!(!result.has_errors(), "feedback: {:?}", result.feedback);

    let fields = inline_fields_in_body(&result.value[0]);
    assert_eq!(fields, vec![('M', "3/4".to_string())]);
}

#[test]
fn mid_body_key_change() {
    let abc = "CDEF|\nK:G\nGABc|\n";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    assert!(!result.has_errors(), "feedback: {:?}", result.feedback);

    let fields = inline_fields_in_body(&result.value[0]);
    assert_eq!(fields, vec![('K', "G".to_string())]);
}

#[test]
fn mid_body_with_continuation_then_directive_then_field() {
    // From spec §6.1 fixture 07. `\<newline>` keeps us on the previous
    // logical line, but the `%%` directive's own newline resets
    // at_line_start so the subsequent `M:9/8` does become an
    // InlineField.
    let abc = "abc cab|\\\n%%setbarnb 10\nM:9/8\nabc cba abc|";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    assert!(!result.has_errors(), "feedback: {:?}", result.feedback);

    let fields = inline_fields_in_body(&result.value[0]);
    assert_eq!(fields.len(), 1, "fields: {:?}", fields);
    assert_eq!(fields[0], ('M', "9/8".to_string()));
}

#[test]
fn mid_body_k_with_attributes() {
    // The §4.6 fixture stacks several K: forms; lines 2+ are mid-body
    // and should now be captured as InlineField. Attribute parsing
    // beyond the captured raw value is left to whatever consumes them.
    let abc = "K:C\nCDEF|\nK:perc stafflines=1\n";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    assert!(!result.has_errors(), "feedback: {:?}", result.feedback);

    let fields = inline_fields_in_body(&result.value[0]);
    assert_eq!(fields, vec![('K', "perc stafflines=1".to_string())]);
}

#[test]
fn mid_body_does_not_eat_lyrics_or_symbol_lines() {
    // Don't regress: `w:` and `s:` still go to their dedicated paths,
    // not the generic info-field path.
    let abc = "C D E F|\nw: doh re mi fa\ns: \"C\" * \"F\" *\n";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    let elems = &result.value[0].voices[0].elements;
    let has_lyrics = elems.iter().any(|e| matches!(e, Element::Lyrics { .. }));
    let has_symbol = elems.iter().any(|e| matches!(e, Element::SymbolLine(_)));
    let has_inline_w = elems
        .iter()
        .any(|e| matches!(e, Element::InlineField(f) if f.field_type == 'w'));
    assert!(has_lyrics, "w: line should become Lyrics, not InlineField");
    assert!(has_symbol, "s: line should become SymbolLine");
    assert!(!has_inline_w, "w: should NOT be captured as InlineField");
}

#[test]
fn mid_body_v_switch_still_works() {
    // V:N at body line-start should still create a VoiceSwitch, not an
    // InlineField, so existing multi-voice routing keeps working.
    let abc = "X:1\nT:Test\nM:4/4\nL:1/4\nK:C\nV:1\nCD|\nV:2\nEF|\n";
    let result = parse_with_mode(abc, ParseMode::Generous);
    assert!(!result.has_errors(), "feedback: {:?}", result.feedback);
    assert_eq!(
        result.value[0].voices.len(),
        2,
        "still expected 2 voices, got: {:?}",
        result.value[0].voices.iter().map(|v| &v.id).collect::<Vec<_>>(),
    );
}

#[test]
fn inline_info_field_emits_no_skipping_warnings() {
    let abc = "CDEF|\nM:3/4\nP:A\nN:a note about this\nGAB|\n";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    let skip = result
        .feedback
        .iter()
        .filter(|f| f.message.contains("Skipping unknown character"))
        .count();
    assert_eq!(skip, 0, "feedback: {:?}", result.feedback);
}
