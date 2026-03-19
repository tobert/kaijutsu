//! Fixture-based tests for ABC parsing and MIDI generation.
//!
//! Each .abc file in tests/fixtures/ is parsed and converted to MIDI.

use kaijutsu_abc::{parse, to_midi, MidiParams};
use std::fs;
use std::path::Path;

fn test_fixture(name: &str) {
    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(format!("{}.abc", name));

    let abc_content = fs::read_to_string(&fixture_path)
        .unwrap_or_else(|e| panic!("Failed to read fixture {}: {}", name, e));

    // Parse should succeed without errors
    let result = parse(&abc_content);
    assert!(
        !result.has_errors(),
        "Fixture {} had parse errors: {:?}",
        name,
        result.feedback
    );

    // MIDI generation should produce valid output
    let midi = to_midi(&result.value, &MidiParams::default());

    // Valid MIDI starts with MThd
    assert_eq!(
        &midi[0..4],
        b"MThd",
        "Fixture {} produced invalid MIDI header",
        name
    );

    // Should have reasonable length
    assert!(
        midi.len() > 20,
        "Fixture {} produced suspiciously short MIDI: {} bytes",
        name,
        midi.len()
    );

    println!(
        "Fixture {}: {} bytes MIDI, {} warnings",
        name,
        midi.len(),
        result.feedback.len()
    );
}

#[test]
fn test_fixture_simple_melody() {
    test_fixture("simple_melody");
}

#[test]
fn test_fixture_accidentals() {
    test_fixture("accidentals");
}

#[test]
fn test_fixture_durations() {
    test_fixture("durations");
}

#[test]
fn test_fixture_chords() {
    test_fixture("chords");
}

#[test]
fn test_fixture_repeats() {
    test_fixture("repeats");
}

#[test]
fn test_fixture_ties() {
    test_fixture("ties");
}

#[test]
fn test_fixture_triplets() {
    test_fixture("triplets");
}

#[test]
fn test_fixture_keys() {
    test_fixture("keys");
}

#[test]
fn test_fixture_two_voices() {
    test_fixture("two_voices");
}

#[test]
fn test_multivoice_structure() {
    let abc = r#"X:1
T:Two Voice Test
M:4/4
L:1/4
V:1 name="Melody"
V:2 name="Bass"
K:C
V:1
cdef|
V:2
C,D,E,F,|
"#;

    let result = parse(abc);
    assert!(!result.has_errors(), "Parse errors: {:?}", result.feedback);

    // Should have 2 voice definitions
    assert_eq!(result.value.header.voice_defs.len(), 2);
    assert_eq!(result.value.header.voice_defs[0].id, "1");
    assert_eq!(
        result.value.header.voice_defs[0].name,
        Some("Melody".to_string())
    );
    assert_eq!(result.value.header.voice_defs[1].id, "2");
    assert_eq!(
        result.value.header.voice_defs[1].name,
        Some("Bass".to_string())
    );

    // Should have 2 voices in the tune
    assert_eq!(result.value.voices.len(), 2);

    // Each voice should have content (notes)
    let voice1_notes = result.value.voices[0]
        .elements
        .iter()
        .filter(|e| matches!(e, kaijutsu_abc::Element::Note(_)))
        .count();
    let voice2_notes = result.value.voices[1]
        .elements
        .iter()
        .filter(|e| matches!(e, kaijutsu_abc::Element::Note(_)))
        .count();

    assert_eq!(voice1_notes, 4, "Voice 1 should have 4 notes");
    assert_eq!(voice2_notes, 4, "Voice 2 should have 4 notes");

    // MIDI should be format 1 (check header)
    let midi = to_midi(&result.value, &MidiParams::default());
    assert_eq!(&midi[0..4], b"MThd");
    // Format at bytes 8-9
    assert_eq!(midi[9], 1, "Should be format 1 for multiple voices");
}

