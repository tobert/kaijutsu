//! MIDI generation from ABC AST.
//!
//! Generates Standard MIDI File (SMF) format 0 (single track).

use std::collections::HashMap;

use crate::ast::{Accidental, Bar, Element, Key, Mode, NoteName, Tune, UnitLength, Voice};
use crate::MidiParams;

/// Get the combined pitch offset from voice properties (transpose + octave)
fn get_voice_pitch_offset(voice: &Voice, voice_defs: &[crate::ast::VoiceDef]) -> i16 {
    // Find the voice definition that matches this voice
    let voice_def = voice
        .id
        .as_ref()
        .and_then(|vid| voice_defs.iter().find(|vd| &vd.id == vid));

    let transpose_offset = voice_def
        .and_then(|vd| vd.transpose)
        .map(|t| t as i16)
        .unwrap_or(0);

    let octave_offset = voice_def
        .and_then(|vd| vd.octave)
        .map(|o| (o as i16) * 12)
        .unwrap_or(0);

    transpose_offset + octave_offset
}

/// Apply pitch offset and clamp to valid MIDI range
fn apply_pitch_offset(base_pitch: u8, offset: i16) -> u8 {
    ((base_pitch as i16) + offset).clamp(0, 127) as u8
}

/// Expand repeats in a voice's elements.
///
/// Handles `|:` ... `:|` simple repeats. First/second endings are passed through unchanged.
fn expand_repeats(elements: &[Element]) -> Vec<Element> {
    let mut result = Vec::new();
    let mut repeat_start_idx: Option<usize> = None;

    for element in elements {
        match element {
            Element::Bar(Bar::RepeatStart) => {
                // Mark start position, add the bar
                repeat_start_idx = Some(result.len());
                result.push(element.clone());
            }
            Element::Bar(Bar::RepeatEnd) => {
                // Add the end bar, then copy from repeat start
                result.push(element.clone());

                let start = repeat_start_idx.unwrap_or(0);
                // Skip the RepeatStart bar itself in the copy
                let copy_start = if repeat_start_idx.is_some() {
                    start + 1
                } else {
                    start
                };

                // Copy elements (excluding the RepeatEnd we just added)
                let to_copy: Vec<_> = result[copy_start..result.len() - 1].to_vec();
                result.extend(to_copy);

                // Reset repeat start (don't repeat again unless new |:)
                repeat_start_idx = None;
            }
            Element::Bar(Bar::RepeatBoth) => {
                // :: means end repeat then start new repeat
                // First, do the repeat
                result.push(Element::Bar(Bar::RepeatEnd));

                let start = repeat_start_idx.unwrap_or(0);
                let copy_start = if repeat_start_idx.is_some() {
                    start + 1
                } else {
                    start
                };
                let to_copy: Vec<_> = result[copy_start..result.len() - 1].to_vec();
                result.extend(to_copy);

                // Then mark new start
                repeat_start_idx = Some(result.len());
                result.push(Element::Bar(Bar::RepeatStart));
            }
            _ => {
                result.push(element.clone());
            }
        }
    }

    result
}

