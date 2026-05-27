//! Layout engine: walks ABC AST and produces positioned EngravingElements.

use crate::ast::*;
use crate::engrave::font::font_cache;
use crate::engrave::ir::{EngravingElement, EngravingOptions, SourceSpan};

/// Layout walks the AST as a flat token stream; nested `SlurGroup`s are
/// expanded into explicit Start/End boundaries so the slur-stack logic
/// can treat them like the old `Element::Slur(SlurBoundary::*)` markers.
enum LayoutToken<'a> {
    Real(&'a Element),
    SlurStart,
    SlurEnd,
}

fn flatten_for_layout<'a>(elements: &'a [Element]) -> Vec<LayoutToken<'a>> {
    let mut out = Vec::with_capacity(elements.len());
    for e in elements {
        match e {
            Element::SlurGroup { elements: inner } => {
                out.push(LayoutToken::SlurStart);
                out.extend(flatten_for_layout(inner));
                out.push(LayoutToken::SlurEnd);
            }
            _ => out.push(LayoutToken::Real(e)),
        }
    }
    out
}

// --- Clef geometry ----------------------------------------------------------

/// SMuFL codepoint and reference-line staff position (in sp units, where
/// 0.0 is the top line and each integer step is one staff line down) for
/// each clef. Returns None for clefs we haven't placed yet.
fn clef_glyph(clef: Clef) -> Option<(u32, f64)> {
    match clef {
        Clef::Treble => Some((0xE050, 3.0)),     // G clef wraps G4 (2nd line from bottom)
        Clef::Bass => Some((0xE062, 1.0)),       // F clef on F3 (4th line from bottom)
        Clef::Alto => Some((0xE05C, 2.0)),       // C clef on middle line (C4)
        Clef::Tenor => Some((0xE05C, 1.0)),      // C clef on 4th line (C4)
        Clef::Percussion => Some((0xE069, 2.0)), // perc clef, centered
    }
}

/// Absolute diatonic of the note that sits on the middle staff line for
/// each clef, encoded the same way as in `note_to_staff_position`.
fn clef_middle_abs(clef: Clef) -> i32 {
    match clef {
        Clef::Treble => 13,     // B4 = (B, octave 1)
        Clef::Bass => 1,        // D3 = (D, octave 0)
        Clef::Alto => 7,        // C4 = (C, octave 1)
        Clef::Tenor => 5,       // A3 = (A, octave 0)
        Clef::Percussion => 13, // not really pitched; treat like treble for fallback
    }
}

/// Standard (note, ABC-octave) placements for the seven sharps in key-sig
/// order (F# C# G# D# A# E# B#). The octaves are conventional —
/// chosen so the accidental sits inside (or just outside) the staff.
fn sharp_octaves(clef: Clef) -> [(NoteName, i8); 7] {
    use NoteName::*;
    match clef {
        Clef::Treble => [(F, 2), (C, 2), (G, 2), (D, 2), (A, 1), (E, 2), (B, 1)],
        Clef::Bass => [(F, 0), (C, 0), (G, 0), (D, 0), (A, -1), (E, 0), (B, -1)],
        Clef::Alto => [(F, 1), (C, 1), (G, 1), (D, 1), (A, 0), (E, 1), (B, 0)],
        // Tenor sits high enough that the conventional placement matches
        // alto's sharps, except F# and G# which drop an octave to stay on
        // the staff.
        Clef::Tenor => [(F, 0), (C, 1), (G, 0), (D, 1), (A, 0), (E, 1), (B, 0)],
        Clef::Percussion => [(F, 2), (C, 2), (G, 2), (D, 2), (A, 1), (E, 2), (B, 1)],
    }
}

/// Standard (note, ABC-octave) placements for the seven flats in key-sig
/// order (Bb Eb Ab Db Gb Cb Fb).
fn flat_octaves(clef: Clef) -> [(NoteName, i8); 7] {
    use NoteName::*;
    match clef {
        Clef::Treble => [(B, 1), (E, 2), (A, 1), (D, 2), (G, 1), (C, 2), (F, 1)],
        Clef::Bass => [(B, -1), (E, 0), (A, -1), (D, 0), (G, -1), (C, 0), (F, -1)],
        Clef::Alto => [(B, 0), (E, 1), (A, 0), (D, 1), (G, 0), (C, 1), (F, 0)],
        Clef::Tenor => [(B, 0), (E, 1), (A, 0), (D, 1), (G, 0), (C, 1), (F, 0)],
        Clef::Percussion => [(B, 1), (E, 2), (A, 1), (D, 2), (G, 1), (C, 2), (F, 1)],
    }
}

// --- Key signature ----------------------------------------------------------