/// Test that all fixtures in the directory are covered by tests
#[test]
fn test_all_fixtures_have_tests() {
    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");

    let fixture_names: Vec<_> = fs::read_dir(&fixtures_dir)
        .expect("Failed to read fixtures directory")
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension()? == "abc" {
                path.file_stem()?.to_str().map(|s| s.to_string())
            } else {
                None
            }
        })
        .collect();

    // List of fixtures we have tests for
    let tested = [
        "simple_melody",
        "accidentals",
        "durations",
        "chords",
        "two_voices",
        "three_voices",
        "repeats",
        "ties",
        "triplets",
        "keys",
    ];

    for name in &fixture_names {
        assert!(
            tested.contains(&name.as_str()),
            "Fixture {} exists but has no test",
            name
        );
    }
}

// ============================================================================
// MIDI Parsing Helpers for Multi-Track Verification
// ============================================================================

/// A note event extracted from MIDI: (absolute_tick, channel, pitch, velocity, is_on)
#[derive(Debug, Clone, PartialEq)]
struct MidiNoteEvent {
    tick: u32,
    channel: u8,
    pitch: u8,
    velocity: u8,
    is_note_on: bool,
}

/// Parse MIDI variable-length quantity, returns (value, bytes_consumed)
fn parse_variable_length(data: &[u8]) -> (u32, usize) {
    let mut value = 0u32;
    let mut bytes = 0;
    for &byte in data {
        bytes += 1;
        value = (value << 7) | (byte & 0x7F) as u32;
        if byte & 0x80 == 0 {
            break;
        }
    }
    (value, bytes)
}

/// Extract note events from a single MIDI track chunk
fn parse_track_events(track_data: &[u8]) -> Vec<MidiNoteEvent> {
    let mut events = Vec::new();
    let mut pos = 0;
    let mut absolute_tick = 0u32;
    let mut running_status: Option<u8> = None;

    while pos < track_data.len() {
        // Parse delta time
        let (delta, delta_bytes) = parse_variable_length(&track_data[pos..]);
        pos += delta_bytes;
        absolute_tick += delta;

        if pos >= track_data.len() {
            break;
        }

        let status = track_data[pos];

        // Meta event
        if status == 0xFF {
            pos += 1;
            if pos >= track_data.len() {
                break;
            }
            let _meta_type = track_data[pos];
            pos += 1;
            let (length, len_bytes) = parse_variable_length(&track_data[pos..]);
            pos += len_bytes + length as usize;
            continue;
        }

        // SysEx
        if status == 0xF0 || status == 0xF7 {
            pos += 1;
            let (length, len_bytes) = parse_variable_length(&track_data[pos..]);
            pos += len_bytes + length as usize;
            continue;
        }

        // Channel message
        let (cmd, channel, data_start) = if status & 0x80 != 0 {
            running_status = Some(status);
            (status & 0xF0, status & 0x0F, pos + 1)
        } else if let Some(rs) = running_status {
            (rs & 0xF0, rs & 0x0F, pos)
        } else {
            pos += 1;
            continue;
        };

        match cmd {
            0x80 => {
                // Note Off
                if data_start + 1 < track_data.len() {
                    events.push(MidiNoteEvent {
                        tick: absolute_tick,
                        channel,
                        pitch: track_data[data_start],
                        velocity: track_data[data_start + 1],
                        is_note_on: false,
                    });
                }
                pos = data_start + 2;
            }
            0x90 => {
                // Note On (velocity 0 = note off)
                if data_start + 1 < track_data.len() {
                    let velocity = track_data[data_start + 1];
                    events.push(MidiNoteEvent {
                        tick: absolute_tick,
                        channel,
                        pitch: track_data[data_start],
                        velocity,
                        is_note_on: velocity > 0,
                    });
                }
                pos = data_start + 2;
            }
            0xA0 | 0xB0 | 0xE0 => {
                pos = data_start + 2;
            }
            0xC0 | 0xD0 => {
                pos = data_start + 1;
            }
            _ => {
                pos += 1;
            }
        }
    }

    events
}

/// Parse all tracks from a MIDI file, returning note events per track
fn parse_midi_tracks(midi: &[u8]) -> Vec<Vec<MidiNoteEvent>> {
    let mut tracks = Vec::new();

    // Skip header: MThd (4) + length (4) + format (2) + ntrks (2) + division (2) = 14 bytes
    let mut pos = 14;

    while pos + 8 <= midi.len() {
        // Check for MTrk
        if &midi[pos..pos + 4] != b"MTrk" {
            break;
        }
        pos += 4;

        // Track length
        let track_len =
            u32::from_be_bytes([midi[pos], midi[pos + 1], midi[pos + 2], midi[pos + 3]]) as usize;
        pos += 4;

        if pos + track_len > midi.len() {
            break;
        }

        let track_data = &midi[pos..pos + track_len];
        tracks.push(parse_track_events(track_data));

        pos += track_len;
    }

    tracks
}