/// Generate MIDI bytes from a parsed ABC tune
pub fn generate(tune: &Tune, params: &MidiParams) -> Vec<u8> {
    // Count voices with actual content
    let voices_with_content: Vec<_> = tune
        .voices
        .iter()
        .filter(|v| !v.elements.is_empty())
        .collect();

    // If multiple voices, use format 1 (multi-track)
    if voices_with_content.len() > 1 {
        return generate_multitrack(tune, params);
    }

    // Single voice - use format 0
    let mut writer = MidiWriter::new(params.ticks_per_beat, params.channel);

    // Set tempo
    if let Some(tempo) = &tune.header.tempo {
        writer.tempo(tempo.bpm);
    } else {
        writer.tempo(120); // Default
    }

    // Set program: ABC %%MIDI program takes priority, then params.program
    let program = tune.header.midi_program.or(params.program);
    if let Some(program) = program {
        writer.program_change(program);
    }

    // Compute key signature accidentals
    let key_accidentals = compute_key_accidentals(&tune.header.key);

    // Compute ticks per unit note
    let unit_length = tune.header.unit_length.unwrap_or_default();
    let unit_ticks = compute_unit_ticks(&unit_length, params.ticks_per_beat);

    // Process all voices (merge into single track for format 0)
    for voice in &tune.voices {
        // Get pitch offset from voice properties (transpose, octave)
        let pitch_offset = get_voice_pitch_offset(voice, &tune.header.voice_defs);

        // Expand repeats before processing
        let elements = expand_repeats(&voice.elements);

        // Bar-scoped accidentals reset at each bar line
        let mut bar_accidentals = key_accidentals.clone();

        // Track held (tied) notes: midi_pitch -> accumulated ticks
        let mut held_notes: HashMap<u8, u32> = HashMap::new();

        for element in &elements {
            match element {
                Element::Note(note) => {
                    // Determine pitch with accidentals, then apply voice offset
                    let base_pitch = note_to_midi_pitch(
                        note.pitch,
                        note.octave,
                        note.accidental,
                        &bar_accidentals,
                    );
                    let midi_pitch = apply_pitch_offset(base_pitch, pitch_offset);
                    let ticks = note.duration.to_ticks(unit_ticks);

                    if let Some(held_ticks) = held_notes.remove(&midi_pitch) {
                        // Continue a tied note - add duration, advance time
                        writer.advance(ticks);
                        if note.tie {
                            // Still tied, keep accumulating
                            held_notes.insert(midi_pitch, held_ticks + ticks);
                        } else {
                            // Tie ends here - emit note off
                            writer.note_off(midi_pitch);
                        }
                    } else if note.tie {
                        // Start a new tied note
                        writer.note_on(midi_pitch, params.velocity);
                        writer.advance(ticks);
                        held_notes.insert(midi_pitch, ticks);
                    } else {
                        // Regular note, emit immediately
                        writer.note(midi_pitch, params.velocity, ticks);
                    }

                    // Update bar accidentals if note has explicit accidental
                    if let Some(acc) = note.accidental {
                        bar_accidentals.insert(note.pitch, acc);
                    }
                }

                Element::Chord(chord) => {
                    let ticks = chord.duration.to_ticks(unit_ticks);

                    // Note on for all notes
                    for note in &chord.notes {
                        let base_pitch = note_to_midi_pitch(
                            note.pitch,
                            note.octave,
                            note.accidental,
                            &bar_accidentals,
                        );
                        let midi_pitch = apply_pitch_offset(base_pitch, pitch_offset);
                        writer.note_on(midi_pitch, params.velocity);

                        if let Some(acc) = note.accidental {
                            bar_accidentals.insert(note.pitch, acc);
                        }
                    }

                    // Advance time
                    writer.advance(ticks);

                    // Note off for all notes
                    for note in &chord.notes {
                        let base_pitch = note_to_midi_pitch(
                            note.pitch,
                            note.octave,
                            note.accidental,
                            &bar_accidentals,
                        );
                        let midi_pitch = apply_pitch_offset(base_pitch, pitch_offset);
                        writer.note_off(midi_pitch);
                    }
                }

                Element::Rest(rest) => {
                    if let Some(bars) = rest.multi_measure {
                        // Multi-measure rest - advance by bars * beats per bar
                        let (beats_per_bar, _) = tune
                            .header
                            .meter
                            .as_ref()
                            .map(|m| m.to_fraction())
                            .unwrap_or((4, 4));
                        let ticks_per_bar = params.ticks_per_beat as u32 * beats_per_bar as u32;
                        writer.advance(ticks_per_bar * bars as u32);
                    } else {
                        let ticks = rest.duration.to_ticks(unit_ticks);
                        writer.advance(ticks);
                    }
                }

                Element::Bar(_) => {
                    // Reset bar accidentals
                    bar_accidentals = key_accidentals.clone();
                }

                Element::Tuplet(tuplet) => {
                    // Scale durations by q/p
                    let scale_num = tuplet.q as u32;
                    let scale_den = tuplet.p as u32;

                    for elem in &tuplet.elements {
                        if let Element::Note(note) = elem {
                            let base_pitch = note_to_midi_pitch(
                                note.pitch,
                                note.octave,
                                note.accidental,
                                &bar_accidentals,
                            );
                            let midi_pitch = apply_pitch_offset(base_pitch, pitch_offset);
                            let base_ticks = note.duration.to_ticks(unit_ticks);
                            let ticks = (base_ticks * scale_num) / scale_den;

                            writer.note(midi_pitch, params.velocity, ticks);

                            if let Some(acc) = note.accidental {
                                bar_accidentals.insert(note.pitch, acc);
                            }
                        }
                    }
                }

                // Decorations, slurs, etc. - ignored in MVP MIDI output
                _ => {}
            }
        }

        // Flush any remaining held notes at end of voice
        for (midi_pitch, _ticks) in held_notes.drain() {
            writer.note_off(midi_pitch);
        }
    }

    writer.finish()
}

