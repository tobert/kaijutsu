//! MIDI generation from ABC AST.
//!
//! Generates Standard MIDI File (SMF) format 0 (single track).

use std::collections::HashMap;

use crate::ast::{Accidental, Bar, Element, Key, Mode, Note, NoteName, Tune, UnitLength, Voice};
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

/// Playback pitch offset from a `K:` field's `transpose=`/`octave=` attributes
/// (ABC v2.1 §4.6). Distinct from, and added to, any `V:`-level offset.
fn key_pitch_offset(key: &Key) -> i16 {
    key.transpose as i16 + (key.octave as i16) * 12
}

/// Play a run of gracenotes just before their principal note, stealing their
/// time from it so the beat grid is preserved (ABC v2.1 §4.20). Each grace gets
/// ~1/8 of a unit note, but the run is capped at half the principal's duration.
/// Returns the ticks stolen (to subtract from the principal). `channel` lets the
/// single-track and per-voice paths share this.
#[allow(clippy::too_many_arguments)]
fn play_grace_run(
    writer: &mut MidiWriter,
    graces: &[Note],
    main_ticks: u32,
    unit_ticks: u32,
    bar_accidentals: &HashMap<NoteName, Accidental>,
    pitch_offset: i16,
    velocity: u8,
    channel: u8,
) -> u32 {
    let count = graces.len() as u32;
    if count == 0 {
        return 0;
    }
    let total = (unit_ticks / 8).max(1).saturating_mul(count).min(main_ticks / 2);
    let each = (total / count).max(1);
    for g in graces {
        let acc = effective_accidental(g.pitch, g.accidental, bar_accidentals, None);
        let pitch = apply_pitch_offset(midi_pitch_with_accidental(g.pitch, g.octave, acc), pitch_offset);
        writer.note_channel(pitch, velocity, each, channel);
    }
    each * count
}

/// Flatten `Element::SlurGroup` nesting away. Slurs have no MIDI
/// semantics; their inner notes need to be heard as if the brackets
/// weren't there.
fn flatten_slurs(elements: &[Element]) -> Vec<Element> {
    let mut out = Vec::with_capacity(elements.len());
    for e in elements {
        match e {
            Element::SlurGroup { elements: inner } => {
                out.extend(flatten_slurs(inner));
            }
            other => out.push(other.clone()),
        }
    }
    out
}

/// True for a bar that opens a variant ending (`|2`, `:|2`, `[2`, `[3-4`, …).
fn is_ending_marker(e: &Element) -> bool {
    matches!(
        e,
        Element::Bar(Bar::SecondEnding) | Element::Bar(Bar::NthEnding(_))
    )
}

/// Expand a first/second variant-ending repeat into a flat linear stream:
/// `|: common |1 first :|2 second |` becomes `common first | common second`.
/// ABC v2.1 §4.9-4.10. Returns `None` when no first-ending marker is present
/// (so the caller falls back to simple repeat expansion). Handles a single
/// ended repeat section; the `:|2` and `|1 … :| [2 …` shapes both work.
fn expand_variant_endings(elements: &[Element]) -> Option<Vec<Element>> {
    let fe = elements
        .iter()
        .position(|e| matches!(e, Element::Bar(Bar::FirstEnding)))?;
    // Optional repeat-start before the first ending; common body follows it.
    let common_start = elements[..fe]
        .iter()
        .rposition(|e| matches!(e, Element::Bar(Bar::RepeatStart)))
        .map(|i| i + 1)
        .unwrap_or(0);

    // Pass-1 terminator after the first ending: a repeat-end or an ending marker.
    let back = (fe + 1..elements.len()).find(|&i| {
        matches!(elements[i], Element::Bar(Bar::RepeatEnd)) || is_ending_marker(&elements[i])
    })?;
    // The second ending opens at `back` (when `:|2`/`[2`) or just after a bare `:|`.
    let se = if is_ending_marker(&elements[back]) {
        back
    } else {
        (back + 1..elements.len()).find(|&i| is_ending_marker(&elements[i]))?
    };
    // The second ending runs until the next repeat section (or end of voice).
    let second_end = (se + 1..elements.len())
        .find(|&i| matches!(elements[i], Element::Bar(Bar::RepeatStart)))
        .unwrap_or(elements.len());

    let common = &elements[common_start..fe];
    let first = &elements[fe + 1..back];
    let second = &elements[se + 1..second_end];

    let mut out = Vec::new();
    out.extend_from_slice(&elements[..common_start]); // prefix incl. RepeatStart
    out.extend_from_slice(common);
    out.push(Element::Bar(Bar::Single));
    out.extend_from_slice(first);
    out.push(Element::Bar(Bar::Single)); // repeat back to common
    out.extend_from_slice(common);
    out.push(Element::Bar(Bar::Single));
    out.extend_from_slice(second);
    out.extend_from_slice(&elements[second_end..]); // tail (unrepeated)
    Some(out)
}

/// Expand repeats in a voice's elements.
///
/// Handles `|:` ... `:|` simple repeats and first/second variant endings
/// (`|1 … :|2 …`); falls back to the simple form when no ending is present.
fn expand_repeats(elements: &[Element]) -> Vec<Element> {
    // Variant endings need bespoke handling (pick the ending per pass).
    if let Some(expanded) = expand_variant_endings(elements) {
        return expanded;
    }

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

    // Single voice - use format 0: frame the shared writer's events as an SMF blob.
    build_single_track_writer(tune, params).finish()
}

/// The timed MIDI event stream for a tune — the per-event view the SMF blob from
/// [`generate`] frames. Stage 3 WI 5 (docs/tracks.md): the render target consumes
/// this to schedule individual NoteOn/NoteOff into an ALSA seq queue, rather than
/// re-parse the SMF byte blob. Events are absolute-tick and **sorted**; meta
/// events (tempo `0xFF 0x51`, etc.) are included for the consumer to honour or
/// skip — an ALSA seq target plays only the channel-voice (`0x80`/`0x90`/…)
/// messages. M1 renders the single-track (format-0) merge: a multi-voice tune
/// collapses onto one channel here (per-voice channel split is a later milestone,
/// matching `generate`'s own format-0 path).
pub fn events(tune: &Tune, params: &MidiParams) -> Vec<MidiEvent> {
    let mut writer = build_single_track_writer(tune, params);
    writer.events.sort_by_key(|e| e.tick);
    writer.events
}