// ============================================================================
// Multi-Voice/Polyphony Tests
// ============================================================================

/// Test 1: Verify notes land on correct MIDI channels in multi-track output
#[test]
fn test_multitrack_midi_content() {
    let abc = r#"X:1
T:Channel Test
M:4/4
L:1/4
V:1
V:2
K:C
V:1
c|
V:2
C,|
"#;

    let result = parse(abc);
    assert!(!result.has_errors(), "Parse errors: {:?}", result.feedback);

    let midi = to_midi(&result.value, &MidiParams::default());
    let tracks = parse_midi_tracks(&midi);

    // Format 1: Track 0 = tempo, Track 1 = voice 1, Track 2 = voice 2
    assert!(
        tracks.len() >= 3,
        "Expected 3 tracks (tempo + 2 voices), got {}",
        tracks.len()
    );

    // Track 1 (voice 1): should have note c (MIDI 72) on channel 0
    let voice1_notes: Vec<_> = tracks[1].iter().filter(|e| e.is_note_on).collect();
    assert!(!voice1_notes.is_empty(), "Voice 1 track should have notes");
    assert_eq!(voice1_notes[0].channel, 0, "Voice 1 should be on channel 0");
    assert_eq!(voice1_notes[0].pitch, 72, "Voice 1 note should be c (72)");

    // Track 2 (voice 2): should have note C, (MIDI 48) on channel 1
    // C, = C at octave -1 = (0 base) + (-1 + 5) * 12 = 48
    let voice2_notes: Vec<_> = tracks[2].iter().filter(|e| e.is_note_on).collect();
    assert!(!voice2_notes.is_empty(), "Voice 2 track should have notes");
    assert_eq!(voice2_notes[0].channel, 1, "Voice 2 should be on channel 1");
    assert_eq!(voice2_notes[0].pitch, 48, "Voice 2 note should be C, (48)");
}

/// Test 2: Inline voice switching within body
#[test]
fn test_inline_voice_switch() {
    let abc = r#"X:1
T:Inline Switch Test
M:4/4
L:1/4
V:1
V:2
K:C
[V:1]cde[V:2]CDE|
"#;

    let result = parse(abc);
    assert!(!result.has_errors(), "Parse errors: {:?}", result.feedback);

    // Should have 2 voices with content routed correctly
    assert_eq!(result.value.voices.len(), 2);

    let voice1_notes: Vec<_> = result.value.voices[0]
        .elements
        .iter()
        .filter_map(|e| {
            if let kaijutsu_abc::Element::Note(n) = e {
                Some(n)
            } else {
                None
            }
        })
        .collect();
    let voice2_notes: Vec<_> = result.value.voices[1]
        .elements
        .iter()
        .filter_map(|e| {
            if let kaijutsu_abc::Element::Note(n) = e {
                Some(n)
            } else {
                None
            }
        })
        .collect();

    assert_eq!(
        voice1_notes.len(),
        3,
        "Voice 1 should have 3 notes (c, d, e)"
    );
    assert_eq!(
        voice2_notes.len(),
        3,
        "Voice 2 should have 3 notes (C, D, E)"
    );

    // Verify note pitches
    assert_eq!(voice1_notes[0].pitch, kaijutsu_abc::NoteName::C);
    assert_eq!(voice1_notes[0].octave, 1); // lowercase c
    assert_eq!(voice2_notes[0].pitch, kaijutsu_abc::NoteName::C);
    assert_eq!(voice2_notes[0].octave, 0); // uppercase C
}

/// Test 3: Three voice fixture
#[test]
fn test_fixture_three_voices() {
    test_fixture("three_voices");
}