/// Generate multi-track MIDI (SMF format 1) for tunes with multiple voices.
fn generate_multitrack(tune: &Tune, params: &MidiParams) -> Vec<u8> {
    let key_accidentals = compute_key_accidentals(&tune.header.key);
    let unit_length = tune.header.unit_length.unwrap_or_default();
    let unit_ticks = compute_unit_ticks(&unit_length, params.ticks_per_beat);

    let mut tracks: Vec<Vec<u8>> = Vec::new();

    // Track 0: Tempo track (meta events only)
    let mut tempo_writer = MidiWriter::new(params.ticks_per_beat, 0);
    if let Some(tempo) = &tune.header.tempo {
        tempo_writer.tempo(tempo.bpm);
    } else {
        tempo_writer.tempo(120);
    }
    tracks.push(tempo_writer.encode_track());

    // One track per voice
    for (voice_idx, voice) in tune.voices.iter().enumerate() {
        if voice.elements.is_empty() {
            continue;
        }

        // Get pitch offset from voice properties (transpose, octave)
        let pitch_offset = get_voice_pitch_offset(voice, &tune.header.voice_defs);

        // Use different MIDI channel per voice (0-15, skip 9 which is percussion)
        let channel = if voice_idx >= 9 {
            voice_idx + 1
        } else {
            voice_idx
        } as u8
            % 16;
        let mut writer = MidiWriter::new(params.ticks_per_beat, channel);

        // Set program: ABC %%MIDI program takes priority, then params.program
        let program = tune.header.midi_program.or(params.program);
        if let Some(program) = program {
            writer.program_change_channel(program, channel);
        }

        let elements = expand_repeats(&voice.elements);
        let mut bar_accidentals = key_accidentals.clone();
        let mut held_notes: HashMap<u8, u32> = HashMap::new();

        for element in &elements {
            match element {
                Element::Note(note) => {
                    let base_pitch = note_to_midi_pitch(
                        note.pitch,
                        note.octave,
                        note.accidental,
                        &bar_accidentals,
                    );
                    let midi_pitch = apply_pitch_offset(base_pitch, pitch_offset);
                    let ticks = note.duration.to_ticks(unit_ticks);

                    if let Some(held_ticks) = held_notes.remove(&midi_pitch) {
                        writer.advance(ticks);
                        if note.tie {
                            held_notes.insert(midi_pitch, held_ticks + ticks);
                        } else {
                            writer.note_off_channel(midi_pitch, channel);
                        }
                    } else if note.tie {
                        writer.note_on_channel(midi_pitch, params.velocity, channel);
                        writer.advance(ticks);
                        held_notes.insert(midi_pitch, ticks);
                    } else {
                        writer.note_channel(midi_pitch, params.velocity, ticks, channel);
                    }

                    if let Some(acc) = note.accidental {
                        bar_accidentals.insert(note.pitch, acc);
                    }
                }

                Element::Chord(chord) => {
                    let ticks = chord.duration.to_ticks(unit_ticks);
                    for note in &chord.notes {
                        let base_pitch = note_to_midi_pitch(
                            note.pitch,
                            note.octave,
                            note.accidental,
                            &bar_accidentals,
                        );
                        let midi_pitch = apply_pitch_offset(base_pitch, pitch_offset);
                        writer.note_on_channel(midi_pitch, params.velocity, channel);
                        if let Some(acc) = note.accidental {
                            bar_accidentals.insert(note.pitch, acc);
                        }
                    }
                    writer.advance(ticks);
                    for note in &chord.notes {
                        let base_pitch = note_to_midi_pitch(
                            note.pitch,
                            note.octave,
                            note.accidental,
                            &bar_accidentals,
                        );
                        let midi_pitch = apply_pitch_offset(base_pitch, pitch_offset);
                        writer.note_off_channel(midi_pitch, channel);
                    }
                }

                Element::Rest(rest) => {
                    if let Some(bars) = rest.multi_measure {
                        let (beats_per_bar, _) = tune
                            .header
                            .meter
                            .as_ref()
                            .map(|m| m.to_fraction())
                            .unwrap_or((4, 4));
                        let ticks_per_bar = params.ticks_per_beat as u32 * beats_per_bar as u32;
                        writer.advance(ticks_per_bar * bars as u32);
                    } else {
                        writer.advance(rest.duration.to_ticks(unit_ticks));
                    }
                }

                Element::Bar(_) => {
                    bar_accidentals = key_accidentals.clone();
                }

                Element::Tuplet(tuplet) => {
                    let scale_num = tuplet.q as u32;
                    let scale_den = tuplet.p as u32;
                    for elem in &tuplet.elements {
                        if let Element::Note(note) = elem {
                            let base_pitch = note_to_midi_pitch(
                                note.pitch,
                                note.octave,
                                note.accidental,
                                &bar_accidentals,
                            );
                            let midi_pitch = apply_pitch_offset(base_pitch, pitch_offset);
                            let base_ticks = note.duration.to_ticks(unit_ticks);
                            let ticks = (base_ticks * scale_num) / scale_den;
                            writer.note_channel(midi_pitch, params.velocity, ticks, channel);
                            if let Some(acc) = note.accidental {
                                bar_accidentals.insert(note.pitch, acc);
                            }
                        }
                    }
                }

                _ => {}
            }
        }

        // Flush held notes
        for (midi_pitch, _) in held_notes.drain() {
            writer.note_off_channel(midi_pitch, channel);
        }

        tracks.push(writer.encode_track());
    }

    // Build multi-track MIDI file
    let mut out = Vec::new();

    // Header chunk
    out.extend_from_slice(b"MThd");
    out.extend_from_slice(&6u32.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes()); // Format 1
    out.extend_from_slice(&(tracks.len() as u16).to_be_bytes());
    out.extend_from_slice(&params.ticks_per_beat.to_be_bytes());

    // Track chunks
    for track_data in tracks {
        out.extend_from_slice(b"MTrk");
        out.extend_from_slice(&(track_data.len() as u32).to_be_bytes());
        out.extend(track_data);
    }

    out
}