/// Key signature accidental count and sign — same logic as midi.rs.
fn key_signature_info(key: &Key) -> (i8, bool) {
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

    let mode_offset = match key.mode {
        Mode::Major | Mode::Ionian => 0,
        Mode::Minor | Mode::Aeolian => -3,
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

// --- Pitch → staff position ------------------------------------------------

/// Convert a note pitch + octave to a staff position relative to the staff's
/// top line, given a clef.
///
/// Position 0.0 = top line, each +1.0 = one staff line down (or one full
/// pitch step at conventional spacing). Half-integer positions = staff
/// spaces. So with sp=10:
///   - Top line     y = 0.0
///   - Middle line  y = 2.0 * sp = 20.0 (pos 2.0)
///   - Bottom line  y = 4.0 * sp = 40.0 (pos 4.0)
fn note_to_staff_position(pitch: &NoteName, octave: i8, clef: Clef) -> f64 {
    // Diatonic position within an octave (C=0, D=1, ..., B=6).
    let diatonic = match pitch {
        NoteName::C => 0,
        NoteName::D => 1,
        NoteName::E => 2,
        NoteName::F => 3,
        NoteName::G => 4,
        NoteName::A => 5,
        NoteName::B => 6,
    };

    // ABC octave 0 = uppercase (C3–B3), ABC octave 1 = lowercase (C4–B4).
    // Each diatonic step = half a staff_spacing.
    let abs_diatonic = octave as i32 * 7 + diatonic;
    let mid = clef_middle_abs(clef);
    (mid - abs_diatonic) as f64 * 0.5 + 2.0
}

// --- Per-staff context ------------------------------------------------------
//
// Bundles everything `emit_*` helpers need so we don't have to thread half
// a dozen parameters through each call.
#[derive(Clone, Copy)]
struct StaffCtx {
    /// Top line y in scene units.
    y_top: f64,
    /// Distance between staff lines.
    sp: f64,
    /// Scale for music glyphs (font units → sp).
    scale: f64,
    /// Resolved clef for this staff.
    clef: Clef,
}

impl StaffCtx {
    /// y for a given staff position (0.0 = top line, 4.0 = bottom line).
    fn y_at(&self, pos: f64) -> f64 {
        self.y_top + pos * self.sp
    }

    /// Staff position for a pitched note in this clef.
    fn pos_for(&self, pitch: &NoteName, octave: i8) -> f64 {
        note_to_staff_position(pitch, octave, self.clef)
    }
}

// --- Clef resolution --------------------------------------------------------

/// Pick the clef for a voice. Priority: explicit V: `clef=` > header K: `clef=`
/// > default (Treble).
fn resolve_clef(header: &Header, voice: &Voice) -> Clef {
    let voice_def = voice
        .id
        .as_ref()
        .and_then(|id| header.voice_defs.iter().find(|vd| &vd.id == id));
    voice_def
        .and_then(|vd| vd.clef)
        .or(header.key.clef)
        .unwrap_or(Clef::Treble)
}

// --- Entry point ------------------------------------------------------------

/// Lay out a tune as engraving elements.
pub fn engrave(tune: &Tune, options: &EngravingOptions) -> Vec<EngravingElement> {
    let mut elements = Vec::new();
    let font = font_cache();
    let sp = options.staff_spacing;
    let scale = sp / font.upem() * 4.0;

    // Title sits above the first staff.
    if !tune.header.title.is_empty() {
        elements.push(EngravingElement::Text {
            content: tune.header.title.clone(),
            x: 0.0,
            y: -sp * 2.0,
            size: sp * 1.8,
            source_span: (0, 0),
        });
    }

    // Each staff occupies 4*sp vertically (4 line gaps). Add a gap of 4*sp
    // between staves so stems/text have room. Total per voice = 8*sp.
    let staff_gap = sp * 4.0;
    let staff_height = sp * 4.0;

    // Render each voice. Default Tune::default() always has at least one
    // (empty) voice, so this loop is never zero-trip.
    let mut staff_line_ranges: Vec<(usize, usize, f64)> = Vec::new();
    let mut max_cursor = 0.0_f64;

    for (i, voice) in tune.voices.iter().enumerate() {
        let y_top = i as f64 * (staff_height + staff_gap);
        let clef = resolve_clef(&tune.header, voice);
        let ctx = StaffCtx {
            y_top,
            sp,
            scale,
            clef,
        };
        let (line_start, line_end, cursor_x) = render_staff(&mut elements, &tune.header, voice, ctx);
        staff_line_ranges.push((line_start, line_end, cursor_x));
        if cursor_x > max_cursor {
            max_cursor = cursor_x;
        }
    }

    // Normalize all staff line widths to the longest cursor — so multiple
    // voices land on staves of the same length, even though each rendered
    // independently.
    for (start, end, _cursor) in staff_line_ranges {
        for elem in &mut elements[start..end] {
            if let EngravingElement::Line { x2, .. } = elem {
                *x2 = max_cursor;
            }
        }
    }

    elements
}

/// Render one staff (one voice) at `ctx.y_top`. Returns
/// `(staff_line_start_idx, staff_line_end_idx, final_cursor_x)`. The
/// top-level uses the staff-line indices to normalize widths across voices.
fn render_staff(
    elements: &mut Vec<EngravingElement>,
    header: &Header,
    voice: &Voice,
    ctx: StaffCtx,
) -> (usize, usize, f64) {
    let font = font_cache();
    let sp = ctx.sp;
    let mut cursor_x: f64 = 0.0;

    // 1. Staff lines (placeholder x2 — fixed later by `engrave()` and the
    //    final-barline emission below).
    let line_start = elements.len();
    for i in 0..5 {
        let y = ctx.y_at(i as f64);
        elements.push(EngravingElement::Line {
            x1: 0.0,
            y1: y,
            x2: 0.0,
            y2: y,
            width: 0.5,
            source_span: (0, 0),
        });
    }
    let line_end = line_start + 5;

    // 2. Clef glyph.
    if let Some((cp, line_pos)) = clef_glyph(ctx.clef) {
        if font.glyph_path(cp).is_some() {
            elements.push(EngravingElement::Glyph {
                codepoint: cp,
                x: cursor_x,
                y: ctx.y_at(line_pos),
                scale: ctx.scale,
                source_span: (0, 0),
            });
        }
    }
    cursor_x += sp * 3.5;

    // 3. Key signature, clef-aware.
    let (count, is_sharp) = key_signature_info(&header.key);
    let acc_codepoint = if is_sharp { 0xE262u32 } else { 0xE260u32 };
    let placements = if is_sharp {
        sharp_octaves(ctx.clef)
    } else {
        flat_octaves(ctx.clef)
    };
    for i in 0..count as usize {
        if i >= 7 {
            break;
        }
        let (note, oct) = placements[i];
        let pos = ctx.pos_for(&note, oct);
        elements.push(EngravingElement::Glyph {
            codepoint: acc_codepoint,
            x: cursor_x,
            y: ctx.y_at(pos),
            scale: ctx.scale,
            source_span: (0, 0),
        });
        cursor_x += sp;
    }
    if count > 0 {
        cursor_x += sp * 0.5;
    }

    // 4. Time signature.
    if let Some(meter) = &header.meter {
        let (num, den) = meter.to_fraction();
        emit_time_sig_digit(elements, num, cursor_x, ctx.y_at(1.0), ctx.scale);
        emit_time_sig_digit(elements, den, cursor_x, ctx.y_at(3.0), ctx.scale);
        cursor_x += sp * 2.5;
    }

    cursor_x += sp * 0.5; // padding before first note

    // 5. Walk voice elements.
    let unit_length = header.unit_length.unwrap_or_default();
    let unit_width = sp * 2.5;

    // Open volta we haven't closed yet: (x_start, label_text). Closed at
    // the next barline (any variant) by `close_volta_bracket`.
    let mut open_volta: Option<(f64, String)> = None;
    // Tie pending: last note had tie=true and is waiting for a same-pitch
    // partner. Cleared after the next note (used or not).
    let mut pending_tie: Option<NoteAnchor> = None;
    // Slur stack: each entry is None until the first note after the open
    // is rendered, then becomes Some(anchor). Pop on slur close.
    let mut slur_stack: Vec<Option<NoteAnchor>> = Vec::new();
    // The most recently-rendered note — needed to close a slur from its
    // last note, and to compute tie geometry when the prior note set tie.
    let mut last_anchor: Option<NoteAnchor> = None;
    // Decorations seen as standalone `Element::Decoration` since the last
    // note. Drained onto the next note's anchor.
    let mut pending_decorations: Vec<Decoration> = Vec::new();
    // Note anchors accumulated since the last w: line. Drained when an
    // Element::Lyrics{aligned:true} is encountered.
    let mut lyric_anchors: Vec<NoteAnchor> = Vec::new();

    // Walk a flattened token stream so nested `Element::SlurGroup`s map
    // back to the explicit Start/End boundaries the slur_stack expects.
    let tokens = flatten_for_layout(&voice.elements);
    for token in tokens {
        let element: &Element = match token {
            LayoutToken::Real(e) => e,
            LayoutToken::SlurStart => {
                slur_stack.push(None);
                continue;
            }
            LayoutToken::SlurEnd => {
                if let Some(Some(start)) = slur_stack.pop() {
                    if let Some(end) = last_anchor {
                        emit_tie_or_slur(elements, start, end, ctx, /*is_tie=*/ false);
                    }
                }
                // SlurEnd with no matching open (or empty group): drop
                // silently. The parser already warns on unbalanced slurs.
                continue;
            }
        };
        match element {
            Element::Note(note) => {
                let span = (0usize, 0usize);
                let x_left = cursor_x;
                let pos = ctx.pos_for(&note.pitch, note.octave);
                let y = ctx.y_at(pos);
                let cp = notehead_codepoint(&note.duration, &unit_length);
                let notehead_width =
                    font.glyph_advance(cp).unwrap_or(500.0) * ctx.scale;
                let anchor = NoteAnchor {
                    x_left,
                    x_right: x_left + notehead_width,
                    y,
                    pos,
                    pitch: note.pitch,
                    octave: note.octave,
                };

                // Resolve any pending tie before the new notehead lands.
                if let Some(prev) = pending_tie.take() {
                    if prev.pitch == anchor.pitch && prev.octave == anchor.octave {
                        emit_tie_or_slur(elements, prev, anchor, ctx, /*is_tie=*/ true);
                    }
                }
                // Bind any pending slur opens to this note's anchor.
                for slot in slur_stack.iter_mut() {
                    if slot.is_none() {
                        *slot = Some(anchor);
                    }
                }

                cursor_x = emit_note(elements, note, cursor_x, ctx, unit_width, &unit_length, span);

                // Drain pending standalone decorations onto this note,
                // then add the note's own decorations.
                if !pending_decorations.is_empty() || !note.decorations.is_empty() {
                    let mut decos = std::mem::take(&mut pending_decorations);
                    decos.extend(note.decorations.iter().cloned());
                    emit_decorations_for_note(elements, &decos, anchor, ctx);
                }

                last_anchor = Some(anchor);
                lyric_anchors.push(anchor);
                if note.tie {
                    pending_tie = Some(anchor);
                }
            }
            Element::Lyrics { aligned: true, text } => {
                emit_aligned_lyrics(elements, text, &lyric_anchors, ctx);
                lyric_anchors.clear();
            }
            Element::Decoration(d) => {
                pending_decorations.push(d.clone());
            }
            Element::GraceNotes {
                acciaccatura,
                notes,
            } => {
                cursor_x = emit_grace_notes(elements, notes, *acciaccatura, cursor_x, ctx);
            }
            // SlurGroup is flattened to SlurStart/SlurEnd tokens before
            // this match runs, so it should never reach here.
            Element::SlurGroup { .. } => unreachable!(
                "SlurGroup should have been flattened by flatten_for_layout"
            ),
            Element::Chord(chord) => {
                let span = (0usize, 0usize);
                let dur_width = duration_to_width(&chord.duration, unit_width);

                for note in &chord.notes {
                    let pos = ctx.pos_for(&note.pitch, note.octave);
                    let cp = notehead_codepoint(&chord.duration, &unit_length);
                    elements.push(EngravingElement::Glyph {
                        codepoint: cp,
                        x: cursor_x,
                        y: ctx.y_at(pos),
                        scale: ctx.scale,
                        source_span: span,
                    });
                    emit_ledger_lines(elements, pos, cursor_x, ctx, span);
                }

                // Stem on the chord (highest to lowest note).
                if absolute_ratio(&chord.duration, &unit_length) < 1.0 {
                    if let (Some(first), Some(last)) = (chord.notes.first(), chord.notes.last()) {
                        let top_pos = ctx.pos_for(&first.pitch, first.octave);
                        let bot_pos = ctx.pos_for(&last.pitch, last.octave);
                        let (stem_top, stem_bot) = if top_pos < bot_pos {
                            (top_pos, bot_pos)
                        } else {
                            (bot_pos, top_pos)
                        };
                        let cp = notehead_codepoint(&chord.duration, &unit_length);
                        let nw = font.glyph_advance(cp).unwrap_or(500.0) * ctx.scale;
                        let avg_pos = (stem_top + stem_bot) / 2.0;
                        let stem_x = if avg_pos <= 2.0 {
                            cursor_x
                        } else {
                            cursor_x + nw
                        };
                        let stem_dir = if avg_pos <= 2.0 { 1.0 } else { -1.0 };
                        elements.push(EngravingElement::Line {
                            x1: stem_x,
                            y1: ctx.y_at(stem_top),
                            x2: stem_x,
                            y2: ctx.y_at(stem_bot) + stem_dir * sp * 3.5,
                            width: 0.8,
                            source_span: span,
                        });
                    }
                }
                cursor_x += dur_width;
            }
            Element::Rest(rest) => {
                let span = (0usize, 0usize);
                let cp = rest_codepoint(&rest.duration, &unit_length);
                elements.push(EngravingElement::Glyph {
                    codepoint: cp,
                    x: cursor_x,
                    y: ctx.y_at(2.0),
                    scale: ctx.scale,
                    source_span: span,
                });
                let dur_width = if let Some(bars) = rest.multi_measure {
                    unit_width * bars as f64 * 4.0
                } else {
                    duration_to_width(&rest.duration, unit_width)
                };
                cursor_x += dur_width;
            }
            Element::Bar(bar) => {
                // A bar of any variant closes an open volta.
                close_volta_bracket(elements, &mut open_volta, cursor_x, ctx);
                let bar_left = cursor_x;
                cursor_x = emit_barline(elements, bar, cursor_x, ctx);
                // If this bar is itself a volta opener, start a new bracket.
                if let Some(label) = volta_label(bar) {
                    emit_volta_open(elements, bar_left, &label, ctx);
                    open_volta = Some((bar_left, label));
                }
            }
            Element::Tuplet(tuplet) => {
                let span = (0usize, 0usize);
                let scale_factor = tuplet.q as f64 / tuplet.p as f64;
                for elem in &tuplet.elements {
                    if let Element::Note(note) = elem {
                        let orig_width = duration_to_width(&note.duration, unit_width);
                        let scaled_width = orig_width * scale_factor;
                        let pos = ctx.pos_for(&note.pitch, note.octave);
                        let cp = notehead_codepoint(&note.duration, &unit_length);
                        elements.push(EngravingElement::Glyph {
                            codepoint: cp,
                            x: cursor_x,
                            y: ctx.y_at(pos),
                            scale: ctx.scale,
                            source_span: span,
                        });
                        emit_ledger_lines(elements, pos, cursor_x, ctx, span);
                        emit_stem(elements, pos, cursor_x, ctx, &note.duration, &unit_length, span);
                        cursor_x += scaled_width;
                    }
                }
            }
            Element::ChordSymbol(text) => {
                elements.push(EngravingElement::Text {
                    content: text.clone(),
                    x: cursor_x,
                    y: ctx.y_at(0.0) - sp * 0.5,
                    size: sp * 1.2,
                    source_span: (0, 0),
                });
            }
            // Space, LineBreak, decorations, slurs, lyrics, etc. — defer.
            _ => {}
        }
    }

    // Close any volta that was still open at end-of-voice.
    close_volta_bracket(elements, &mut open_volta, cursor_x, ctx);

    // Auto-emitted thick final barline — only if the body didn't already
    // end on a bar. Avoids double-drawing when the user wrote `|]`/`:|`/etc.
    let ends_with_bar = matches!(voice.elements.last(), Some(Element::Bar(_)));
    if !ends_with_bar {
        elements.push(EngravingElement::Line {
            x1: cursor_x,
            y1: ctx.y_at(0.0),
            x2: cursor_x,
            y2: ctx.y_at(4.0),
            width: 2.0,
            source_span: (0, 0),
        });
    }

    // Initial pass: extend staff lines to current cursor. `engrave()`
    // re-extends them to the widest voice afterward.
    for elem in &mut elements[line_start..line_end] {
        if let EngravingElement::Line { x2, .. } = elem {
            *x2 = cursor_x;
        }
    }

    (line_start, line_end, cursor_x)
}

// --- Lyrics -----------------------------------------------------------------

/// A lyric token from a `w:` line. Per spec §17, syllables map to notes
/// in order; `Skip` advances the note pointer without emitting text.
enum LyricToken {
    /// A syllable to draw under a note. Hyphenated words emit the leading
    /// syllable(s) with a trailing `-`.
    Syl(String),
    /// `*` (skip) or `_` (extend previous). Both advance the note pointer
    /// without drawing new text — the visual difference is left to a
    /// future syllable-extension pass.
    Skip,
}

/// Tokenise a `w:` line into syllables and skips. Whitespace and `-`
/// separate syllables; `-` adds a hyphen suffix to the preceding
/// syllable so it renders as `hel-`. `*` and `_` become Skip. `|` and
/// `~` are tolerated (`|` ignored, `~` collapses to a space inside the
/// current syllable).
fn tokenize_lyrics(text: &str) -> Vec<LyricToken> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                if !current.is_empty() {
                    tokens.push(LyricToken::Syl(std::mem::take(&mut current)));
                }
            }
            '-' => {
                // End the current syllable with a trailing hyphen marker.
                current.push('-');
                tokens.push(LyricToken::Syl(std::mem::take(&mut current)));
            }
            '*' | '_' => {
                if !current.is_empty() {
                    tokens.push(LyricToken::Syl(std::mem::take(&mut current)));
                }
                tokens.push(LyricToken::Skip);
            }
            '|' => {
                if !current.is_empty() {
                    tokens.push(LyricToken::Syl(std::mem::take(&mut current)));
                }
                // `|` syncs to the next bar; for v1 we just treat it as
                // a delimiter without further alignment logic.
            }
            '~' => {
                // Join words under a single note — write as space.
                if !current.is_empty() {
                    current.push(' ');
                }
            }
            '\\' => {
                // Soft hyphen `\-` — consume the next `-` literally.
                if chars.peek() == Some(&'-') {
                    chars.next();
                    current.push('-');
                } else {
                    current.push(c);
                }
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(LyricToken::Syl(current));
    }
    tokens
}