/// Test 4: Voice transpose property - WILL FAIL until midi.rs is fixed
#[test]
fn test_voice_transpose() {
    let abc = r#"X:1
T:Transpose Test
M:4/4
L:1/4
V:1 transpose=-12
K:C
V:1
c|
"#;

    let result = parse(abc);
    assert!(!result.has_errors(), "Parse errors: {:?}", result.feedback);

    // Verify transpose was parsed
    assert_eq!(
        result.value.header.voice_defs[0].transpose,
        Some(-12),
        "Voice should have transpose=-12"
    );

    let midi = to_midi(&result.value, &MidiParams::default());
    let tracks = parse_midi_tracks(&midi);

    // Single voice = format 0 = single track (index 0)
    assert!(!tracks.is_empty(), "Should have at least one track");
    let voice_notes: Vec<_> = tracks[0].iter().filter(|e| e.is_note_on).collect();
    assert!(!voice_notes.is_empty(), "Should have notes");

    // c is normally MIDI 72, with transpose=-12 it should be 60
    assert_eq!(
        voice_notes[0].pitch, 60,
        "c with transpose=-12 should be MIDI 60, got {}",
        voice_notes[0].pitch
    );
}

/// Test 5: Voice octave property - WILL FAIL until midi.rs is fixed
#[test]
fn test_voice_octave() {
    let abc = r#"X:1
T:Octave Test
M:4/4
L:1/4
V:1 octave=-1
K:C
V:1
c|
"#;

    let result = parse(abc);
    assert!(!result.has_errors(), "Parse errors: {:?}", result.feedback);

    // Verify octave was parsed
    assert_eq!(
        result.value.header.voice_defs[0].octave,
        Some(-1),
        "Voice should have octave=-1"
    );

    let midi = to_midi(&result.value, &MidiParams::default());
    let tracks = parse_midi_tracks(&midi);

    // Single voice = format 0 = single track (index 0)
    assert!(!tracks.is_empty(), "Should have at least one track");
    let voice_notes: Vec<_> = tracks[0].iter().filter(|e| e.is_note_on).collect();
    assert!(!voice_notes.is_empty(), "Should have notes");

    // c is normally MIDI 72, with octave=-1 it should be 60
    assert_eq!(
        voice_notes[0].pitch, 60,
        "c with octave=-1 should be MIDI 60, got {}",
        voice_notes[0].pitch
    );
}

/// Test 6: Timing alignment - both voices start at tick 0
#[test]
fn test_voice_timing_alignment() {
    let abc = r#"X:1
T:Timing Test
M:4/4
L:1/4
V:1
V:2
K:C
V:1
cdef|
V:2
CDEF|
"#;

    let result = parse(abc);
    assert!(!result.has_errors(), "Parse errors: {:?}", result.feedback);

    let midi = to_midi(&result.value, &MidiParams::default());
    let tracks = parse_midi_tracks(&midi);

    assert!(
        tracks.len() >= 3,
        "Should have tempo track + 2 voice tracks"
    );

    // Both voice tracks should have first note at tick 0
    let voice1_first = tracks[1].iter().find(|e| e.is_note_on);
    let voice2_first = tracks[2].iter().find(|e| e.is_note_on);

    assert!(voice1_first.is_some(), "Voice 1 should have notes");
    assert!(voice2_first.is_some(), "Voice 2 should have notes");

    assert_eq!(
        voice1_first.unwrap().tick,
        0,
        "Voice 1 should start at tick 0"
    );
    assert_eq!(
        voice2_first.unwrap().tick,
        0,
        "Voice 2 should start at tick 0"
    );
}

// ============================================================================
// Priority 1: Correctness Verification Tests
// ============================================================================