/// Convert note to MIDI pitch, applying accidentals from context
fn note_to_midi_pitch(
    pitch: NoteName,
    octave: i8,
    note_accidental: Option<Accidental>,
    bar_accidentals: &HashMap<NoteName, Accidental>,
) -> u8 {
    let base = pitch.to_semitone();

    // ABC octave 0 (uppercase C-B) = MIDI 60-71 (middle C octave)
    // ABC octave 1 (lowercase c-b) = MIDI 72-83
    let octave_offset = (octave + 5) * 12;

    // Priority: note's own accidental > bar context > key signature
    let acc_offset = note_accidental
        .or_else(|| bar_accidentals.get(&pitch).copied())
        .map(|a| a.to_semitone_offset())
        .unwrap_or(0);

    ((base as i16) + (octave_offset as i16) + (acc_offset as i16)).clamp(0, 127) as u8
}

/// Compute accidentals from key signature
fn compute_key_accidentals(key: &Key) -> HashMap<NoteName, Accidental> {
    let mut accidentals = HashMap::new();

    // Circle of fifths
    let sharps = [
        NoteName::F,
        NoteName::C,
        NoteName::G,
        NoteName::D,
        NoteName::A,
        NoteName::E,
        NoteName::B,
    ];
    let flats = [
        NoteName::B,
        NoteName::E,
        NoteName::A,
        NoteName::D,
        NoteName::G,
        NoteName::C,
        NoteName::F,
    ];

    // Determine number of sharps/flats based on key
    let (count, is_sharp) = key_signature_accidentals(key);

    let affected = if is_sharp { &sharps } else { &flats };
    for i in 0..count as usize {
        if i < affected.len() {
            accidentals.insert(
                affected[i],
                if is_sharp {
                    Accidental::Sharp
                } else {
                    Accidental::Flat
                },
            );
        }
    }

    accidentals
}

/// Get number of sharps (positive) or flats (negative) for a key
fn key_signature_accidentals(key: &Key) -> (i8, bool) {
    // Base sharps/flats for major keys
    let base = match (&key.root, &key.accidental) {
        (NoteName::C, None) => 0,
        (NoteName::G, None) => 1,
        (NoteName::D, None) => 2,
        (NoteName::A, None) => 3,
        (NoteName::E, None) => 4,
        (NoteName::B, None) => 5,
        (NoteName::F, Some(Accidental::Sharp)) => 6,
        (NoteName::C, Some(Accidental::Sharp)) => 7,
        (NoteName::F, None) => -1,
        (NoteName::B, Some(Accidental::Flat)) => -2,
        (NoteName::E, Some(Accidental::Flat)) => -3,
        (NoteName::A, Some(Accidental::Flat)) => -4,
        (NoteName::D, Some(Accidental::Flat)) => -5,
        (NoteName::G, Some(Accidental::Flat)) => -6,
        (NoteName::C, Some(Accidental::Flat)) => -7,
        _ => 0,
    };

    // Adjust for mode
    let mode_offset = match key.mode {
        Mode::Major | Mode::Ionian => 0,
        Mode::Minor | Mode::Aeolian => -3, // Relative minor is 3 flats less
        Mode::Dorian => -2,
        Mode::Phrygian => -4,
        Mode::Lydian => 1,
        Mode::Mixolydian => -1,
        Mode::Locrian => -5,
    };

    let total = base + mode_offset;
    if total >= 0 {
        (total, true)
    } else {
        (-total, false)
    }
}

/// Compute MIDI ticks per ABC unit note
fn compute_unit_ticks(unit_length: &UnitLength, ticks_per_beat: u16) -> u32 {
    // Unit length is relative to a whole note
    // ticks_per_beat is ticks per quarter note
    // So ticks per whole note = ticks_per_beat * 4
    let ticks_per_whole = ticks_per_beat as u32 * 4;
    (ticks_per_whole * unit_length.numerator as u32) / unit_length.denominator as u32
}