/// Emit per-syllable Text elements under their assigned note anchors.
fn emit_aligned_lyrics(
    elements: &mut Vec<EngravingElement>,
    text: &str,
    anchors: &[NoteAnchor],
    ctx: StaffCtx,
) {
    let tokens = tokenize_lyrics(text);
    let mut note_idx = 0;
    // Place lyrics below the lowest note in the line so they don't
    // overlap a low-tessitura phrase or ledger lines below the staff.
    let lowest_y = anchors
        .iter()
        .map(|a| a.y)
        .fold(ctx.y_at(4.0), f64::max);
    let y_lyric = lowest_y + ctx.sp * 2.5;
    let size = ctx.sp * 1.1;
    for tok in tokens {
        if note_idx >= anchors.len() {
            break;
        }
        let anchor = anchors[note_idx];
        match tok {
            LyricToken::Syl(text) => {
                elements.push(EngravingElement::Text {
                    content: text,
                    x: anchor.x_left,
                    y: y_lyric,
                    size,
                    source_span: (0, 0),
                });
                note_idx += 1;
            }
            LyricToken::Skip => {
                note_idx += 1;
            }
        }
    }
}

// --- Grace notes ------------------------------------------------------------

/// Render a `{...}` grace group at smaller scale. Returns the new cursor_x
/// after the grace prefix. Acciaccatura adds a diagonal slash through the
/// first grace note's stem.
fn emit_grace_notes(
    elements: &mut Vec<EngravingElement>,
    notes: &[Note],
    acciaccatura: bool,
    cursor_x: f64,
    ctx: StaffCtx,
) -> f64 {
    if notes.is_empty() {
        return cursor_x;
    }
    let font = font_cache();
    let grace_scale = ctx.scale * 0.6;
    let grace_step = ctx.sp * 1.0;
    let notehead_cp = 0xE0A4u32; // filled
    let advance = font.glyph_advance(notehead_cp).unwrap_or(500.0) * grace_scale;

    let mut x = cursor_x;
    let mut first_grace_anchor: Option<(f64, f64)> = None;

    for note in notes {
        let pos = note_to_staff_position(&note.pitch, note.octave, ctx.clef);
        let y = ctx.y_at(pos);

        // Accidental in front of the grace head, also small.
        if let Some(acc) = note.accidental {
            let acc_cp = match acc {
                Accidental::Sharp | Accidental::DoubleSharp => 0xE262,
                Accidental::Flat | Accidental::DoubleFlat => 0xE260,
                Accidental::Natural => 0xE261,
            };
            elements.push(EngravingElement::Glyph {
                codepoint: acc_cp,
                x: x - ctx.sp * 0.5,
                y,
                scale: grace_scale,
                source_span: (0, 0),
            });
        }

        elements.push(EngravingElement::Glyph {
            codepoint: notehead_cp,
            x,
            y,
            scale: grace_scale,
            source_span: (0, 0),
        });

        // Stem: always up for grace notes (a short stem from the right side).
        let stem_x = x + advance;
        let stem_top_y = y - ctx.sp * 2.0;
        elements.push(EngravingElement::Line {
            x1: stem_x,
            y1: y,
            x2: stem_x,
            y2: stem_top_y,
            width: 0.6,
            source_span: (0, 0),
        });

        if first_grace_anchor.is_none() {
            first_grace_anchor = Some((stem_x, y));
        }

        x += grace_step;
    }

    if acciaccatura {
        if let Some((stem_x, y)) = first_grace_anchor {
            // Slash crossing the stem at roughly mid-stem height.
            let slash_half = ctx.sp * 0.5;
            elements.push(EngravingElement::Line {
                x1: stem_x - slash_half,
                y1: y - ctx.sp * 0.5,
                x2: stem_x + slash_half,
                y2: y - ctx.sp * 1.7,
                width: 0.7,
                source_span: (0, 0),
            });
        }
    }

    // Leave a small gap between the grace group and the principal note.
    x + ctx.sp * 0.3
}