/// Test accidental bar-scoping: ^C C C | C should produce C#, C#, C#, C
/// Accidentals persist within a bar but reset at bar lines.
#[test]
fn test_accidental_bar_scoping() {
    let abc = r#"X:1
T:Accidental Scoping Test
M:4/4
L:1/4
K:C
^C C C | C |
"#;

    let result = parse(abc);
    assert!(!result.has_errors(), "Parse errors: {:?}", result.feedback);

    let midi = to_midi(&result.value, &MidiParams::default());
    let tracks = parse_midi_tracks(&midi);

    let notes: Vec<_> = tracks[0].iter().filter(|e| e.is_note_on).collect();
    assert_eq!(notes.len(), 4, "Should have 4 notes");

    // Uppercase C = octave 0 = MIDI 60 (middle C), C# = 61
    // ABC octave formula: (octave + 5) * 12 + base_semitone
    // C at octave 0: (0 + 5) * 12 + 0 = 60
    assert_eq!(notes[0].pitch, 61, "First C should be C# (61)");
    assert_eq!(notes[1].pitch, 61, "Second C should inherit C# (61)");
    assert_eq!(notes[2].pitch, 61, "Third C should inherit C# (61)");

    // Fourth C is after bar line, should reset to C natural
    assert_eq!(
        notes[3].pitch, 60,
        "Fourth C after bar should be C natural (60)"
    );
}

/// Test key signature affects MIDI output: K:G means F becomes F#
#[test]
fn test_key_signature_affects_midi_pitch() {
    let abc = r#"X:1
T:Key Signature Test
M:4/4
L:1/4
K:G
F G A B |
"#;

    let result = parse(abc);
    assert!(!result.has_errors(), "Parse errors: {:?}", result.feedback);

    let midi = to_midi(&result.value, &MidiParams::default());
    let tracks = parse_midi_tracks(&midi);

    let notes: Vec<_> = tracks[0].iter().filter(|e| e.is_note_on).collect();
    assert_eq!(notes.len(), 4, "Should have 4 notes");

    // Uppercase notes = octave 0 = MIDI 60 base
    // F at octave 0: (0 + 5) * 12 + 5 = 65, with K:G F# = 66
    // G = 67, A = 69, B = 71
    assert_eq!(notes[0].pitch, 66, "F in K:G should be F# (66)");
    assert_eq!(notes[1].pitch, 67, "G should be 67");
    assert_eq!(notes[2].pitch, 69, "A should be 69");
    assert_eq!(notes[3].pitch, 71, "B should be 71");
}

/// Test natural accidental resets previous accidental: ^C =C should produce C#, C
#[test]
fn test_natural_accidental_resets() {
    let abc = r#"X:1
T:Natural Accidental Test
M:4/4
L:1/4
K:C
^C =C C |
"#;

    let result = parse(abc);
    assert!(!result.has_errors(), "Parse errors: {:?}", result.feedback);

    let midi = to_midi(&result.value, &MidiParams::default());
    let tracks = parse_midi_tracks(&midi);

    let notes: Vec<_> = tracks[0].iter().filter(|e| e.is_note_on).collect();
    assert_eq!(notes.len(), 3, "Should have 3 notes");

    // Uppercase C = octave 0 = MIDI 60, C# = 61
    assert_eq!(notes[0].pitch, 61, "^C should be C# (61)");
    assert_eq!(notes[1].pitch, 60, "=C should be C natural (60)");
    assert_eq!(notes[2].pitch, 60, "Third C should inherit natural (60)");
}

/// Test double repeat :: correctly doubles the content
#[test]
fn test_double_repeat() {
    // :: means end current repeat and start a new one
    // |: A B :: C D :| should produce: A B A B C D C D
    let abc = r#"X:1
T:Double Repeat Test
M:4/4
L:1/4
K:C
|: A B :: C D :|
"#;

    let result = parse(abc);
    assert!(!result.has_errors(), "Parse errors: {:?}", result.feedback);

    let midi = to_midi(&result.value, &MidiParams::default());
    let tracks = parse_midi_tracks(&midi);

    let notes: Vec<_> = tracks[0].iter().filter(|e| e.is_note_on).collect();

    // Expected: A B (first time) A B (repeat) C D (first time) C D (repeat) = 8 notes
    assert_eq!(
        notes.len(),
        8,
        "Double repeat should produce 8 notes: A B A B C D C D"
    );

    // Uppercase A = octave 0 = MIDI 69, B = 71, C = 60, D = 62
    assert_eq!(notes[0].pitch, 69, "Note 1: A");
    assert_eq!(notes[1].pitch, 71, "Note 2: B");
    assert_eq!(notes[2].pitch, 69, "Note 3: A (repeat)");
    assert_eq!(notes[3].pitch, 71, "Note 4: B (repeat)");
    assert_eq!(notes[4].pitch, 60, "Note 5: C");
    assert_eq!(notes[5].pitch, 62, "Note 6: D");
    assert_eq!(notes[6].pitch, 60, "Note 7: C (repeat)");
    assert_eq!(notes[7].pitch, 62, "Note 8: D (repeat)");
}

