//! The drum chair's ABC: the header it tells the player to write and the
//! General MIDI note spellings it teaches render as the notes it names, on
//! the percussion channel.

use kaijutsu_abc::{midi, parse, MidiParams};

const CHAIR: &str = include_str!("../../../assets/defaults/rc/musician/chairs/drums.md");

/// The chair's fenced header block, as the player is told to write it.
fn chair_header() -> String {
    let fenced = CHAIR.split("```").nth(1).expect("the drums chair fences its header");
    fenced.trim_start_matches('\n').to_string()
}

fn note_ons(tune: &kaijutsu_abc::Tune) -> Vec<(u8, u8)> {
    midi::events(tune, &MidiParams::default())
        .iter()
        .filter(|e| e.data.first().map(|b| b & 0xF0) == Some(0x90))
        .map(|e| (e.data[0] & 0x0F, e.data[1]))
        .collect()
}

#[test]
fn the_chair_header_and_gm_spellings_render_kick_snare_and_hat_on_channel_ten() {
    // Beat 1 kick+hat chord, 2 snare, 3 kick+hat chord, 4 snare.
    let abc = format!("{}[C,,^F,,] D,, [C,,^F,,] D,, |\n", chair_header());
    let parsed = parse(&abc);
    assert_eq!(parsed.errors().count(), 0, "the chair's tune parses cleanly: {abc}");
    let tune = &parsed.value[0];
    assert_eq!(tune.header.midi_channel, Some(9), "channel 10 is wire index 9");
    let ons = note_ons(tune);
    let notes: Vec<u8> = ons.iter().map(|&(_, n)| n).collect();
    let mut sorted = notes.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, vec![36, 36, 38, 38, 42, 42], "kick 36, snare 38, closed hat 42: {ons:?}");
    assert!(ons.iter().all(|&(ch, _)| ch == 9), "every note is on the percussion channel: {ons:?}");
}

#[test]
fn a_chord_takes_one_beat() {
    let abc = format!("{}[C,,^F,,] D,, [C,,^F,,] D,, |\n", chair_header());
    let tune = &parse(&abc).value[0];
    let ticks: Vec<u32> = midi::events(tune, &MidiParams::default())
        .iter()
        .filter(|e| e.data.first().map(|b| b & 0xF0) == Some(0x90))
        .map(|e| e.tick)
        .collect();
    let beat = u32::from(MidiParams::default().ticks_per_beat);
    assert_eq!(ticks, vec![0, 0, beat, 2 * beat, 2 * beat, 3 * beat], "two drums share a beat, four beats a bar");
}

/// The kernel validates a timed phrase with a Strict parse; the chair's header
/// and a chord body must pass it.
#[test]
fn the_strict_parse_the_kernel_validates_with_accepts_the_drums_tune() {
    let abc = format!("{}[C,,^F,,] D,, [C,,^F,,] D,, |\n", chair_header());
    let parsed = kaijutsu_abc::parse_with_mode(&abc, kaijutsu_abc::ParseMode::Strict);
    assert!(!parsed.has_errors(), "{:?}", parsed.errors().map(|f| f.message.clone()).collect::<Vec<_>>());
    assert_eq!(parsed.value.len(), 1);
}

fn engraved_heads(abc: &str) -> Vec<(u32, f64)> {
    use kaijutsu_abc::engrave::{layout, EngravingElement, EngravingOptions};

    let parsed = parse(abc);
    assert!(!parsed.has_errors(), "parse errors: {:?}", parsed.errors().collect::<Vec<_>>());
    layout::engrave(&parsed.value[0], &EngravingOptions::default())
        .iter()
        .filter_map(|element| match element {
            EngravingElement::Glyph { codepoint: cp @ (0xE0A4 | 0xE0A9), y, .. } => Some((*cp, *y)),
            _ => None,
        })
        .collect()
}

fn assert_drum_heads(abc: &str) {
    let mut heads = engraved_heads(abc);
    heads.sort_by(|a, b| a.1.total_cmp(&b.1));
    // Default staff spacing is 10: hat above the top line, snare in the
    // second space from the top, and kick in the bottom space.
    assert_eq!(heads, vec![(0xE0A9, -5.0), (0xE0A4, 15.0), (0xE0A4, 35.0)]);
}

#[test]
fn channel_ten_engraves_gm_drum_notes_and_chords_on_the_staff() {
    for body in ["C,, D,, ^F,, |", "[C,,D,,^F,,] |"] {
        let abc = format!("X:1\nT:Drums\n%%MIDI channel 10\nM:4/4\nL:1/4\nK:C\n{body}\n");
        assert_drum_heads(&abc);
    }
}

#[test]
fn percussion_clef_engraves_gm_drum_notes_and_chords_on_the_staff() {
    for body in ["C,, D,, ^F,, |", "[C,,D,,^F,,] |"] {
        let abc = format!("X:1\nT:Drums\nM:4/4\nL:1/4\nK:perc\n{body}\n");
        assert_drum_heads(&abc);
    }
}

#[test]
fn melodic_notes_keep_their_pitch_based_positions() {
    let heads = engraved_heads("X:1\nT:Melody\nM:4/4\nL:1/4\nK:C\nC D F |\n");
    assert_eq!(heads, vec![(0xE0A4, 50.0), (0xE0A4, 45.0), (0xE0A4, 35.0)]);
}

#[test]
fn wrapped_percussion_systems_use_key_pitch_without_sharp_glyphs() {
    use kaijutsu_abc::engrave::{layout, EngravingElement, EngravingOptions};

    let abc = "X:1\nT:Drums\n%%MIDI channel 10\nM:4/4\nL:1/4\nK:G\nF,, F,, F,, F,, | F,, F,, F,, F,, | F,, F,, F,, F,, |\n";
    let parsed = parse(abc);
    assert!(!parsed.has_errors(), "parse errors: {:?}", parsed.errors().collect::<Vec<_>>());
    let tune = &parsed.value[0];
    assert!(note_ons(tune).iter().all(|&(channel, pitch)| channel == 9 && pitch == 42));
    assert_eq!(note_ons(tune).len(), 12);

    let systems = layout::engrave_systems(tune, &EngravingOptions::default(), 200.0);
    assert!(systems.len() >= 2, "expected wrapped systems, got {}", systems.len());
    for system in &systems {
        assert!(system.iter().any(|e| matches!(e, EngravingElement::Glyph { codepoint: 0xE0A9, y, .. } if (*y + 5.0).abs() < 0.01)), "each system has a closed-hat head");
        assert!(!system.iter().any(|e| matches!(e, EngravingElement::Glyph { codepoint: 0xE262, .. })), "percussion displays no sharp signs");
    }
}

#[test]
fn an_unassigned_percussion_note_uses_the_middle_line() {
    for body in ["C,,,,", "c'''''"] {
        let abc = format!("X:1\nM:4/4\nL:1/4\nK:perc\n{body}|\n");
        assert_eq!(engraved_heads(&abc), vec![(0xE0A4, 20.0)]);
    }
}