// --- Decorations ------------------------------------------------------------

/// SMuFL codepoint for a decoration glyph, when one exists in Bravura.
/// Decorations without a glyph (`Other`, `Crescendo`, `Diminuendo` hairpins)
/// return None — the caller decides whether to render text or skip.
fn decoration_glyph(deco: &Decoration) -> Option<u32> {
    match deco {
        Decoration::Staccato => Some(0xE4A2),
        Decoration::Accent => Some(0xE4A0),
        Decoration::Fermata => Some(0xE4C0),
        Decoration::Trill => Some(0xE566),
        // Roll/Mordent/Turn — use ornament glyphs that exist in Bravura.
        Decoration::Roll => Some(0xE566),         // Trill-like; the Irish roll has no
                                                  // dedicated SMuFL glyph.
        Decoration::Mordent { upper: false } => Some(0xE56C),
        Decoration::Mordent { upper: true } => Some(0xE56D),
        Decoration::Turn => Some(0xE567),
        Decoration::UpBow => Some(0xE612),
        Decoration::DownBow => Some(0xE610),
        Decoration::Dynamic(d) => Some(match d {
            Dynamic::PPP => 0xE52A,
            Dynamic::PP => 0xE52B,
            Dynamic::P => 0xE520,
            Dynamic::MP => 0xE52C,
            Dynamic::MF => 0xE52D,
            Dynamic::F => 0xE522,
            Dynamic::FF => 0xE52F,
            Dynamic::FFF => 0xE530,
        }),
        Decoration::Crescendo { .. } | Decoration::Diminuendo { .. } => None,
        Decoration::Other(_) => None,
    }
}