// ============================================================================
// Priority 2: Feature Verification Tests
// ============================================================================

/// Test multi-measure rest Z4 creates correct silence duration
#[test]
fn test_multi_measure_rest_timing() {
    // Z2 = rest for 2 full bars, then a note
    // In 4/4 with L:1/4, each bar = 4 beats = 4 * 480 = 1920 ticks
    // Z2 = 2 bars = 3840 ticks, then note at tick 3840
    let abc = r#"X:1
T:Multi-Measure Rest Test
M:4/4
L:1/4
K:C
Z2 C |
"#;

    let result = parse(abc);
    assert!(!result.has_errors(), "Parse errors: {:?}", result.feedback);

    let midi = to_midi(&result.value, &MidiParams::default());
    let tracks = parse_midi_tracks(&midi);

    let notes: Vec<_> = tracks[0].iter().filter(|e| e.is_note_on).collect();
    assert_eq!(
        notes.len(),
        1,
        "Should have 1 note after multi-measure rest"
    );

    // 2 bars * 4 beats * 480 ticks = 3840 ticks
    assert_eq!(
        notes[0].tick, 3840,
        "Note should start at tick 3840 (after Z2)"
    );
}

/// Test grace notes are parsed (documents current MIDI behavior)
#[test]
fn test_grace_notes_parsing() {
    // Grace notes {ga} before a main note
    let abc = r#"X:1
T:Grace Notes Test
M:4/4
L:1/4
K:C
{ga}c d e f |
"#;

    let result = parse(abc);
    assert!(!result.has_errors(), "Parse errors: {:?}", result.feedback);

    // Check that grace notes are in the AST
    let has_grace = result.value.voices[0]
        .elements
        .iter()
        .any(|e| matches!(e, kaijutsu_abc::Element::GraceNotes { .. }));
    assert!(has_grace, "Should have parsed grace notes");

    // Note: Grace notes are currently NOT rendered to MIDI
    // This test documents the current behavior - MIDI only has the main notes
    let midi = to_midi(&result.value, &MidiParams::default());
    let tracks = parse_midi_tracks(&midi);
    let notes: Vec<_> = tracks[0].iter().filter(|e| e.is_note_on).collect();

    // Currently only main notes (c, d, e, f) are in MIDI, grace notes are skipped
    assert_eq!(
        notes.len(),
        4,
        "MIDI has 4 main notes (grace notes not rendered)"
    );
}

/// Test invisible rest x advances time without sound
#[test]
fn test_invisible_rest() {
    // x = invisible rest (same as z but for notation purposes)
    let abc = r#"X:1
T:Invisible Rest Test
M:4/4
L:1/4
K:C
C x C |
"#;

    let result = parse(abc);
    assert!(!result.has_errors(), "Parse errors: {:?}", result.feedback);

    let midi = to_midi(&result.value, &MidiParams::default());
    let tracks = parse_midi_tracks(&midi);

    let notes: Vec<_> = tracks[0].iter().filter(|e| e.is_note_on).collect();
    assert_eq!(
        notes.len(),
        2,
        "Should have 2 notes (invisible rest is silent)"
    );

    // First C at tick 0, invisible rest for 1 beat (480 ticks), second C at tick 960
    assert_eq!(notes[0].tick, 0, "First C at tick 0");
    assert_eq!(
        notes[1].tick, 960,
        "Second C at tick 960 (after invisible rest)"
    );
}

// ============================================================================
// Priority 3: Edge Case & Robustness Tests
// ============================================================================