/// MIDI file writer
struct MidiWriter {
    ticks_per_beat: u16,
    channel: u8,
    events: Vec<MidiEvent>,
    current_tick: u32,
}

struct MidiEvent {
    tick: u32,
    data: Vec<u8>,
}

impl MidiWriter {
    fn new(ticks_per_beat: u16, channel: u8) -> Self {
        MidiWriter {
            ticks_per_beat,
            channel: channel & 0x0F,
            events: Vec::new(),
            current_tick: 0,
        }
    }

    fn tempo(&mut self, bpm: u16) {
        let us_per_beat = 60_000_000u32 / bpm as u32;
        self.meta_event(
            0x51,
            vec![
                ((us_per_beat >> 16) & 0xFF) as u8,
                ((us_per_beat >> 8) & 0xFF) as u8,
                (us_per_beat & 0xFF) as u8,
            ],
        );
    }

    fn note_on(&mut self, pitch: u8, velocity: u8) {
        self.channel_event(vec![0x90 | self.channel, pitch, velocity]);
    }

    fn note_off(&mut self, pitch: u8) {
        self.channel_event(vec![0x80 | self.channel, pitch, 0]);
    }

    fn program_change(&mut self, program: u8) {
        self.channel_event(vec![0xC0 | self.channel, program & 0x7F]);
    }

    fn program_change_channel(&mut self, program: u8, channel: u8) {
        self.channel_event(vec![0xC0 | (channel & 0x0F), program & 0x7F]);
    }

    fn note(&mut self, pitch: u8, velocity: u8, duration: u32) {
        self.note_on(pitch, velocity);
        self.advance(duration);
        self.note_off(pitch);
    }

    // Channel-specific versions for multi-track MIDI
    fn note_on_channel(&mut self, pitch: u8, velocity: u8, channel: u8) {
        self.channel_event(vec![0x90 | (channel & 0x0F), pitch, velocity]);
    }

    fn note_off_channel(&mut self, pitch: u8, channel: u8) {
        self.channel_event(vec![0x80 | (channel & 0x0F), pitch, 0]);
    }

    fn note_channel(&mut self, pitch: u8, velocity: u8, duration: u32, channel: u8) {
        self.note_on_channel(pitch, velocity, channel);
        self.advance(duration);
        self.note_off_channel(pitch, channel);
    }

    fn advance(&mut self, ticks: u32) {
        self.current_tick += ticks;
    }

    fn meta_event(&mut self, event_type: u8, data: Vec<u8>) {
        let mut event_data = vec![0xFF, event_type];
        // Length as variable-length quantity (for small lengths, just the byte)
        event_data.extend(encode_variable_length(data.len() as u32));
        event_data.extend(data);
        self.events.push(MidiEvent {
            tick: self.current_tick,
            data: event_data,
        });
    }

    fn channel_event(&mut self, data: Vec<u8>) {
        self.events.push(MidiEvent {
            tick: self.current_tick,
            data,
        });
    }

    fn finish(mut self) -> Vec<u8> {
        // Sort events by tick
        self.events.sort_by_key(|e| e.tick);

        // Encode track data
        let track_data = self.encode_track();

        // Build complete MIDI file
        let mut out = Vec::new();

        // Header chunk: MThd
        out.extend_from_slice(b"MThd");
        out.extend_from_slice(&6u32.to_be_bytes()); // chunk length
        out.extend_from_slice(&0u16.to_be_bytes()); // format 0
        out.extend_from_slice(&1u16.to_be_bytes()); // 1 track
        out.extend_from_slice(&self.ticks_per_beat.to_be_bytes());

        // Track chunk: MTrk
        out.extend_from_slice(b"MTrk");
        out.extend_from_slice(&(track_data.len() as u32).to_be_bytes());
        out.extend(track_data);

        out
    }

    fn encode_track(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut last_tick = 0u32;

        for event in &self.events {
            let delta = event.tick.saturating_sub(last_tick);
            out.extend(encode_variable_length(delta));
            out.extend(&event.data);
            last_tick = event.tick;
        }

        // End of track
        out.extend(&[0x00, 0xFF, 0x2F, 0x00]);
        out
    }
}