/// True if the decoration is a dynamic mark — those always render below
/// the staff regardless of stem direction.
fn is_dynamic(deco: &Decoration) -> bool {
    matches!(deco, Decoration::Dynamic(_))
}

/// Emit all decorations attached to a note. `decos` is the buffered list
/// of standalone `Element::Decoration` items collected immediately before
/// this note, concatenated with the note's own `.decorations`. The first
/// dynamic stacks below the staff; non-dynamic decorations stack on the
/// side opposite the stem.
fn emit_decorations_for_note(
    elements: &mut Vec<EngravingElement>,
    decos: &[Decoration],
    anchor: NoteAnchor,
    ctx: StaffCtx,
) {
    let font = font_cache();
    let sp = ctx.sp;
    // Dynamics below the staff; their y stacks downward.
    let mut dyn_y = ctx.y_at(4.0) + sp * 2.0;
    // Above-note decorations: stack going up from a position above the
    // higher of (note position, staff top).
    let above_origin = ctx.y_at(anchor.pos.min(0.0)) - sp * 1.5;
    // Below-note decorations: stack going down from below the lower of
    // (note position, staff bottom).
    let below_origin = ctx.y_at(anchor.pos.max(4.0)) + sp * 1.5;
    let mut above_y = above_origin;
    let mut below_y = below_origin;
    // Side: stem-down (pos ≤ 2.0) → decorations above; stem-up → below.
    let above_side = anchor.pos <= 2.0;

    for deco in decos {
        let Some(cp) = decoration_glyph(deco) else {
            continue;
        };
        if font.glyph_path(cp).is_none() {
            continue;
        }
        let (x, y) = if is_dynamic(deco) {
            let y = dyn_y;
            dyn_y += sp * 1.4;
            (anchor.x_left, y)
        } else if above_side {
            let y = above_y;
            above_y -= sp * 1.2;
            (anchor.x_left, y)
        } else {
            let y = below_y;
            below_y += sp * 1.2;
            (anchor.x_left, y)
        };
        elements.push(EngravingElement::Glyph {
            codepoint: cp,
            x,
            y,
            scale: ctx.scale,
            source_span: (0, 0),
        });
    }
}

// --- Tie / slur geometry ----------------------------------------------------

/// Geometric anchor for one rendered note. Used to draw ties (note-to-note
/// curves of the same pitch) and slurs (curves over arbitrary note groups).
#[derive(Clone, Copy)]
struct NoteAnchor {
    x_left: f64,
    x_right: f64,
    y: f64,
    pos: f64,
    pitch: NoteName,
    octave: i8,
}

/// Draw a filled-lens curve between two note anchors. Used for both ties
/// (`is_tie = true`, narrower endpoints) and slurs (`is_tie = false`,
/// curve from outer edges of the span).
fn emit_tie_or_slur(
    elements: &mut Vec<EngravingElement>,
    start: NoteAnchor,
    end: NoteAnchor,
    ctx: StaffCtx,
    is_tie: bool,
) {
    let sp = ctx.sp;

    // Direction is opposite to stem: pos ≤ 2.0 means top half of staff →
    // stem down → curve above (dir = -1); else stem up → curve below.
    let avg_pos = (start.pos + end.pos) / 2.0;
    let dir = if avg_pos <= 2.0 { -1.0 } else { 1.0 };
    let offset_from_note = sp * 0.5;
    // Slurs are usually slightly deeper than ties because they span more.
    let span_x = (end.x_left - start.x_right).abs().max(sp);
    let depth = if is_tie {
        sp * 0.7
    } else {
        sp * 0.7 + span_x * 0.05
    };
    let thickness = sp * 0.16;

    // Curve endpoints: for ties, hug the noteheads' inside edges; for
    // slurs, span from start's left edge to end's right edge so the
    // curve clearly arches over the group.
    let (x_a, x_b) = if is_tie {
        (start.x_right, end.x_left)
    } else {
        (start.x_left, end.x_right)
    };
    let y_a = start.y + dir * offset_from_note;
    let y_b = end.y + dir * offset_from_note;
    let mid_x = (x_a + x_b) / 2.0;
    let mid_y_outer = (y_a + y_b) / 2.0 + dir * depth;
    let mid_y_inner = mid_y_outer - dir * thickness;

    // Lens shape: outer arc then inner arc back to start.
    let d = format!(
        "M {:.3} {:.3} Q {:.3} {:.3} {:.3} {:.3} Q {:.3} {:.3} {:.3} {:.3} Z",
        x_a,
        y_a,
        mid_x,
        mid_y_outer,
        x_b,
        y_b,
        mid_x,
        mid_y_inner,
        x_a,
        y_a,
    );
    elements.push(EngravingElement::Path {
        d,
        fill: true,
        source_span: (0, 0),
    });
}

// --- Barlines & voltas ------------------------------------------------------

const BAR_THIN: f64 = 1.0;
const BAR_THICK: f64 = 3.0;

/// Draw a single full-height vertical line at `x` with the given stroke width.
fn vertical_bar(elements: &mut Vec<EngravingElement>, x: f64, width: f64, ctx: StaffCtx) {
    elements.push(EngravingElement::Line {
        x1: x,
        y1: ctx.y_at(0.0),
        x2: x,
        y2: ctx.y_at(4.0),
        width,
        source_span: (0, 0),
    });
}

