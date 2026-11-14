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