/// Build the single-track (SMF format-0) writer for a tune — all voices merged
/// onto `params.channel`. Shared by [`generate`] (frames it as an SMF blob) and
/// [`events`] (returns its timed event stream). Stage 3 WI 5.
fn build_single_track_writer(tune: &Tune, params: &MidiParams) -> MidiWriter {
    let mut writer = MidiWriter::new(params.ticks_per_beat, params.channel);

    // Set tempo
    if let Some(tempo) = &tune.header.tempo {
        writer.tempo(tempo.beat_unit, tempo.bpm);
    } else {
        writer.tempo((1, 4), 120); // Default: quarter = 120
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
        let pitch_offset = get_voice_pitch_offset(voice, &tune.header.voice_defs)
            + key_pitch_offset(&tune.header.key)
            + tune.header.midi_transpose.map(|t| t as i16).unwrap_or(0);

        // Strip slur grouping (MIDI-irrelevant), then expand repeats.
        let flat = flatten_slurs(&voice.elements);
        let elements = expand_repeats(&flat);

        // Bar-scoped accidentals reset at each bar line
        let mut bar_accidentals = key_accidentals.clone();

        // Track held (tied) notes: midi_pitch -> accumulated ticks
        let mut held_notes: HashMap<u8, u32> = HashMap::new();
        // Accidental carried by a tie to the next same-pitch note, so it stays
        // valid across the bar-line accidental reset (§4.2). Keyed (letter, octave).
        let mut tie_carry: HashMap<(NoteName, i8), Accidental> = HashMap::new();
        // Key/length/meter, mutated by inline `[K:]`/`[L:]`/`[M:]` fields (§3.2).
        let mut voice_key_acc = key_accidentals.clone();
        let mut unit_ticks = unit_ticks;
        let mut cur_meter = tune.header.meter.clone();
        // Gracenotes buffered until their principal note arrives (§4.20).
        let mut pending_grace: Vec<Note> = Vec::new();

        for element in &elements {
            match element {
                Element::Note(note) => {
                    // Resolve the accidental (own > bar > tie-carry), then pitch.
                    let carried = tie_carry.remove(&(note.pitch, note.octave));
                    let eff_acc =
                        effective_accidental(note.pitch, note.accidental, &bar_accidentals, carried);
                    let base_pitch = midi_pitch_with_accidental(note.pitch, note.octave, eff_acc);
                    let midi_pitch = apply_pitch_offset(base_pitch, pitch_offset);
                    let mut ticks = note.duration.to_ticks(unit_ticks);

                    // Sound any buffered gracenotes, stealing their time from
                    // this principal note so the beat grid stays intact.
                    if !pending_grace.is_empty() {
                        let stolen = play_grace_run(
                            &mut writer,
                            &pending_grace,
                            ticks,
                            unit_ticks,
                            &bar_accidentals,
                            pitch_offset,
                            params.velocity,
                            params.channel,
                        );
                        ticks = ticks.saturating_sub(stolen);
                        pending_grace.clear();
                    }

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

                    // A tie carries the effective accidental to the next note.
                    if note.tie {
                        if let Some(acc) = eff_acc {
                            tie_carry.insert((note.pitch, note.octave), acc);
                        }
                    }

                    // Update bar accidentals if note has explicit accidental
                    if let Some(acc) = note.accidental {
                        bar_accidentals.insert(note.pitch, acc);
                    }
                }

                Element::Chord(chord) => {
                    let mut ticks = chord.duration.to_ticks(unit_ticks);

                    // Gracenotes ornament the chord too; steal from its duration.
                    if !pending_grace.is_empty() {
                        let stolen = play_grace_run(
                            &mut writer,
                            &pending_grace,
                            ticks,
                            unit_ticks,
                            &bar_accidentals,
                            pitch_offset,
                            params.velocity,
                            params.channel,
                        );
                        ticks = ticks.saturating_sub(stolen);
                        pending_grace.clear();
                    }

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
                        let ticks_per_bar =
                            meter_bar_ticks(cur_meter.as_ref(), params.ticks_per_beat);
                        writer.advance(ticks_per_bar * bars as u32);
                    } else {
                        let ticks = rest.duration.to_ticks(unit_ticks);
                        writer.advance(ticks);
                    }
                }

                Element::Bar(_) => {
                    // Reset bar accidentals to the (possibly inline-changed) key.
                    bar_accidentals = voice_key_acc.clone();
                }

                // Mid-tune inline field changes (§3.2): K: swaps the signature,
                // L: the unit length, M: the meter (multi-measure-rest length).
                Element::InlineField(field) => match field.field_type {
                    'K' => {
                        voice_key_acc = inline_key_accidentals(&field.value);
                        bar_accidentals = voice_key_acc.clone();
                    }
                    'L' => {
                        if let Some(ul) = inline_unit_length(&field.value) {
                            unit_ticks = compute_unit_ticks(&ul, params.ticks_per_beat);
                        }
                    }
                    'M' => cur_meter = Some(inline_meter(&field.value)),
                    _ => {}
                },

                Element::GraceNotes { notes, .. } => {
                    // Buffer until the principal note/chord steals their time.
                    pending_grace = notes.clone();
                }

                Element::Tuplet(tuplet) => {
                    // Each inner element's duration scales by q/p. Notes, rests
                    // AND chords all participate — a dropped rest/chord would
                    // shorten the tuplet and start later notes early (ABC §4.13).
                    let (q, p) = (tuplet.q as u32, (tuplet.p as u32).max(1)); // p=0 only on malformed input
                    for elem in &tuplet.elements {
                        match elem {
                            Element::Note(note) => {
                                let base_pitch = note_to_midi_pitch(
                                    note.pitch,
                                    note.octave,
                                    note.accidental,
                                    &bar_accidentals,
                                );
                                let midi_pitch = apply_pitch_offset(base_pitch, pitch_offset);
                                let ticks = note.duration.to_ticks(unit_ticks) * q / p;
                                writer.note(midi_pitch, params.velocity, ticks);
                                if let Some(acc) = note.accidental {
                                    bar_accidentals.insert(note.pitch, acc);
                                }
                            }
                            Element::Rest(rest) => {
                                writer.advance(rest.duration.to_ticks(unit_ticks) * q / p);
                            }
                            Element::Chord(chord) => {
                                let ticks = chord.duration.to_ticks(unit_ticks) * q / p;
                                for note in &chord.notes {
                                    let base_pitch = note_to_midi_pitch(
                                        note.pitch,
                                        note.octave,
                                        note.accidental,
                                        &bar_accidentals,
                                    );
                                    writer.note_on(
                                        apply_pitch_offset(base_pitch, pitch_offset),
                                        params.velocity,
                                    );
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
                                    writer.note_off(apply_pitch_offset(base_pitch, pitch_offset));
                                }
                            }
                            _ => {}
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

    writer
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
        tempo_writer.tempo(tempo.beat_unit, tempo.bpm);
    } else {
        tempo_writer.tempo((1, 4), 120);
    }
    tracks.push(tempo_writer.encode_track());

    // One track per voice
    for (voice_idx, voice) in tune.voices.iter().enumerate() {
        if voice.elements.is_empty() {
            continue;
        }

        // Get pitch offset from voice properties (transpose, octave)
        let pitch_offset = get_voice_pitch_offset(voice, &tune.header.voice_defs)
            + key_pitch_offset(&tune.header.key)
            + tune.header.midi_transpose.map(|t| t as i16).unwrap_or(0);

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

        let flat = flatten_slurs(&voice.elements);
        let elements = expand_repeats(&flat);
        let mut bar_accidentals = key_accidentals.clone();
        let mut held_notes: HashMap<u8, u32> = HashMap::new();
        let mut tie_carry: HashMap<(NoteName, i8), Accidental> = HashMap::new();
        let mut voice_key_acc = key_accidentals.clone();
        let mut unit_ticks = unit_ticks;
        let mut cur_meter = tune.header.meter.clone();
        let mut pending_grace: Vec<Note> = Vec::new();

        for element in &elements {
            match element {
                Element::Note(note) => {
                    let carried = tie_carry.remove(&(note.pitch, note.octave));
                    let eff_acc =
                        effective_accidental(note.pitch, note.accidental, &bar_accidentals, carried);
                    let base_pitch = midi_pitch_with_accidental(note.pitch, note.octave, eff_acc);
                    let midi_pitch = apply_pitch_offset(base_pitch, pitch_offset);
                    let mut ticks = note.duration.to_ticks(unit_ticks);

                    if !pending_grace.is_empty() {
                        let stolen = play_grace_run(
                            &mut writer,
                            &pending_grace,
                            ticks,
                            unit_ticks,
                            &bar_accidentals,
                            pitch_offset,
                            params.velocity,
                            channel,
                        );
                        ticks = ticks.saturating_sub(stolen);
                        pending_grace.clear();
                    }

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

                    if note.tie {
                        if let Some(acc) = eff_acc {
                            tie_carry.insert((note.pitch, note.octave), acc);
                        }
                    }

                    if let Some(acc) = note.accidental {
                        bar_accidentals.insert(note.pitch, acc);
                    }
                }

                Element::Chord(chord) => {
                    let mut ticks = chord.duration.to_ticks(unit_ticks);
                    if !pending_grace.is_empty() {
                        let stolen = play_grace_run(
                            &mut writer,
                            &pending_grace,
                            ticks,
                            unit_ticks,
                            &bar_accidentals,
                            pitch_offset,
                            params.velocity,
                            channel,
                        );
                        ticks = ticks.saturating_sub(stolen);
                        pending_grace.clear();
                    }
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
                        let ticks_per_bar =
                            meter_bar_ticks(cur_meter.as_ref(), params.ticks_per_beat);
                        writer.advance(ticks_per_bar * bars as u32);
                    } else {
                        writer.advance(rest.duration.to_ticks(unit_ticks));
                    }
                }

                Element::Bar(_) => {
                    bar_accidentals = voice_key_acc.clone();
                }

                Element::InlineField(field) => match field.field_type {
                    'K' => {
                        voice_key_acc = inline_key_accidentals(&field.value);
                        bar_accidentals = voice_key_acc.clone();
                    }
                    'L' => {
                        if let Some(ul) = inline_unit_length(&field.value) {
                            unit_ticks = compute_unit_ticks(&ul, params.ticks_per_beat);
                        }
                    }
                    'M' => cur_meter = Some(inline_meter(&field.value)),
                    _ => {}
                },

                Element::GraceNotes { notes, .. } => {
                    pending_grace = notes.clone();
                }

                Element::Tuplet(tuplet) => {
                    // Notes, rests and chords inside the tuplet all scale by q/p
                    // (ABC §4.13); dropping rests/chords corrupts timing.
                    let (q, p) = (tuplet.q as u32, (tuplet.p as u32).max(1)); // p=0 only on malformed input
                    for elem in &tuplet.elements {
                        match elem {
                            Element::Note(note) => {
                                let base_pitch = note_to_midi_pitch(
                                    note.pitch,
                                    note.octave,
                                    note.accidental,
                                    &bar_accidentals,
                                );
                                let midi_pitch = apply_pitch_offset(base_pitch, pitch_offset);
                                let ticks = note.duration.to_ticks(unit_ticks) * q / p;
                                writer.note_channel(midi_pitch, params.velocity, ticks, channel);
                                if let Some(acc) = note.accidental {
                                    bar_accidentals.insert(note.pitch, acc);
                                }
                            }
                            Element::Rest(rest) => {
                                writer.advance(rest.duration.to_ticks(unit_ticks) * q / p);
                            }
                            Element::Chord(chord) => {
                                let ticks = chord.duration.to_ticks(unit_ticks) * q / p;
                                for note in &chord.notes {
                                    let base_pitch = note_to_midi_pitch(
                                        note.pitch,
                                        note.octave,
                                        note.accidental,
                                        &bar_accidentals,
                                    );
                                    writer.note_on_channel(
                                        apply_pitch_offset(base_pitch, pitch_offset),
                                        params.velocity,
                                        channel,
                                    );
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
                                    writer.note_off_channel(
                                        apply_pitch_offset(base_pitch, pitch_offset),
                                        channel,
                                    );
                                }
                            }
                            _ => {}
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

/// Resolve the accidental in effect for a note: its own explicit accidental,
/// else a bar-scoped accidental (which is seeded from the key signature), else
/// an accidental carried across a bar line by a tie. ABC v2.1 §4.2.
fn effective_accidental(
    pitch: NoteName,
    note_accidental: Option<Accidental>,
    bar_accidentals: &HashMap<NoteName, Accidental>,
    carried: Option<Accidental>,
) -> Option<Accidental> {
    note_accidental
        .or_else(|| bar_accidentals.get(&pitch).copied())
        .or(carried)
}

/// Convert note to MIDI pitch, applying accidentals from context
fn note_to_midi_pitch(
    pitch: NoteName,
    octave: i8,
    note_accidental: Option<Accidental>,
    bar_accidentals: &HashMap<NoteName, Accidental>,
) -> u8 {
    let acc = effective_accidental(pitch, note_accidental, bar_accidentals, None);
    midi_pitch_with_accidental(pitch, octave, acc)
}

/// MIDI pitch for a note with its accidental already resolved.
fn midi_pitch_with_accidental(pitch: NoteName, octave: i8, acc: Option<Accidental>) -> u8 {
    let base = pitch.to_semitone();
    // ABC octave 0 (uppercase C-B) = MIDI 60-71 (middle C octave)
    // ABC octave 1 (lowercase c-b) = MIDI 72-83
    let octave_offset = (octave + 5) * 12;
    let acc_offset = acc.map(|a| a.to_semitone_offset()).unwrap_or(0);
    ((base as i16) + (octave_offset as i16) + (acc_offset as i16)).clamp(0, 127) as u8
}

/// Key-signature accidentals for an inline `[K:…]` field value (e.g. `"G"`,
/// `"D dor"`). A mid-tune key change replaces the active key signature; the
/// caller also resets the bar-scoped accidentals to the new signature. §3.2.
fn inline_key_accidentals(value: &str) -> HashMap<NoteName, Accidental> {
    let mut fc = crate::feedback::FeedbackCollector::new();
    compute_key_accidentals(&crate::parser::key::parse_key_field(value, &mut fc))
}

/// Parse an inline `[L:n/m]` value into a [`UnitLength`]. Returns `None` for a
/// malformed value so the caller keeps the current unit length. §3.2.
fn inline_unit_length(value: &str) -> Option<UnitLength> {
    let (n, d) = value.trim().split_once('/')?;
    Some(UnitLength {
        numerator: n.trim().parse().ok()?,
        denominator: d.trim().parse().ok()?,
    })
}

/// Parse an inline `[M:…]` value into a [`crate::ast::Meter`]. §3.2.
fn inline_meter(value: &str) -> crate::ast::Meter {
    let mut fc = crate::feedback::FeedbackCollector::new();
    crate::parser::header::parse_meter(value.trim(), &mut fc)
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

    // Explicit / modifying accidentals in the K: field (e.g. `K:C exp ^f`,
    // `K:Hp`) apply like a key signature — to every matching pitch in all
    // octaves — and override the circle-of-fifths entry for that letter.
    // ABC v2.1 §4.2 / §6.1.2. The parser populates `explicit_accidentals`;
    // honour it here so K:Hp keeps its C# and `exp` accidentals sound.
    for (acc, note) in &key.explicit_accidentals {
        accidentals.insert(*note, *acc);
    }

    accidentals
}

/// Get number of sharps (positive) or flats (negative) for a key
fn key_signature_accidentals(key: &Key) -> (i8, bool) {
    // Position of the (major) tonic on the circle of fifths = its key-signature
    // count. Each note letter has a base fifths-index; a sharp adds 7, a flat
    // subtracts 7. This handles accidental'd tonics (G#, D#, A#, …) that aren't
    // in the 15 standard-major spellings. ABC v2.1 §3.1.14.
    let letter_fifths = match key.root {
        NoteName::F => -1,
        NoteName::C => 0,
        NoteName::G => 1,
        NoteName::D => 2,
        NoteName::A => 3,
        NoteName::E => 4,
        NoteName::B => 5,
    };
    let acc_fifths = match key.accidental {
        Some(Accidental::Sharp) => 7,
        Some(Accidental::DoubleSharp) => 14,
        Some(Accidental::Flat) => -7,
        Some(Accidental::DoubleFlat) => -14,
        _ => 0,
    };
    let base: i8 = letter_fifths + acc_fifths;

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

/// Ticks in one full bar of the tune's meter, for multi-measure rests (`Zn`).
/// A bar lasts `num/den` of a whole note; a whole note is `ticks_per_beat*4`
/// ticks (ticks_per_beat is per quarter). Free meter (`M:none`) assumes 4/4.
/// ABC v2.1 §4.5. NB: must use the denominator — `6/8` is 3 quarters, not 6.
fn meter_bar_ticks(meter: Option<&crate::ast::Meter>, ticks_per_beat: u16) -> u32 {
    let (num, den) = meter.map(|m| m.to_fraction()).unwrap_or((4, 4));
    // `den` is 0 only for a malformed meter like `M:0/0`; clamp to avoid a panic.
    (ticks_per_beat as u32 * 4 * num as u32) / (den.max(1) as u32)
}

/// Compute MIDI ticks per ABC unit note
fn compute_unit_ticks(unit_length: &UnitLength, ticks_per_beat: u16) -> u32 {
    // Unit length is relative to a whole note
    // ticks_per_beat is ticks per quarter note
    // So ticks per whole note = ticks_per_beat * 4
    let ticks_per_whole = ticks_per_beat as u32 * 4;
    // A 0 denominator only comes from malformed input (`L:1/0`); clamp to 1.
    (ticks_per_whole * unit_length.numerator as u32) / (unit_length.denominator.max(1) as u32)
}

/// MIDI file writer
struct MidiWriter {
    ticks_per_beat: u16,
    channel: u8,
    events: Vec<MidiEvent>,
    current_tick: u32,
}

/// One timed MIDI message: raw status+data bytes at an absolute tick. The
/// `data` bytes are a complete MIDI message (e.g. `[0x90|ch, pitch, vel]` for a
/// NoteOn, or `[0xFF, 0x51, …]` for a tempo meta event) — exactly what the SMF
/// track stores after its delta-time. Exposed by [`events`] for the Stage 3
/// render target (docs/tracks.md WI 5).
pub struct MidiEvent {
    pub tick: u32,
    pub data: Vec<u8>,
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

    /// Emit a Set Tempo meta event. `beat_unit` is the ABC `Q:` beat (e.g.
    /// `(1, 4)` for a quarter, `(1, 2)` for a half) and `bpm` is beats per
    /// minute. The MIDI meta stores microseconds-per-QUARTER, so we convert:
    /// one beat = `num/den` of a whole note = `4*num/den` quarters, hence
    /// `us_per_quarter = 60_000_000 * den / (bpm * num * 4)`. ABC v2.1 §3.1.8.
    fn tempo(&mut self, beat_unit: (u8, u8), bpm: u16) {
        let (num, den) = beat_unit;
        let denom = (bpm as u64) * (num as u64) * 4;
        let us_per_quarter = if denom == 0 {
            500_000
        } else {
            (60_000_000u64 * den as u64 / denom) as u32
        };
        self.meta_event(
            0x51,
            vec![
                ((us_per_quarter >> 16) & 0xFF) as u8,
                ((us_per_quarter >> 8) & 0xFF) as u8,
                (us_per_quarter & 0xFF) as u8,
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

    /// Pull the microseconds-per-quarter value out of a tempo meta event
    /// (`0xFF 0x51 0x03 b0 b1 b2`) in an event stream.
    fn tempo_us_per_quarter(evs: &[MidiEvent]) -> Option<u32> {
        evs.iter()
            .find(|e| e.data.len() >= 6 && e.data[0] == 0xFF && e.data[1] == 0x51)
            .map(|e| {
                ((e.data[3] as u32) << 16) | ((e.data[4] as u32) << 8) | (e.data[5] as u32)
            })
    }

    #[test]
    fn tempo_respects_non_quarter_beat_unit() {
        // ABC v2.1 §3.1.8: Q:<beat>=<bpm>. The MIDI tempo meta stores
        // microseconds-per-QUARTER regardless of the notated beat, so a
        // non-quarter beat unit must be converted, not passed through as bpm.
        let cases = [
            // (Q field, expected us/quarter)
            ("1/4=120", 500_000), // quarter=120 → 500000 (baseline)
            ("1/2=120", 250_000), // half=120 → quarter=240 → 250000
            ("3/8=120", 333_333), // dotted-quarter=120 → quarter=180 → 333333
            ("1/8=120", 1_000_000), // eighth=120 → quarter=60 → 1000000
        ];
        for (q, expected) in cases {
            let abc = format!("X:1\nT:t\nM:4/4\nL:1/4\nQ:{q}\nK:C\nCDEF|\n");
            let tune = &crate::parse(&abc).value[0];
            let evs = events(tune, &MidiParams::default());
            let got = tempo_us_per_quarter(&evs).expect("a tempo meta event");
            assert_eq!(got, expected, "Q:{q} → wrong us/quarter");
        }
    }

    #[test]
    fn midi_transpose_directive_applies() {
        // `%%MIDI transpose N` shifts playback by N semitones (matrix: midi).
        let abc = "X:1\nT:t\n%%MIDI transpose -14\nM:4/4\nL:1/4\nK:C\nc|\n";
        let tune = &crate::parse(abc).value[0];
        assert_eq!(tune.header.midi_transpose, Some(-14));
        let evs = events(tune, &MidiParams::default());
        let first = evs
            .iter()
            .find(|e| e.data.first().map(|b| b & 0xF0) == Some(0x90))
            .unwrap();
        assert_eq!(first.data[1], 58, "c (72) transposed -14 semitones = 58");
    }

    #[test]
    fn key_level_transpose_and_octave_apply_in_midi() {
        // ABC v2.1 §4.6: K: `transpose=<semitones>` and `octave=<int>` shift
        // playback for the whole tune.
        let abc = "X:1\nT:t\nM:4/4\nL:1/4\nK:C transpose=2\nC|\n";
        let evs = events(&crate::parse(abc).value[0], &MidiParams::default());
        let first = evs
            .iter()
            .find(|e| e.data.first().map(|b| b & 0xF0) == Some(0x90))
            .unwrap();
        assert_eq!(first.data[1], 62, "C transposed +2 semitones = D (62)");

        let abc2 = "X:1\nT:t\nM:4/4\nL:1/4\nK:C octave=1\nC|\n";
        let evs2 = events(&crate::parse(abc2).value[0], &MidiParams::default());
        let first2 = evs2
            .iter()
            .find(|e| e.data.first().map(|b| b & 0xF0) == Some(0x90))
            .unwrap();
        assert_eq!(first2.data[1], 72, "C up one octave = 72");
    }

    #[test]
    fn inline_unit_length_change_applies_in_midi() {
        // ABC v2.1 §3.2: an inline [L:] changes the unit note length mid-tune.
        // L:1/4 → first C is a quarter (480t); after [L:1/8] the second C is an
        // eighth (240t): note-offs land at 480 and 720, not 480 and 960.
        let abc = "X:1\nT:t\nM:4/4\nL:1/4\nK:C\nC[L:1/8]C|\n";
        let tune = &crate::parse(abc).value[0];
        let evs = events(tune, &MidiParams::default());
        let offs: Vec<u32> = evs
            .iter()
            .filter(|e| e.data.first().map(|b| b & 0xF0) == Some(0x80))
            .map(|e| e.tick)
            .collect();
        assert_eq!(offs, vec![480, 720], "second C is an eighth after [L:1/8]");
    }

    #[test]
    fn inline_key_change_applies_in_midi() {
        // ABC v2.1 §3.2: an inline [K:] changes the key mid-tune. In K:C an F is
        // natural (65); after [K:G] it must sound as F# (66).
        let abc = "X:1\nT:t\nM:4/4\nL:1/4\nK:C\nF|[K:G]F|\n";
        let tune = &crate::parse(abc).value[0];
        let evs = events(tune, &MidiParams::default());
        let ons: Vec<u8> = evs
            .iter()
            .filter(|e| e.data.first().map(|b| b & 0xF0) == Some(0x90))
            .map(|e| e.data[1])
            .collect();
        assert_eq!(ons, vec![65, 66], "F natural, then F# after [K:G]");
    }

    #[test]
    fn tie_carries_accidental_across_barline() {
        // ABC v2.1 §4.2/§4.11: an accidental is carried by a tie across a bar
        // line, so `^C-|C` is a single sustained C# — not a stuck C# (never
        // released) plus a separate natural C. Exactly one on (61) + one off.
        let abc = "X:1\nT:t\nM:4/4\nL:1/4\nK:C\n^C-|C|\n";
        let tune = &crate::parse(abc).value[0];
        let evs = events(tune, &MidiParams::default());
        let ons: Vec<u8> = evs
            .iter()
            .filter(|e| e.data.first().map(|b| b & 0xF0) == Some(0x90))
            .map(|e| e.data[1])
            .collect();
        let offs: Vec<u8> = evs
            .iter()
            .filter(|e| e.data.first().map(|b| b & 0xF0) == Some(0x80))
            .map(|e| e.data[1])
            .collect();
        assert_eq!(ons, vec![61], "one tied C# note-on");
        assert_eq!(offs, vec![61], "one matching note-off — no hung note");
    }

    #[test]
    fn chord_inner_note_duration_sets_length() {
        // ABC v2.1 §4.17: when inner notes carry a length, the chord's duration
        // is the first note's (inner × outer). `[c4a4]` at L:1/4 = 4 quarters =
        // 1920 ticks, so a following G starts at 1920 (not 480).
        let abc = "X:1\nT:t\nM:4/4\nL:1/4\nK:C\n[c4a4]G|\n";
        let tune = &crate::parse(abc).value[0];
        let evs = events(tune, &MidiParams::default());
        let on_ticks: Vec<u32> = evs
            .iter()
            .filter(|e| e.data.first().map(|b| b & 0xF0) == Some(0x90))
            .map(|e| e.tick)
            .collect();
        assert_eq!(on_ticks, vec![0, 0, 1920], "chord c+a last 4 units; G at 1920");
    }

    #[test]
    fn chord_inner_times_outer_duration() {
        // `[C2E2G2]3` ≡ `[CEG]6` (inner 2 × outer 3 = 6 units). At L:1/4 → 6
        // quarters = 2880 ticks; following G at 2880.
        let abc = "X:1\nT:t\nM:4/4\nL:1/4\nK:C\n[C2E2G2]3 G|\n";
        let tune = &crate::parse(abc).value[0];
        let evs = events(tune, &MidiParams::default());
        let last_on = evs
            .iter()
            .filter(|e| e.data.first().map(|b| b & 0xF0) == Some(0x90))
            .map(|e| e.tick)
            .max()
            .unwrap();
        assert_eq!(last_on, 2880, "[C2E2G2]3 = 6 units; trailing G at 2880");
    }

    #[test]
    fn grace_notes_sound_and_steal_from_the_following_note() {
        // §4.20: gracenotes sound briefly before their principal note. We steal
        // their time from that note so the beat grid is preserved. `{ga}c d`:
        // grace g(79),a(81) play first, then c(72), then d(74) still on the beat.
        let abc = "X:1\nT:t\nM:4/4\nL:1/4\nK:C\n{ga}c d|\n";
        let tune = &crate::parse(abc).value[0];
        let evs = events(tune, &MidiParams::default());
        let ons: Vec<(u32, u8)> = evs
            .iter()
            .filter(|e| e.data.first().map(|b| b & 0xF0) == Some(0x90))
            .map(|e| (e.tick, e.data[1]))
            .collect();
        let pitches: Vec<u8> = ons.iter().map(|(_, p)| *p).collect();
        assert_eq!(pitches, vec![79, 81, 72, 74], "grace g,a then c then d");
        assert_eq!(ons[0].0, 0, "first grace starts at the beat");
        let d = ons.iter().find(|(_, p)| *p == 74).unwrap();
        assert_eq!(d.0, 480, "d stays on the beat — graces stole from c");
    }

    #[test]
    fn tuplet_rest_advances_time() {
        // ABC v2.1 §4.13: a tuplet groups notes/rests/chords; each element's
        // duration scales by q/p. `(3zab` = rest,a,b in the time of 2: at L:1/4
        // (480 ticks) each slot is 480*2/3 = 320. The rest must advance time, so
        // a starts at 320 and b at 640 — not a@0 (rest silently dropped).
        let abc = "X:1\nT:t\nM:4/4\nL:1/4\nK:C\n(3zab|\n";
        let tune = &crate::parse(abc).value[0];
        let evs = events(tune, &MidiParams::default());
        let on_ticks: Vec<u32> = evs
            .iter()
            .filter(|e| e.data.first().map(|b| b & 0xF0) == Some(0x90))
            .map(|e| e.tick)
            .collect();
        assert_eq!(on_ticks, vec![320, 640], "rest slot advances; a@320 b@640");
    }

    #[test]
    fn tuplet_chord_sounds_all_notes() {
        // `(3[CEG]ab`: triplet of chord,a,b. The chord must sound all three of
        // its notes (not be dropped). Expect 3 (chord) + 1 + 1 = 5 NoteOns.
        let abc = "X:1\nT:t\nM:4/4\nL:1/4\nK:C\n(3[CEG]ab|\n";
        let tune = &crate::parse(abc).value[0];
        let evs = events(tune, &MidiParams::default());
        let on_count = evs
            .iter()
            .filter(|e| e.data.first().map(|b| b & 0xF0) == Some(0x90))
            .count();
        assert_eq!(on_count, 5, "chord(3) + a + b = 5 NoteOns");
    }

    #[test]
    fn uppercase_x_multi_measure_invisible_rest() {
        // ABC v2.1 §4.5: `Xn` is an invisible multi-measure rest with the same
        // timing as `Zn`. `X2` in 4/4 advances two full bars (2×1920 = 3840).
        let abc = "X:1\nT:t\nM:4/4\nL:1/4\nK:C\nX2 C|\n";
        let tune = &crate::parse(abc).value[0];
        let has_invisible_mm = tune.voices[0].elements.iter().any(
            |e| matches!(e, Element::Rest(r) if r.multi_measure == Some(2) && !r.visible),
        );
        assert!(has_invisible_mm, "X2 should be an invisible 2-bar rest");
        let evs = events(tune, &MidiParams::default());
        let first_on = evs
            .iter()
            .find(|e| e.data.first().map(|b| b & 0xF0) == Some(0x90))
            .unwrap();
        assert_eq!(first_on.tick, 3840, "X2 advances two full bars");
    }

    #[test]
    fn multi_measure_rest_uses_meter_denominator() {
        // ABC v2.1 §4.5: `Zn` rests for n full bars. A 6/8 bar = 6 eighths =
        // 3 quarters = 1440 ticks at 480/quarter, so Z2 = 2880 ticks and the
        // following note must start there (not 5760 from beat-count × num).
        let abc = "X:1\nT:t\nM:6/8\nL:1/8\nQ:1/4=120\nK:C\nZ2 C|\n";
        let tune = &crate::parse(abc).value[0];
        let evs = events(tune, &MidiParams::default());
        let first_on = evs
            .iter()
            .find(|e| e.data.first().map(|b| b & 0xF0) == Some(0x90))
            .expect("a note on after the rest");
        assert_eq!(first_on.tick, 2880, "Z2 in 6/8 should advance 2*1440 ticks");
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
            transpose: 0,
            octave: 0,
            stafflines: None,
            middle: None,
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
            transpose: 0,
            octave: 0,
            stafflines: None,
            middle: None,
        };
        let acc = compute_key_accidentals(&key);
        assert_eq!(acc.get(&NoteName::B), Some(&Accidental::Flat));
        assert_eq!(acc.len(), 1);
    }

    #[test]
    fn key_explicit_accidentals_reach_midi() {
        // ABC v2.1 §4.2/§6.1.2: explicit/modifying accidentals in K: apply to
        // every matching pitch (all octaves) like a key signature. K:Hp marks
        // F# AND C#; the circle of fifths for D Mixolydian only yields F#, so
        // the C# must come from key.explicit_accidentals. Both C's → C# (61/73).
        let abc = "X:1\nT:t\nM:4/4\nL:1/4\nK:Hp\nC c F f|\n";
        let tune = &crate::parse(abc).value[0];
        let evs = events(tune, &MidiParams::default());
        let on_pitches: Vec<u8> = evs
            .iter()
            .filter(|e| e.data.first().map(|b| b & 0xF0) == Some(0x90))
            .map(|e| e.data[1])
            .collect();
        // C#4=61, C#5=73, F#4=66, F#5=78
        assert_eq!(on_pitches, vec![61, 73, 66, 78], "K:Hp must sharpen C and F");
    }

    #[test]
    fn key_exp_accidental_reaches_midi() {
        // K:C exp ^f — explicit sharp on F over an otherwise-empty signature.
        let abc = "X:1\nT:t\nM:4/4\nL:1/4\nK:C exp ^f\nF f|\n";
        let tune = &crate::parse(abc).value[0];
        let evs = events(tune, &MidiParams::default());
        let on_pitches: Vec<u8> = evs
            .iter()
            .filter(|e| e.data.first().map(|b| b & 0xF0) == Some(0x90))
            .map(|e| e.data[1])
            .collect();
        assert_eq!(on_pitches, vec![66, 78], "K:C exp ^f must sharpen both F's");
    }

    #[test]
    fn sharp_minor_and_modal_keys_get_correct_signature() {
        // ABC v2.1 §3.1.14: G#m = 5 sharps, D#m = 6 sharps, A#m = 7 sharps.
        // Tonics with an accidental (G#, D#, A#) aren't in the standard-major
        // table; the signature must come from the circle-of-fifths position.
        fn sharps(root: NoteName, acc: Option<Accidental>, mode: Mode) -> Vec<NoteName> {
            let key = Key {
                root,
                accidental: acc,
                mode,
                ..Default::default()
            };
            let mut v: Vec<NoteName> = compute_key_accidentals(&key)
                .iter()
                .filter(|(_, a)| **a == Accidental::Sharp)
                .map(|(n, _)| *n)
                .collect();
            v.sort_by_key(|n| n.to_semitone());
            v
        }
        // G#m → 5 sharps: F# C# G# D# A#
        assert_eq!(
            sharps(NoteName::G, Some(Accidental::Sharp), Mode::Minor).len(),
            5,
            "G#m should have 5 sharps"
        );
        // D#m → 6 sharps
        assert_eq!(
            sharps(NoteName::D, Some(Accidental::Sharp), Mode::Minor).len(),
            6,
            "D#m should have 6 sharps"
        );
        // A#m → 7 sharps
        assert_eq!(
            sharps(NoteName::A, Some(Accidental::Sharp), Mode::Minor).len(),
            7,
            "A#m should have 7 sharps"
        );
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
            transpose: 0,
            octave: 0,
            stafflines: None,
            middle: None,
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
    fn events_yields_timed_note_on_off_stream() {
        // Stage 3 WI 5: the per-event view the render target consumes. CDEF at
        // L:1/4 / 120 BPM → four quarter notes, each one beat (480 ticks default).
        let abc = "X:1\nT:t\nM:4/4\nL:1/4\nQ:1/4=120\nK:C\nCDEF|\n";
        let result = crate::parse(abc);
        assert!(!result.has_errors());
        let tune = &result.value[0];
        let params = MidiParams::default();

        let evs = events(tune, &params);

        // Sorted by tick (the contract the render target relies on).
        assert!(
            evs.windows(2).all(|w| w[0].tick <= w[1].tick),
            "events() must return a tick-sorted stream"
        );

        // The four NoteOns land one beat apart: 0, 480, 960, 1440.
        let note_on_ticks: Vec<u32> = evs
            .iter()
            .filter(|e| e.data.first().map(|b| b & 0xF0) == Some(0x90))
            .map(|e| e.tick)
            .collect();
        assert_eq!(
            note_on_ticks,
            vec![0, 480, 960, 1440],
            "four quarter-note NoteOns, one beat (480 ticks) apart"
        );

        // Each NoteOn has a matching NoteOff one beat later (4 on, 4 off).
        let note_off_ticks: Vec<u32> = evs
            .iter()
            .filter(|e| e.data.first().map(|b| b & 0xF0) == Some(0x80))
            .map(|e| e.tick)
            .collect();
        assert_eq!(
            note_off_ticks,
            vec![480, 960, 1440, 1920],
            "each note ends a beat after it starts"
        );
    }

    #[test]
    fn events_and_generate_share_the_single_track_writer() {
        // The refactor's invariant: `generate` (SMF blob) and `events` (timed
        // stream) are framings of the SAME writer, so the byte blob's channel
        // messages match the event stream's. A change to one without the other
        // would diverge here. Count NoteOns both ways and assert they agree.
        let abc = "X:1\nT:t\nM:4/4\nL:1/8\nK:C\nCDEFGABc|\n";
        let tune = &crate::parse(abc).value[0];
        let params = MidiParams::default();

        let evs = events(tune, &params);
        let blob = generate(tune, &params);

        let events_note_ons = evs
            .iter()
            .filter(|e| e.data.first().map(|b| b & 0xF0) == Some(0x90))
            .count();
        let blob_note_ons = blob.windows(1).filter(|w| w[0] & 0xF0 == 0x90).count();
        // (the blob count is approximate — 0x90 can appear in a delta/data byte —
        // so assert the stream has the expected 8 and the blob has at least that.)
        assert_eq!(events_note_ons, 8, "eight eighth-notes → eight NoteOns");
        assert!(
            blob_note_ons >= events_note_ons,
            "the SMF blob carries the same NoteOns the event stream does"
        );
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

        let midi = generate(&result.value[0], &MidiParams::default());

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

        let midi = generate(&result.value[0], &MidiParams::default());

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

        let midi = generate(&result.value[0], &MidiParams::default());

        // Should still be one note
        let note_ons = midi
            .windows(2)
            .filter(|w| w[0] == 0x90 && w[1] == 72)
            .count();
        assert_eq!(note_ons, 1, "Tie across bar should produce single note-on");
    }

    #[test]
    fn variant_endings_expand_in_midi() {
        // ABC v2.1 §4.9: `|: common |1 first :|2 second |` plays the common body
        // + first ending on pass 1, then the common body + second ending on pass
        // 2. `|:CD|1E:|2F|` → C D E (pass 1) then C D F (pass 2).
        let abc = "X:1\nT:t\nM:4/4\nL:1/4\nK:C\n|:CD|1E:|2F|\n";
        let tune = &crate::parse(abc).value[0];
        let evs = events(tune, &MidiParams::default());
        let ons: Vec<u8> = evs
            .iter()
            .filter(|e| e.data.first().map(|b| b & 0xF0) == Some(0x90))
            .map(|e| e.data[1])
            .collect();
        // C=60 D=62 E=64 F=65
        assert_eq!(
            ons,
            vec![60, 62, 64, 60, 62, 65],
            "common+1st ending, repeat, common+2nd ending"
        );
    }

    #[test]
    fn variant_endings_explicit_repeat_end_form() {
        // The `|1 … :| [2 …` shape: first ending closes at an explicit `:|`, and
        // the second ending opens with a bracket `[2`. `|:A|1B:|[2C|` →
        // A B (pass 1), A C (pass 2). A=69 B=71 C=60.
        let abc = "X:1\nT:t\nM:4/4\nL:1/4\nK:C\n|:A|1B:|[2C|\n";
        let tune = &crate::parse(abc).value[0];
        let evs = events(tune, &MidiParams::default());
        let ons: Vec<u8> = evs
            .iter()
            .filter(|e| e.data.first().map(|b| b & 0xF0) == Some(0x90))
            .map(|e| e.data[1])
            .collect();
        assert_eq!(ons, vec![69, 71, 69, 60], "common+1st, repeat, common+2nd");
    }

    #[test]
    fn test_repeat_expansion() {
        // |: c d :| should produce c d c d (lowercase c = MIDI 72, d = MIDI 74)
        let abc = "X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n|:cd:|\n";
        let result = crate::parse(abc);
        assert!(!result.has_errors(), "Parse errors: {:?}", result.feedback);

        let midi = generate(&result.value[0], &MidiParams::default());

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
        let midi_ch0 = generate(&result.value[0], &MidiParams::default());
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
        let midi_ch9 = generate(&result.value[0], &params_ch9);
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
        assert_eq!(result.value[0].header.midi_program, Some(33));

        let midi = generate(&result.value[0], &MidiParams::default());

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
        assert_eq!(result.value[0].header.midi_program, None);

        let params = MidiParams {
            velocity: 80,
            ticks_per_beat: 480,
            channel: 0,
            program: Some(56), // Trumpet
        };
        let midi = generate(&result.value[0], &params);

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
        let midi = generate(&result.value[0], &params);

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
            result.value[0].header.midi_program,
            Some(56),
            "%%MIDI program before K: should be parsed"
        );

        let midi = generate(&result.value[0], &MidiParams::default());
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
            result.value[0].header.midi_program, None,
            "%%MIDI program after K: is not parsed (in body, not header)"
        );
    }

    #[test]
    fn test_midi_program_various_positions() {
        // Test %%MIDI program works in various valid header positions
        let cases = [
            (
                "X:1\n%%MIDI program 40\nT:Test\nM:4/4\nK:C\nC|\n",
                Some(40),
                "after X:",
            ),
            (
                "X:1\nT:Test\n%%MIDI program 41\nM:4/4\nK:C\nC|\n",
                Some(41),
                "after T:",
            ),
            (
                "X:1\nT:Test\nM:4/4\n%%MIDI program 42\nL:1/4\nK:C\nC|\n",
                Some(42),
                "after M:",
            ),
            (
                "X:1\nT:Test\nM:4/4\nL:1/4\n%%MIDI program 43\nK:C\nC|\n",
                Some(43),
                "before K:",
            ),
            (
                "X:1\nT:Test\nM:4/4\nL:1/4\nK:C\n%%MIDI program 44\nC|\n",
                None,
                "after K: (body)",
            ),
        ];

        for (abc, expected_program, position) in cases {
            let result = crate::parse(abc);
            assert!(!result.has_errors(), "Parse failed for {}", position);
            assert_eq!(
                result.value[0].header.midi_program, expected_program,
                "Wrong program for %%MIDI {}",
                position
            );
        }
    }
}