/// Two filled dots in the second and third spaces (between staff lines
/// 1-2 and 3-4) at horizontal position `x`. Used for `:|` / `|:` / `::`.
fn emit_repeat_dots(elements: &mut Vec<EngravingElement>, x: f64, ctx: StaffCtx) {
    let r = ctx.sp * 0.18;
    for pos in [1.5_f64, 2.5_f64] {
        let cy = ctx.y_at(pos);
        // SVG circle via two arc halves: M (x-r) cy A r,r 0 1,0 (x+r),cy A r,r 0 1,0 (x-r),cy Z
        let d = format!(
            "M {:.3} {:.3} A {:.3} {:.3} 0 1 0 {:.3} {:.3} A {:.3} {:.3} 0 1 0 {:.3} {:.3} Z",
            x - r,
            cy,
            r,
            r,
            x + r,
            cy,
            r,
            r,
            x - r,
            cy,
        );
        elements.push(EngravingElement::Path {
            d,
            fill: true,
            source_span: (0, 0),
        });
    }
}

/// Render a barline (any variant) and return the new cursor_x position.
fn emit_barline(
    elements: &mut Vec<EngravingElement>,
    bar: &Bar,
    cursor_x: f64,
    ctx: StaffCtx,
) -> f64 {
    let sp = ctx.sp;
    let gap = sp * 0.3; // space between adjacent lines / dots
    match bar {
        // FirstEnding / NthEnding open a volta; they only draw a thin
        // barline themselves — the bracket is drawn by `emit_volta_open`.
        Bar::Single | Bar::FirstEnding | Bar::NthEnding(_) => {
            vertical_bar(elements, cursor_x, BAR_THIN, ctx);
            cursor_x + sp
        }
        Bar::Double => {
            vertical_bar(elements, cursor_x, BAR_THIN, ctx);
            vertical_bar(elements, cursor_x + gap, BAR_THIN, ctx);
            cursor_x + sp + gap
        }
        Bar::End => {
            // thin then thick (left → right)
            vertical_bar(elements, cursor_x, BAR_THIN, ctx);
            vertical_bar(elements, cursor_x + gap + BAR_THICK * 0.5, BAR_THICK, ctx);
            cursor_x + sp + gap + BAR_THICK
        }
        Bar::Start => {
            // thick then thin (left → right)
            vertical_bar(elements, cursor_x + BAR_THICK * 0.5, BAR_THICK, ctx);
            vertical_bar(elements, cursor_x + BAR_THICK + gap, BAR_THIN, ctx);
            cursor_x + sp + gap + BAR_THICK
        }
        Bar::RepeatStart => {
            // |: → thick + thin + dots
            let x_thick = cursor_x + BAR_THICK * 0.5;
            vertical_bar(elements, x_thick, BAR_THICK, ctx);
            let x_thin = x_thick + BAR_THICK * 0.5 + gap;
            vertical_bar(elements, x_thin, BAR_THIN, ctx);
            let x_dots = x_thin + sp * 0.45;
            emit_repeat_dots(elements, x_dots, ctx);
            x_dots + sp * 0.6
        }
        // RepeatEnd and SecondEnding share the visual shape :|
        // SecondEnding additionally opens a volta bracket — handled by the
        // caller via `volta_label`.
        Bar::RepeatEnd | Bar::SecondEnding => {
            // :| → dots + thin + thick
            let x_dots = cursor_x + sp * 0.2;
            emit_repeat_dots(elements, x_dots, ctx);
            let x_thin = x_dots + sp * 0.45;
            vertical_bar(elements, x_thin, BAR_THIN, ctx);
            let x_thick = x_thin + gap + BAR_THICK * 0.5;
            vertical_bar(elements, x_thick, BAR_THICK, ctx);
            x_thick + BAR_THICK * 0.5 + sp * 0.3
        }
        Bar::RepeatBoth => {
            // :: → dots + thin + thick + thin + dots
            let x_dots1 = cursor_x + sp * 0.2;
            emit_repeat_dots(elements, x_dots1, ctx);
            let x_thin1 = x_dots1 + sp * 0.45;
            vertical_bar(elements, x_thin1, BAR_THIN, ctx);
            let x_thick = x_thin1 + gap + BAR_THICK * 0.5;
            vertical_bar(elements, x_thick, BAR_THICK, ctx);
            let x_thin2 = x_thick + BAR_THICK * 0.5 + gap;
            vertical_bar(elements, x_thin2, BAR_THIN, ctx);
            let x_dots2 = x_thin2 + sp * 0.45;
            emit_repeat_dots(elements, x_dots2, ctx);
            x_dots2 + sp * 0.6
        }
    }
}

/// If the given bar opens a volta, return the label text ("1", "2", "1-3,5").
/// Returns None for non-volta bars.
fn volta_label(bar: &Bar) -> Option<String> {
    match bar {
        Bar::FirstEnding => Some("1".to_string()),
        Bar::SecondEnding => Some("2".to_string()),
        Bar::NthEnding(nums) => Some(format_nth_label(nums)),
        _ => None,
    }
}

/// Render the numeric list from `[1,3,5-7` etc. back into a compact label
/// like `1,3,5-7`. Bracket-form `[1-3` keeps its dash; `[1,3` keeps the
/// comma. The parser passes us the numbers in source order.
fn format_nth_label(nums: &[u8]) -> String {
    // The parser stores the raw numbers without separators, so we need a
    // heuristic to display them. Use `-` to join a contiguous run (auto-
    // detected) and `,` between runs. This matches how players read voltas.
    if nums.is_empty() {
        return String::new();
    }
    let mut runs: Vec<(u8, u8)> = Vec::new(); // (start, end) inclusive
    for &n in nums {
        if let Some(last) = runs.last_mut() {
            if n == last.1 + 1 {
                last.1 = n;
                continue;
            }
        }
        runs.push((n, n));
    }
    runs.iter()
        .map(|(a, b)| if a == b { a.to_string() } else { format!("{}-{}", a, b) })
        .collect::<Vec<_>>()
        .join(",")
}

/// Draw the opening hook + label of a volta bracket. The horizontal line is
/// drawn later by `close_volta_bracket` once the closing bar's x is known.
fn emit_volta_open(
    elements: &mut Vec<EngravingElement>,
    x_start: f64,
    label: &str,
    ctx: StaffCtx,
) {
    let sp = ctx.sp;
    let y_bracket = ctx.y_at(0.0) - sp * 2.5;
    // Left hook: short vertical from the bracket down toward the staff top.
    elements.push(EngravingElement::Line {
        x1: x_start,
        y1: y_bracket,
        x2: x_start,
        y2: y_bracket + sp * 0.9,
        width: 0.8,
        source_span: (0, 0),
    });
    // Label text just inside the hook.
    elements.push(EngravingElement::Text {
        content: label.to_string(),
        x: x_start + sp * 0.4,
        y: y_bracket + sp * 1.1,
        size: sp * 1.1,
        source_span: (0, 0),
    });
}