/// Test extreme transpose values clamp to valid MIDI range 0-127
#[test]
fn test_transpose_clamping() {
    // transpose=+60 on a high note should clamp to 127
    let abc = r#"X:1
T:Transpose Clamping Test
M:4/4
L:1/4
V:1 transpose=60
K:C
V:1
c''' |
"#;
    // c''' = very high C, octave 4 relative to lowercase c
    // c (octave 1) = 72, c' = 84, c'' = 96, c''' = 108
    // 108 + 60 = 168 → should clamp to 127

    let result = parse(abc);
    assert!(!result.has_errors(), "Parse errors: {:?}", result.feedback);

    let midi = to_midi(&result.value, &MidiParams::default());
    let tracks = parse_midi_tracks(&midi);

    let notes: Vec<_> = tracks[0].iter().filter(|e| e.is_note_on).collect();
    assert_eq!(notes.len(), 1, "Should have 1 note");
    assert_eq!(notes[0].pitch, 127, "Pitch should clamp to 127 (max MIDI)");
}

/// Test transpose clamping at low end
#[test]
fn test_transpose_clamping_low() {
    // transpose=-60 on a low note should clamp to 0
    let abc = r#"X:1
T:Transpose Clamping Low Test
M:4/4
L:1/4
V:1 transpose=-60
K:C
V:1
C,, |
"#;
    // C,, = very low C, octave -2
    // C (octave 0) = 60, C, = 48, C,, = 36
    // 36 - 60 = -24 → should clamp to 0

    let result = parse(abc);
    assert!(!result.has_errors(), "Parse errors: {:?}", result.feedback);

    let midi = to_midi(&result.value, &MidiParams::default());
    let tracks = parse_midi_tracks(&midi);

    let notes: Vec<_> = tracks[0].iter().filter(|e| e.is_note_on).collect();
    assert_eq!(notes.len(), 1, "Should have 1 note");
    assert_eq!(notes[0].pitch, 0, "Pitch should clamp to 0 (min MIDI)");
}

/// Test first/second endings - documents current limitation
/// NOTE: First/second endings are parsed but NOT correctly expanded in MIDI
#[test]
fn test_first_second_endings_parsing() {
    // |1 ... :|2 ... means first ending on first pass, second on repeat
    let abc = r#"X:1
T:First Second Endings Test
M:4/4
L:1/4
K:C
|: C D |1 E F :|2 G A |
"#;

    let result = parse(abc);
    assert!(!result.has_errors(), "Parse errors: {:?}", result.feedback);

    // Check that bar types are in the AST
    let has_first_ending = result.value.voices[0].elements.iter().any(|e| {
        matches!(
            e,
            kaijutsu_abc::Element::Bar(kaijutsu_abc::Bar::FirstEnding)
        )
    });
    let has_second_ending = result.value.voices[0].elements.iter().any(|e| {
        matches!(
            e,
            kaijutsu_abc::Element::Bar(kaijutsu_abc::Bar::SecondEnding)
        )
    });

    assert!(has_first_ending, "Should have parsed first ending |1");
    assert!(has_second_ending, "Should have parsed second ending :|2");

    // Note: Current expand_repeats() does NOT implement first/second ending logic
    // It just passes through the bar markers and does a simple repeat
    // Expected correct behavior: C D E F C D G A (8 notes)
    // Current behavior: C D E F C D E F (simple repeat, ignores endings)
    let midi = to_midi(&result.value, &MidiParams::default());
    let tracks = parse_midi_tracks(&midi);
    let notes: Vec<_> = tracks[0].iter().filter(|e| e.is_note_on).collect();

    // Document current behavior - this will need to change when endings are implemented
    // For now, just verify it doesn't crash and produces some notes
    assert!(notes.len() >= 4, "Should produce at least 4 notes");
}