/// Encode a value as MIDI variable-length quantity
fn encode_variable_length(mut value: u32) -> Vec<u8> {
    if value == 0 {
        return vec![0];
    }

    let mut bytes = Vec::new();
    bytes.push((value & 0x7F) as u8);
    value >>= 7;

    while value > 0 {
        bytes.push(((value & 0x7F) | 0x80) as u8);
        value >>= 7;
    }

    bytes.reverse();
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Duration, Header, Meter, Note, Rest, Tempo, Voice};

    #[test]
    fn test_variable_length_encoding() {
        assert_eq!(encode_variable_length(0), vec![0x00]);
        assert_eq!(encode_variable_length(127), vec![0x7F]);
        assert_eq!(encode_variable_length(128), vec![0x81, 0x00]);
        assert_eq!(encode_variable_length(16383), vec![0xFF, 0x7F]);
        assert_eq!(encode_variable_length(16384), vec![0x81, 0x80, 0x00]);
    }

    #[test]
    fn test_compute_unit_ticks() {
        // L:1/4 with 480 ticks/beat = 480 ticks per unit (quarter note)
        assert_eq!(
            compute_unit_ticks(
                &UnitLength {
                    numerator: 1,
                    denominator: 4
                },
                480
            ),
            480
        );

        // L:1/8 with 480 ticks/beat = 240 ticks per unit (eighth note)
        assert_eq!(
            compute_unit_ticks(
                &UnitLength {
                    numerator: 1,
                    denominator: 8
                },
                480
            ),
            240
        );

        // L:1/16 with 480 ticks/beat = 120 ticks per unit
        assert_eq!(
            compute_unit_ticks(
                &UnitLength {
                    numerator: 1,
                    denominator: 16
                },
                480
            ),
            120
        );
    }

    #[test]
    fn test_key_accidentals_c_major() {
        let key = Key::default(); // C major
        let acc = compute_key_accidentals(&key);
        assert!(acc.is_empty()); // No sharps or flats
    }

    #[test]
    fn test_key_accidentals_g_major() {
        let key = Key {
            root: NoteName::G,
            accidental: None,
            mode: Mode::Major,
            explicit_accidentals: vec![],
            clef: None,
        };
        let acc = compute_key_accidentals(&key);
        assert_eq!(acc.get(&NoteName::F), Some(&Accidental::Sharp));
        assert_eq!(acc.len(), 1);
    }

    #[test]
    fn test_key_accidentals_f_major() {
        let key = Key {
            root: NoteName::F,
            accidental: None,
            mode: Mode::Major,
            explicit_accidentals: vec![],
            clef: None,
        };
        let acc = compute_key_accidentals(&key);
        assert_eq!(acc.get(&NoteName::B), Some(&Accidental::Flat));
        assert_eq!(acc.len(), 1);
    }

    #[test]
    fn test_key_accidentals_d_dorian() {
        // D dorian = same as C major (no sharps/flats)
        let key = Key {
            root: NoteName::D,
            accidental: None,
            mode: Mode::Dorian,
            explicit_accidentals: vec![],
            clef: None,
        };
        let acc = compute_key_accidentals(&key);
        assert!(acc.is_empty());
    }

    #[test]
    fn test_generate_simple_tune() {
        let tune = Tune {
            header: Header {
                reference: 1,
                title: "Test".to_string(),
                titles: vec![],
                key: Key::default(),
                meter: Some(Meter::Simple {
                    numerator: 4,
                    denominator: 4,
                }),
                unit_length: Some(UnitLength {
                    numerator: 1,
                    denominator: 8,
                }),
                tempo: Some(Tempo {
                    beat_unit: (1, 4),
                    bpm: 120,
                    text: None,
                }),
                ..Header::default()
            },
            voices: vec![Voice {
                id: None,
                name: None,
                elements: vec![
                    Element::Note(Note::new(NoteName::C, 1)), // middle C
                    Element::Note(Note::new(NoteName::D, 1)),
                    Element::Note(Note::new(NoteName::E, 1)),
                    Element::Note(Note::new(NoteName::F, 1)),
                ],
            }],
        };

        let midi = generate(&tune, &MidiParams::default());

        // Check MIDI header
        assert_eq!(&midi[0..4], b"MThd");
        assert_eq!(&midi[8..10], &[0, 0]); // format 0
        assert_eq!(&midi[10..12], &[0, 1]); // 1 track
    }

    #[test]
    fn test_generate_with_rest() {
        let tune = Tune {
            header: Header {
                reference: 1,
                title: "Test".to_string(),
                titles: vec![],
                key: Key::default(),
                meter: Some(Meter::Simple {
                    numerator: 4,
                    denominator: 4,
                }),
                unit_length: Some(UnitLength {
                    numerator: 1,
                    denominator: 4,
                }),
                tempo: Some(Tempo {
                    beat_unit: (1, 4),
                    bpm: 120,
                    text: None,
                }),
                ..Header::default()
            },
            voices: vec![Voice {
                id: None,
                name: None,
                elements: vec![
                    Element::Note(Note::new(NoteName::C, 1)),
                    Element::Rest(Rest::new(Duration::unit())),
                    Element::Note(Note::new(NoteName::E, 1)),
                ],
            }],
        };

        let midi = generate(&tune, &MidiParams::default());

        // Should have valid MIDI header
        assert_eq!(&midi[0..4], b"MThd");
    }

    #[test]
    fn test_roundtrip_parse_generate() {
        let abc = "X:1\nT:Test\nM:4/4\nL:1/4\nQ:1/4=120\nK:C\nCDEF|\n";
        let result = crate::parse(abc);
        assert!(!result.has_errors());

        let midi = generate(&result.value, &MidiParams::default());

        // Should produce valid MIDI
        assert_eq!(&midi[0..4], b"MThd");
        assert!(midi.len() > 20); // Should have some content
    }

    #[test]
    fn test_tie_handling() {
        // c-c should produce one long note, not two separate notes
        let abc = "X:1\nT:Test\nM:4/4\nL:1/4\nK:C\nc-c|\n";
        let result = crate::parse(abc);
        assert!(!result.has_errors());

        let midi = generate(&result.value, &MidiParams::default());

        // Count note-on events (0x90) - should be exactly 1 for the tied note
        let note_ons = midi
            .windows(2)
            .filter(|w| w[0] == 0x90 && w[1] == 72) // 0x90 = note on, 72 = c (C5)
            .count();
        assert_eq!(note_ons, 1, "Tied notes should produce single note-on");
    }

    #[test]
    fn test_tie_across_bar() {
        // Tie across bar line should work
        let abc = "X:1\nT:Test\nM:4/4\nL:1/4\nK:C\nc-|c|\n";
        let result = crate::parse(abc);
        assert!(!result.has_errors());

        let midi = generate(&result.value, &MidiParams::default());

        // Should still be one note
        let note_ons = midi
            .windows(2)
            .filter(|w| w[0] == 0x90 && w[1] == 72)
            .count();
        assert_eq!(note_ons, 1, "Tie across bar should produce single note-on");
    }

    #[test]
    fn test_repeat_expansion() {
        // |: c d :| should produce c d c d (lowercase c = MIDI 72, d = MIDI 74)
        let abc = "X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n|:cd:|\n";
        let result = crate::parse(abc);
        assert!(!result.has_errors(), "Parse errors: {:?}", result.feedback);

        let midi = generate(&result.value, &MidiParams::default());

        // Count c notes (midi 72) - should be 2 (once per repeat)
        let c_notes = midi
            .windows(2)
            .filter(|w| w[0] == 0x90 && w[1] == 72)
            .count();
        assert_eq!(c_notes, 2, "Repeat should double the notes");

        // Count d notes (midi 74)
        let d_notes = midi
            .windows(2)
            .filter(|w| w[0] == 0x90 && w[1] == 74)
            .count();
        assert_eq!(d_notes, 2, "Repeat should double the notes");
    }

    #[test]
    fn test_expand_repeats_function() {
        use crate::ast::{Bar, Note};

        let elements = vec![
            Element::Bar(Bar::RepeatStart),
            Element::Note(Note::new(NoteName::C, 1)),
            Element::Note(Note::new(NoteName::D, 1)),
            Element::Bar(Bar::RepeatEnd),
        ];

        let expanded = expand_repeats(&elements);

        // Should have: |: C D :| C D
        // That's: RepeatStart, C, D, RepeatEnd, C, D = 6 elements
        assert_eq!(expanded.len(), 6);

        // Check structure
        assert!(matches!(expanded[0], Element::Bar(Bar::RepeatStart)));
        assert!(matches!(expanded[3], Element::Bar(Bar::RepeatEnd)));
        // Elements 4 and 5 should be copies of C and D
        if let Element::Note(n) = &expanded[4] {
            assert_eq!(n.pitch, NoteName::C);
        } else {
            panic!("Expected note");
        }
    }

    #[test]
    fn test_midi_channel_parameter() {
        // Channel 9 is GM drums - verify we emit events on the specified channel
        // Using lowercase c = MIDI 72 (C5)
        let abc = "X:1\nT:Test\nM:4/4\nL:1/4\nK:C\ncde|\n";
        let result = crate::parse(abc);
        assert!(!result.has_errors());

        // Channel 0 (default)
        let midi_ch0 = generate(&result.value, &MidiParams::default());
        // Look for note-on: 0x90 = channel 0 note-on, 72 = c (C5)
        let has_ch0 = midi_ch0.windows(2).any(|w| w[0] == 0x90 && w[1] == 72);
        assert!(has_ch0, "Should have note-on on channel 0");

        // Channel 9 (drums)
        let params_ch9 = MidiParams {
            velocity: 80,
            ticks_per_beat: 480,
            channel: 9,
            program: None,
        };
        let midi_ch9 = generate(&result.value, &params_ch9);
        // Look for note-on: 0x99 = channel 9 note-on
        let has_ch9 = midi_ch9.windows(2).any(|w| w[0] == 0x99 && w[1] == 72);
        assert!(has_ch9, "Should have note-on on channel 9");

        // Verify no channel 0 events when using channel 9
        let has_ch0_in_ch9 = midi_ch9.windows(2).any(|w| w[0] == 0x90 && w[1] == 72);
        assert!(
            !has_ch0_in_ch9,
            "Should not have channel 0 events when using channel 9"
        );
    }

    #[test]
    fn test_midi_program_from_abc() {
        // Test that %%MIDI program directive results in program change event
        let abc = "X:1\nT:Test\n%%MIDI program 33\nM:4/4\nL:1/4\nK:C\ncde|\n";
        let result = crate::parse(abc);
        assert!(!result.has_errors());
        assert_eq!(result.value.header.midi_program, Some(33));

        let midi = generate(&result.value, &MidiParams::default());

        // Look for program change: 0xC0 = channel 0 program change, 33 = program
        let has_program_change = midi.windows(2).any(|w| w[0] == 0xC0 && w[1] == 33);
        assert!(has_program_change, "Should have program change to 33");
    }

    #[test]
    fn test_midi_program_from_params() {
        // Test that params.program works when ABC doesn't have %%MIDI program
        let abc = "X:1\nT:Test\nM:4/4\nL:1/4\nK:C\ncde|\n";
        let result = crate::parse(abc);
        assert!(!result.has_errors());
        assert_eq!(result.value.header.midi_program, None);

        let params = MidiParams {
            velocity: 80,
            ticks_per_beat: 480,
            channel: 0,
            program: Some(56), // Trumpet
        };
        let midi = generate(&result.value, &params);

        // Look for program change: 0xC0 = channel 0 program change, 56 = trumpet
        let has_program_change = midi.windows(2).any(|w| w[0] == 0xC0 && w[1] == 56);
        assert!(has_program_change, "Should have program change to 56");
    }

    #[test]
    fn test_abc_program_overrides_params() {
        // Test that ABC %%MIDI program takes priority over params.program
        let abc = "X:1\nT:Test\n%%MIDI program 52\nM:4/4\nL:1/4\nK:C\ncde|\n";
        let result = crate::parse(abc);

        let params = MidiParams {
            velocity: 80,
            ticks_per_beat: 480,
            channel: 0,
            program: Some(0), // Piano - but ABC says 52
        };
        let midi = generate(&result.value, &params);

        // Should use ABC's program 52, not params' program 0
        let has_program_52 = midi.windows(2).any(|w| w[0] == 0xC0 && w[1] == 52);
        assert!(has_program_52, "ABC program should override params");
    }

    #[test]
    fn test_midi_program_in_header_before_key() {
        // %%MIDI program MUST come before K: field (in header section)
        // This is the correct placement
        let abc = "X:1\nT:Test\n%%MIDI program 56\nM:4/4\nL:1/4\nK:C\nCDEF|\n";
        let result = crate::parse(abc);
        assert!(!result.has_errors());
        assert_eq!(
            result.value.header.midi_program,
            Some(56),
            "%%MIDI program before K: should be parsed"
        );

        let midi = generate(&result.value, &MidiParams::default());
        let has_program_change = midi.windows(2).any(|w| w[0] == 0xC0 && w[1] == 56);
        assert!(has_program_change, "Should emit program change 56");
    }

    #[test]
    fn test_midi_program_after_key_not_parsed() {
        // %%MIDI program after K: field is in the body, not header
        // Currently this is NOT parsed (header parsing stops at K:)
        // This test documents the current behavior
        let abc = "X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n%%MIDI program 56\nCDEF|\n";
        let result = crate::parse(abc);
        assert!(!result.has_errors());
        // Currently NOT parsed because it's after K:
        assert_eq!(
            result.value.header.midi_program,
            None,
            "%%MIDI program after K: is not parsed (in body, not header)"
        );
    }

    #[test]
    fn test_midi_program_various_positions() {
        // Test %%MIDI program works in various valid header positions
        let cases = [
            ("X:1\n%%MIDI program 40\nT:Test\nM:4/4\nK:C\nC|\n", Some(40), "after X:"),
            ("X:1\nT:Test\n%%MIDI program 41\nM:4/4\nK:C\nC|\n", Some(41), "after T:"),
            ("X:1\nT:Test\nM:4/4\n%%MIDI program 42\nL:1/4\nK:C\nC|\n", Some(42), "after M:"),
            ("X:1\nT:Test\nM:4/4\nL:1/4\n%%MIDI program 43\nK:C\nC|\n", Some(43), "before K:"),
            ("X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n%%MIDI program 44\nC|\n", None, "after K: (body)"),
        ];

        for (abc, expected_program, position) in cases {
            let result = crate::parse(abc);
            assert!(!result.has_errors(), "Parse failed for {}", position);
            assert_eq!(
                result.value.header.midi_program, expected_program,
                "Wrong program for %%MIDI {}", position
            );
        }
    }
}