/// Close any open volta bracket by drawing the horizontal line from its
/// start_x to the current cursor_x.
fn close_volta_bracket(
    elements: &mut Vec<EngravingElement>,
    open: &mut Option<(f64, String)>,
    cursor_x: f64,
    ctx: StaffCtx,
) {
    if let Some((x_start, _label)) = open.take() {
        let sp = ctx.sp;
        let y_bracket = ctx.y_at(0.0) - sp * 2.5;
        elements.push(EngravingElement::Line {
            x1: x_start,
            y1: y_bracket,
            x2: cursor_x,
            y2: y_bracket,
            width: 0.8,
            source_span: (0, 0),
        });
    }
}

// --- Element emission -------------------------------------------------------

fn emit_note(
    elements: &mut Vec<EngravingElement>,
    note: &Note,
    cursor_x: f64,
    ctx: StaffCtx,
    unit_width: f64,
    unit: &UnitLength,
    span: SourceSpan,
) -> f64 {
    let pos = ctx.pos_for(&note.pitch, note.octave);
    let y = ctx.y_at(pos);

    if let Some(acc) = note.accidental {
        let acc_cp = match acc {
            Accidental::Sharp | Accidental::DoubleSharp => 0xE262,
            Accidental::Flat | Accidental::DoubleFlat => 0xE260,
            Accidental::Natural => 0xE261,
        };
        elements.push(EngravingElement::Glyph {
            codepoint: acc_cp,
            x: cursor_x - ctx.sp * 0.8,
            y,
            scale: ctx.scale,
            source_span: span,
        });
    }

    let cp = notehead_codepoint(&note.duration, unit);
    elements.push(EngravingElement::Glyph {
        codepoint: cp,
        x: cursor_x,
        y,
        scale: ctx.scale,
        source_span: span,
    });

    emit_ledger_lines(elements, pos, cursor_x, ctx, span);
    emit_stem(elements, pos, cursor_x, ctx, &note.duration, unit, span);
    emit_flag(elements, pos, cursor_x, ctx, &note.duration, unit, span);

    cursor_x + duration_to_width(&note.duration, unit_width)
}

fn absolute_ratio(duration: &Duration, unit: &UnitLength) -> f64 {
    (duration.numerator as f64 * unit.numerator as f64)
        / (duration.denominator as f64 * unit.denominator as f64)
}

fn notehead_codepoint(duration: &Duration, unit: &UnitLength) -> u32 {
    let abs = absolute_ratio(duration, unit);
    if abs >= 1.0 {
        0xE0A2
    } else if abs >= 0.5 {
        0xE0A3
    } else {
        0xE0A4
    }
}

fn rest_codepoint(duration: &Duration, unit: &UnitLength) -> u32 {
    let abs = absolute_ratio(duration, unit);
    if abs >= 1.0 {
        0xE4E3
    } else if abs >= 0.5 {
        0xE4E4
    } else if abs >= 0.25 {
        0xE4E5
    } else if abs >= 0.125 {
        0xE4E6
    } else {
        0xE4E7
    }
}

fn duration_to_width(duration: &Duration, unit_width: f64) -> f64 {
    let ratio = duration.numerator as f64 / duration.denominator as f64;
    (unit_width * ratio).max(unit_width * 0.25)
}

fn emit_ledger_lines(
    elements: &mut Vec<EngravingElement>,
    pos: f64,
    x: f64,
    ctx: StaffCtx,
    span: SourceSpan,
) {
    let ledger_width = ctx.sp * 1.4;
    let lx1 = x - ctx.sp * 0.2;
    let lx2 = lx1 + ledger_width;

    // Above staff (pos < 0)
    let mut lp = -0.5;
    while lp >= pos {
        if (lp * 2.0).round() as i32 % 2 == 0 {
            let y = ctx.y_at(lp);
            elements.push(EngravingElement::Line {
                x1: lx1,
                y1: y,
                x2: lx2,
                y2: y,
                width: 0.5,
                source_span: span,
            });
        }
        lp -= 0.5;
    }

    // Below staff (pos > 4.0)
    let mut lp = 4.5;
    while lp <= pos {
        if (lp * 2.0).round() as i32 % 2 == 0 {
            let y = ctx.y_at(lp);
            elements.push(EngravingElement::Line {
                x1: lx1,
                y1: y,
                x2: lx2,
                y2: y,
                width: 0.5,
                source_span: span,
            });
        }
        lp += 0.5;
    }
}

fn emit_stem(
    elements: &mut Vec<EngravingElement>,
    pos: f64,
    x: f64,
    ctx: StaffCtx,
    duration: &Duration,
    unit: &UnitLength,
    span: SourceSpan,
) {
    let abs = absolute_ratio(duration, unit);
    if abs >= 1.0 {
        return;
    }

    let font = font_cache();
    let cp = notehead_codepoint(duration, unit);
    let notehead_width = font.glyph_advance(cp).unwrap_or(500.0) * ctx.scale;

    let stem_length = ctx.sp * 3.5;
    let note_y = ctx.y_at(pos);

    if pos <= 2.0 {
        let stem_x = x;
        elements.push(EngravingElement::Line {
            x1: stem_x,
            y1: note_y,
            x2: stem_x,
            y2: note_y + stem_length,
            width: 0.8,
            source_span: span,
        });
    } else {
        let stem_x = x + notehead_width;
        elements.push(EngravingElement::Line {
            x1: stem_x,
            y1: note_y,
            x2: stem_x,
            y2: note_y - stem_length,
            width: 0.8,
            source_span: span,
        });
    }
}

fn emit_flag(
    elements: &mut Vec<EngravingElement>,
    pos: f64,
    x: f64,
    ctx: StaffCtx,
    duration: &Duration,
    unit: &UnitLength,
    span: SourceSpan,
) {
    let abs = absolute_ratio(duration, unit);

    let flag_cp = if abs <= 0.0625 {
        if pos <= 2.0 {
            Some(0xE243u32)
        } else {
            Some(0xE242u32)
        }
    } else if abs <= 0.125 {
        if pos <= 2.0 {
            Some(0xE241u32)
        } else {
            Some(0xE240u32)
        }
    } else {
        None
    };

    if let Some(cp) = flag_cp {
        let font = font_cache();
        let notehead_cp = notehead_codepoint(duration, unit);
        let notehead_width = font.glyph_advance(notehead_cp).unwrap_or(500.0) * ctx.scale;

        let stem_length = ctx.sp * 3.5;
        let note_y = ctx.y_at(pos);
        let stem_x = if pos <= 2.0 {
            x
        } else {
            x + notehead_width
        };
        let flag_y = if pos <= 2.0 {
            note_y + stem_length
        } else {
            note_y - stem_length
        };
        elements.push(EngravingElement::Glyph {
            codepoint: cp,
            x: stem_x,
            y: flag_y,
            scale: ctx.scale,
            source_span: span,
        });
    }
}