/// Test chord inside tuplet - documents current LIMITATION
/// NOTE: Tuplet MIDI generation only handles Note elements, not Chord elements
#[test]
fn test_chord_in_tuplet() {
    // (3[CEG][FAC][GBD] = triplet of three chords
    let abc = r#"X:1
T:Chord in Tuplet Test
M:4/4
L:1/4
K:C
(3[CEG][FAc][GBd] |
"#;

    let result = parse(abc);
    assert!(!result.has_errors(), "Parse errors: {:?}", result.feedback);

    // Check that tuplet was parsed with chord elements
    let has_tuplet = result.value.voices[0]
        .elements
        .iter()
        .any(|e| matches!(e, kaijutsu_abc::Element::Tuplet(_)));
    assert!(has_tuplet, "Should have parsed tuplet");

    // LIMITATION: Current MIDI generator only handles Note inside Tuplet, not Chord
    // The midi.rs Tuplet handler has: `if let Element::Note(note) = elem { ... }`
    // This means chords inside tuplets are silently skipped
    let midi = to_midi(&result.value, &MidiParams::default());
    let tracks = parse_midi_tracks(&midi);
    let notes: Vec<_> = tracks[0].iter().filter(|e| e.is_note_on).collect();

    // Document current behavior: chords in tuplets produce NO notes
    // This should be 9 notes when properly implemented (3 chords * 3 notes each)
    assert_eq!(
        notes.len(),
        0,
        "LIMITATION: Chords inside tuplets are not rendered to MIDI"
    );
}

/// Test many voices handles MIDI channel overflow gracefully
#[test]
fn test_many_voices_channel_handling() {
    // Create 18 voices - more than 16 MIDI channels
    let abc = r#"X:1
T:Many Voices Test
M:4/4
L:1/4
V:1
V:2
V:3
V:4
V:5
V:6
V:7
V:8
V:9
V:10
V:11
V:12
V:13
V:14
V:15
V:16
V:17
V:18
K:C
V:1
C|
V:2
D|
V:3
E|
V:4
F|
V:5
G|
V:6
A|
V:7
B|
V:8
c|
V:9
d|
V:10
e|
V:11
f|
V:12
g|
V:13
a|
V:14
b|
V:15
c'|
V:16
d'|
V:17
e'|
V:18
f'|
"#;

    let result = parse(abc);
    assert!(!result.has_errors(), "Parse errors: {:?}", result.feedback);

    // Should not panic, channels wrap modulo 16
    let midi = to_midi(&result.value, &MidiParams::default());
    let tracks = parse_midi_tracks(&midi);

    // Should have tempo track + 18 voice tracks = 19 tracks
    assert_eq!(
        tracks.len(),
        19,
        "Should have 19 tracks (tempo + 18 voices)"
    );

    // All voices should have produced notes
    for i in 1..19 {
        let voice_notes: Vec<_> = tracks[i].iter().filter(|e| e.is_note_on).collect();
        assert!(!voice_notes.is_empty(), "Voice {} should have notes", i);
    }
}

/// Test empty voice doesn't cause issues
#[test]
fn test_empty_voice() {
    let abc = r#"X:1
T:Empty Voice Test
M:4/4
L:1/4
V:1
V:2
K:C
V:1
C D E F |
V:2
|
"#;
    // V:2 has only a bar line, no notes

    let result = parse(abc);
    assert!(!result.has_errors(), "Parse errors: {:?}", result.feedback);

    let midi = to_midi(&result.value, &MidiParams::default());
    let tracks = parse_midi_tracks(&midi);

    // Should handle gracefully - at least the tempo track and voice 1
    assert!(
        tracks.len() >= 2,
        "Should have at least tempo + voice 1 tracks"
    );

    // Voice 1 should have its notes
    let voice1_notes: Vec<_> = tracks[1].iter().filter(|e| e.is_note_on).collect();
    assert_eq!(voice1_notes.len(), 4, "Voice 1 should have 4 notes");
}

/// Test very long note duration
#[test]
fn test_long_duration() {
    // C16 = 16 times the unit length
    let abc = r#"X:1
T:Long Duration Test
M:4/4
L:1/4
K:C
C16 |
"#;

    let result = parse(abc);
    assert!(!result.has_errors(), "Parse errors: {:?}", result.feedback);

    let midi = to_midi(&result.value, &MidiParams::default());
    let tracks = parse_midi_tracks(&midi);

    let all_events: Vec<_> = tracks[0].iter().collect();
    let note_on = all_events.iter().find(|e| e.is_note_on);
    let note_off = all_events.iter().find(|e| !e.is_note_on && e.pitch == 60);

    assert!(note_on.is_some(), "Should have note on");
    assert!(note_off.is_some(), "Should have note off");

    // Duration should be 16 * 480 = 7680 ticks
    let duration = note_off.unwrap().tick - note_on.unwrap().tick;
    assert_eq!(
        duration, 7680,
        "Note duration should be 7680 ticks (16 beats)"
    );
}