fn emit_time_sig_digit(
    elements: &mut Vec<EngravingElement>,
    digit: u8,
    x: f64,
    y: f64,
    scale: f64,
) {
    if digit <= 9 {
        elements.push(EngravingElement::Glyph {
            codepoint: 0xE080 + digit as u32,
            x,
            y,
            scale,
            source_span: (0, 0),
        });
    } else {
        let tens = digit / 10;
        let ones = digit % 10;
        let font = font_cache();
        let advance = font.glyph_advance(0xE080).unwrap_or(500.0) * scale;
        elements.push(EngravingElement::Glyph {
            codepoint: 0xE080 + tens as u32,
            x,
            y,
            scale,
            source_span: (0, 0),
        });
        elements.push(EngravingElement::Glyph {
            codepoint: 0xE080 + ones as u32,
            x: x + advance,
            y,
            scale,
            source_span: (0, 0),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn middle_c_is_below_treble_staff() {
        // Middle C (C4) = ABC (C, octave 1) → pos 5.0, one ledger line below
        let pos = note_to_staff_position(&NoteName::C, 1, Clef::Treble);
        assert!((pos - 5.0).abs() < 0.01, "got {}", pos);
    }

    #[test]
    fn b4_is_treble_middle_line() {
        let pos = note_to_staff_position(&NoteName::B, 1, Clef::Treble);
        assert!((pos - 2.0).abs() < 0.01, "got {}", pos);
    }

    #[test]
    fn f5_is_treble_top_line() {
        let pos = note_to_staff_position(&NoteName::F, 2, Clef::Treble);
        assert!((pos - 0.0).abs() < 0.01, "got {}", pos);
    }

    #[test]
    fn e4_is_treble_bottom_line() {
        let pos = note_to_staff_position(&NoteName::E, 1, Clef::Treble);
        assert!((pos - 4.0).abs() < 0.01, "got {}", pos);
    }

    #[test]
    fn d3_is_bass_middle_line() {
        // D3 = (D, octave 0) — uppercase D in ABC.
        let pos = note_to_staff_position(&NoteName::D, 0, Clef::Bass);
        assert!((pos - 2.0).abs() < 0.01, "got {}", pos);
    }

    #[test]
    fn middle_c_is_alto_middle_line() {
        let pos = note_to_staff_position(&NoteName::C, 1, Clef::Alto);
        assert!((pos - 2.0).abs() < 0.01, "got {}", pos);
    }

    #[test]
    fn middle_c_is_tenor_fourth_line() {
        let pos = note_to_staff_position(&NoteName::C, 1, Clef::Tenor);
        assert!((pos - 1.0).abs() < 0.01, "got {}", pos);
    }

    // Notehead/rest/flag tests — unchanged from the pre-clef era. They use
    // the same parse() → engrave() pipeline so any clef regression in the
    // default (treble) path shows up here.

    const WHOLE_NOTEHEAD: u32 = 0xE0A2;
    const HALF_NOTEHEAD: u32 = 0xE0A3;
    const FILLED_NOTEHEAD: u32 = 0xE0A4;

    fn notehead_codepoints(elements: &[EngravingElement]) -> Vec<u32> {
        elements
            .iter()
            .filter_map(|e| match e {
                EngravingElement::Glyph { codepoint, .. }
                    if [WHOLE_NOTEHEAD, HALF_NOTEHEAD, FILLED_NOTEHEAD].contains(codepoint) =>
                {
                    Some(*codepoint)
                }
                _ => None,
            })
            .collect()
    }

    fn has_flag_glyphs(elements: &[EngravingElement]) -> bool {
        elements.iter().any(|e| matches!(e,
            EngravingElement::Glyph { codepoint, .. }
                if (0xE240..=0xE243).contains(codepoint)
        ))
    }

    #[test]
    fn eighth_notes_get_filled_noteheads() {
        let abc = "X:1\nT:Test\nM:4/4\nL:1/8\nK:C\nC D E\n";
        let tune = crate::parse(abc).value.into_iter().next().unwrap();
        let elements = engrave(&tune, &EngravingOptions::default());
        let heads = notehead_codepoints(&elements);
        assert_eq!(heads.len(), 3);
        for &cp in &heads {
            assert_eq!(cp, FILLED_NOTEHEAD);
        }
        assert!(has_flag_glyphs(&elements));
    }

    #[test]
    fn quarter_note_gets_filled_notehead() {
        let abc = "X:1\nT:Test\nM:4/4\nL:1/8\nK:C\nC2\n";
        let tune = crate::parse(abc).value.into_iter().next().unwrap();
        let elements = engrave(&tune, &EngravingOptions::default());
        let heads = notehead_codepoints(&elements);
        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0], FILLED_NOTEHEAD);
        assert!(!has_flag_glyphs(&elements));
    }

    #[test]
    fn half_note_gets_half_notehead() {
        let abc = "X:1\nT:Test\nM:4/4\nL:1/8\nK:C\nC4\n";
        let tune = crate::parse(abc).value.into_iter().next().unwrap();
        let elements = engrave(&tune, &EngravingOptions::default());
        let heads = notehead_codepoints(&elements);
        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0], HALF_NOTEHEAD);
    }

    #[test]
    fn whole_note_gets_whole_notehead() {
        let abc = "X:1\nT:Test\nM:4/4\nL:1/8\nK:C\nC8\n";
        let tune = crate::parse(abc).value.into_iter().next().unwrap();
        let elements = engrave(&tune, &EngravingOptions::default());
        let heads = notehead_codepoints(&elements);
        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0], WHOLE_NOTEHEAD);
    }

    #[test]
    fn quarter_note_with_l14() {
        let abc = "X:1\nT:Test\nM:4/4\nL:1/4\nK:C\nC\n";
        let tune = crate::parse(abc).value.into_iter().next().unwrap();
        let elements = engrave(&tune, &EngravingOptions::default());
        let heads = notehead_codepoints(&elements);
        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0], FILLED_NOTEHEAD);
    }

    #[test]
    fn half_note_with_l14() {
        let abc = "X:1\nT:Test\nM:4/4\nL:1/4\nK:C\nC2\n";
        let tune = crate::parse(abc).value.into_iter().next().unwrap();
        let elements = engrave(&tune, &EngravingOptions::default());
        let heads = notehead_codepoints(&elements);
        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0], HALF_NOTEHEAD);
    }

    #[test]
    fn quarter_rest_with_l18() {
        let abc = "X:1\nT:Test\nM:4/4\nL:1/8\nK:C\nz2\n";
        let tune = crate::parse(abc).value.into_iter().next().unwrap();
        let elements = engrave(&tune, &EngravingOptions::default());
        let rest_cps: Vec<u32> = elements
            .iter()
            .filter_map(|e| match e {
                EngravingElement::Glyph { codepoint, .. }
                    if (0xE4E3..=0xE4E7).contains(codepoint) =>
                {
                    Some(*codepoint)
                }
                _ => None,
            })
            .collect();
        assert_eq!(rest_cps.len(), 1);
        assert_eq!(rest_cps[0], 0xE4E5);
    }
}
