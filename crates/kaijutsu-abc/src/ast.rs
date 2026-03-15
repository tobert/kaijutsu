//! Abstract Syntax Tree types for ABC notation.
//!
//! These types represent the full semantic content of ABC notation,
//! including features that may not yet be supported in MIDI output.

use serde::{Deserialize, Serialize};

/// A complete ABC tune
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tune {
    pub header: Header,
    pub voices: Vec<Voice>,
}

impl Default for Tune {
    fn default() -> Self {
        Tune {
            header: Header::default(),
            voices: vec![Voice::default()],
        }
    }
}

/// Tune header (metadata)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Header {
    pub reference: u32,
    pub title: String,
    pub titles: Vec<String>,
    pub key: Key,
    pub meter: Option<Meter>,
    pub unit_length: Option<UnitLength>,
    pub tempo: Option<Tempo>,
    pub composer: Option<String>,
    pub rhythm: Option<String>,
    pub source: Option<String>,
    pub notes: Option<String>,
    pub voice_defs: Vec<VoiceDef>,
    pub other_fields: Vec<InfoField>,
    /// MIDI program number from %%MIDI program directive (0-127)
    pub midi_program: Option<u8>,
}

impl Default for Header {
    fn default() -> Self {
        Header {
            reference: 1,
            title: String::new(),
            titles: Vec::new(),
            key: Key::default(),
            meter: None,
            unit_length: None,
            tempo: None,
            composer: None,
            rhythm: None,
            source: None,
            notes: None,
            voice_defs: Vec::new(),
            other_fields: Vec::new(),
            midi_program: None,
        }
    }
}

/// Key signature
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Key {
    pub root: NoteName,
    pub accidental: Option<Accidental>,
    pub mode: Mode,
    pub explicit_accidentals: Vec<(Accidental, NoteName)>,
    pub clef: Option<Clef>,
}

impl Default for Key {
    fn default() -> Self {
        Key {
            root: NoteName::C,
            accidental: None,
            mode: Mode::Major,
            explicit_accidentals: Vec::new(),
            clef: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NoteName {
    C,
    D,
    E,
    F,
    G,
    A,
    B,
}

impl NoteName {
    /// Convert to semitone offset from C (0-11)
    pub fn to_semitone(&self) -> i8 {
        match self {
            NoteName::C => 0,
            NoteName::D => 2,
            NoteName::E => 4,
            NoteName::F => 5,
            NoteName::G => 7,
            NoteName::A => 9,
            NoteName::B => 11,
        }
    }

    /// All note names in order
    pub fn all() -> [NoteName; 7] {
        [
            NoteName::C,
            NoteName::D,
            NoteName::E,
            NoteName::F,
            NoteName::G,
            NoteName::A,
            NoteName::B,
        ]
    }

    /// Create from semitone offset (0-11), preferring sharps for chromatic notes
    pub fn from_semitone(semitone: i8) -> (NoteName, Option<Accidental>) {
        match semitone.rem_euclid(12) {
            0 => (NoteName::C, None),
            1 => (NoteName::C, Some(Accidental::Sharp)),
            2 => (NoteName::D, None),
            3 => (NoteName::D, Some(Accidental::Sharp)),
            4 => (NoteName::E, None),
            5 => (NoteName::F, None),
            6 => (NoteName::F, Some(Accidental::Sharp)),
            7 => (NoteName::G, None),
            8 => (NoteName::G, Some(Accidental::Sharp)),
            9 => (NoteName::A, None),
            10 => (NoteName::A, Some(Accidental::Sharp)),
            11 => (NoteName::B, None),
            _ => unreachable!(),
        }
    }

    /// Parse from string (case-insensitive)
    pub fn parse(s: &str) -> Option<NoteName> {
        match s.to_uppercase().as_str() {
            "C" => Some(NoteName::C),
            "D" => Some(NoteName::D),
            "E" => Some(NoteName::E),
            "F" => Some(NoteName::F),
            "G" => Some(NoteName::G),
            "A" => Some(NoteName::A),
            "B" => Some(NoteName::B),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Accidental {
    DoubleSharp,
    Sharp,
    Natural,
    Flat,
    DoubleFlat,
}

impl Accidental {
    /// Convert to semitone offset
    pub fn to_semitone_offset(&self) -> i8 {
        match self {
            Accidental::DoubleSharp => 2,
            Accidental::Sharp => 1,
            Accidental::Natural => 0,
            Accidental::Flat => -1,
            Accidental::DoubleFlat => -2,
        }
    }

    /// Parse from string
    pub fn parse(s: &str) -> Option<Accidental> {
        match s {
            "#" | "^" => Some(Accidental::Sharp),
            "b" | "_" => Some(Accidental::Flat),
            "##" | "^^" => Some(Accidental::DoubleSharp),
            "bb" | "__" => Some(Accidental::DoubleFlat),
            "=" => Some(Accidental::Natural),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Mode {
    #[default]
    Major,
    Minor,
    Ionian,
    Dorian,
    Phrygian,
    Lydian,
    Mixolydian,
    Aeolian,
    Locrian,
}

impl Mode {
    /// Parse mode from string (case-insensitive, allows abbreviations)
    pub fn parse(s: &str) -> Option<Mode> {
        let s = s.to_lowercase();
        match s.as_str() {
            "maj" | "major" | "" => Some(Mode::Major),
            "min" | "minor" | "m" => Some(Mode::Minor),
            "ion" | "ionian" => Some(Mode::Ionian),
            "dor" | "dorian" => Some(Mode::Dorian),
            "phr" | "phrygian" => Some(Mode::Phrygian),
            "lyd" | "lydian" => Some(Mode::Lydian),
            "mix" | "mixolydian" => Some(Mode::Mixolydian),
            "aeo" | "aeolian" => Some(Mode::Aeolian),
            "loc" | "locrian" => Some(Mode::Locrian),
            _ => None,
        }
    }
}

/// Meter/time signature
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Meter {
    Simple { numerator: u8, denominator: u8 },
    Common, // C = 4/4
    Cut,    // C| = 2/2
    None,   // Free meter
}

impl Meter {
    /// Get beats per bar and beat unit for MIDI timing
    pub fn to_fraction(&self) -> (u8, u8) {
        match self {
            Meter::Simple {
                numerator,
                denominator,
            } => (*numerator, *denominator),
            Meter::Common => (4, 4),
            Meter::Cut => (2, 2),
            Meter::None => (4, 4), // Default assumption
        }
    }
}

/// Unit note length (L: field)
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct UnitLength {
    pub numerator: u8,
    pub denominator: u8,
}

impl Default for UnitLength {
    fn default() -> Self {
        UnitLength {
            numerator: 1,
            denominator: 8,
        }
    }
}

/// Tempo (Q: field)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tempo {
    pub beat_unit: (u8, u8), // e.g., (1, 4) for quarter note
    pub bpm: u16,
    pub text: Option<String>,
}

impl Default for Tempo {
    fn default() -> Self {
        Tempo {
            beat_unit: (1, 4),
            bpm: 120,
            text: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Clef {
    #[default]
    Treble,
    Bass,
    Alto,
    Tenor,
    Percussion,
}

/// Generic info field (for fields we don't specifically handle)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InfoField {
    pub field_type: char,
    pub value: String,
}

/// Voice definition from V: field in header
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceDef {
    pub id: String,
    pub name: Option<String>,
    pub clef: Option<Clef>,
    pub octave: Option<i8>,    // Octave transposition
    pub transpose: Option<i8>, // Semitone transposition
    pub stem: Option<StemDirection>,
}

impl VoiceDef {
    pub fn new(id: impl Into<String>) -> Self {
        VoiceDef {
            id: id.into(),
            name: None,
            clef: None,
            octave: None,
            transpose: None,
            stem: None,
        }
    }
}

/// Stem direction for notation
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StemDirection {
    Up,
    Down,
    Auto,
}

/// A voice (track) in the tune
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Voice {
    pub id: Option<String>,
    pub name: Option<String>,
    pub elements: Vec<Element>,
}

/// A music element in the body
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Element {
    Note(Note),
    Chord(Chord),
    Rest(Rest),
    Bar(Bar),
    Tuplet(Tuplet),
    GraceNotes {
        acciaccatura: bool,
        notes: Vec<Note>,
    },
    ChordSymbol(String),
    InlineField(InfoField),
    Decoration(Decoration),
    Slur(SlurBoundary),
    VoiceSwitch(String), // Switch to voice with given ID
    Space,
    LineBreak,
}

/// A single note
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub pitch: NoteName,
    pub octave: i8, // 0 = C-B, 1 = c-b, -1 = C,-B,
    pub accidental: Option<Accidental>,
    pub duration: Duration,
    pub tie: bool,
    pub decorations: Vec<Decoration>,
}

impl Note {
    /// Create a simple note with default duration
    pub fn new(pitch: NoteName, octave: i8) -> Self {
        Note {
            pitch,
            octave,
            accidental: None,
            duration: Duration::default(),
            tie: false,
            decorations: Vec::new(),
        }
    }

    /// Convert to MIDI pitch number (middle C = 60)
    /// Does not account for key signature - caller must handle that
    pub fn to_midi_pitch(&self) -> u8 {
        let base = self.pitch.to_semitone();
        // ABC octave 1 (lowercase c-b) = MIDI 60-71 (middle C octave)
        // So: base + (octave + 4) * 12
        let octave_offset = (self.octave + 4) * 12;
        let acc_offset = self.accidental.map(|a| a.to_semitone_offset()).unwrap_or(0);

        ((base as i16) + (octave_offset as i16) + (acc_offset as i16)).clamp(0, 127) as u8
    }
}

/// Note duration as a ratio of the unit length
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Duration {
    pub numerator: u16,
    pub denominator: u16,
}

impl Duration {
    pub fn new(numerator: u16, denominator: u16) -> Self {
        Duration {
            numerator,
            denominator,
        }
    }

    pub fn unit() -> Self {
        Duration {
            numerator: 1,
            denominator: 1,
        }
    }

    /// Convert to MIDI ticks given ticks per unit note
    pub fn to_ticks(&self, ticks_per_unit: u32) -> u32 {
        (ticks_per_unit * self.numerator as u32) / self.denominator as u32
    }

    /// Multiply duration by an integer
    pub fn multiply(&self, n: u16) -> Self {
        Duration {
            numerator: self.numerator * n,
            denominator: self.denominator,
        }
    }
}

impl Default for Duration {
    fn default() -> Self {
        Self::unit()
    }
}

/// Chord (simultaneous notes)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chord {
    pub notes: Vec<Note>,
    pub duration: Duration,
}

/// Rest
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rest {
    pub duration: Duration,
    pub visible: bool,              // z vs x
    pub multi_measure: Option<u16>, // Z4 = 4 bars
}

impl Rest {
    pub fn new(duration: Duration) -> Self {
        Rest {
            duration,
            visible: true,
            multi_measure: None,
        }
    }

    pub fn invisible(duration: Duration) -> Self {
        Rest {
            duration,
            visible: false,
            multi_measure: None,
        }
    }

    pub fn multi_measure(bars: u16) -> Self {
        Rest {
            duration: Duration::unit(),
            visible: true,
            multi_measure: Some(bars),
        }
    }
}

/// Bar line types
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Bar {
    Single,       // |
    Double,       // ||
    End,          // |]
    Start,        // [|
    RepeatStart,  // |:
    RepeatEnd,    // :|
    RepeatBoth,   // ::
    FirstEnding,  // |1
    SecondEnding, // :|2
    NthEnding(Vec<u8>),
}

/// Tuplet
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tuplet {
    pub p: u8, // p notes
    pub q: u8, // in time of q notes
    pub elements: Vec<Element>,
}

impl Tuplet {
    /// Create a triplet (3 notes in time of 2)
    pub fn triplet(elements: Vec<Element>) -> Self {
        Tuplet {
            p: 3,
            q: 2,
            elements,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlurBoundary {
    Start,
    End,
}

/// Decorations (articulations, ornaments, dynamics)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Decoration {
    Staccato,
    Accent,
    Fermata,
    Trill,
    Roll,
    Mordent { upper: bool },
    Turn,
    UpBow,
    DownBow,
    Dynamic(Dynamic),
    Crescendo { start: bool },
    Diminuendo { start: bool },
    Other(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Dynamic {
    PPP,
    PP,
    P,
    MP,
    MF,
    F,
    FF,
    FFF,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_note_to_midi_pitch() {
        // Middle C (c in ABC, octave 1) should be MIDI 60
        let middle_c = Note::new(NoteName::C, 1);
        assert_eq!(middle_c.to_midi_pitch(), 60);

        // C below middle C (C in ABC, octave 0) should be MIDI 48
        let low_c = Note::new(NoteName::C, 0);
        assert_eq!(low_c.to_midi_pitch(), 48);

        // C an octave above middle C (c' in ABC, octave 2)
        let high_c = Note::new(NoteName::C, 2);
        assert_eq!(high_c.to_midi_pitch(), 72);

        // C, (octave -1) should be MIDI 36
        let very_low_c = Note::new(NoteName::C, -1);
        assert_eq!(very_low_c.to_midi_pitch(), 36);
    }

    #[test]
    fn test_note_with_accidental() {
        let mut c_sharp = Note::new(NoteName::C, 1);
        c_sharp.accidental = Some(Accidental::Sharp);
        assert_eq!(c_sharp.to_midi_pitch(), 61);

        let mut b_flat = Note::new(NoteName::B, 0);
        b_flat.accidental = Some(Accidental::Flat);
        assert_eq!(b_flat.to_midi_pitch(), 58); // Bb below middle C
    }

    #[test]
    fn test_duration_to_ticks() {
        let ticks_per_unit = 480u32;

        // Unit duration
        let unit = Duration::unit();
        assert_eq!(unit.to_ticks(ticks_per_unit), 480);

        // Double duration (A2)
        let double = Duration::new(2, 1);
        assert_eq!(double.to_ticks(ticks_per_unit), 960);

        // Half duration (A/2)
        let half = Duration::new(1, 2);
        assert_eq!(half.to_ticks(ticks_per_unit), 240);

        // Dotted (A3/2)
        let dotted = Duration::new(3, 2);
        assert_eq!(dotted.to_ticks(ticks_per_unit), 720);
    }

    #[test]
    fn test_mode_parse() {
        assert_eq!(Mode::parse("maj"), Some(Mode::Major));
        assert_eq!(Mode::parse("Major"), Some(Mode::Major));
        assert_eq!(Mode::parse("m"), Some(Mode::Minor));
        assert_eq!(Mode::parse("min"), Some(Mode::Minor));
        assert_eq!(Mode::parse("dor"), Some(Mode::Dorian));
        assert_eq!(Mode::parse("Mixolydian"), Some(Mode::Mixolydian));
        assert_eq!(Mode::parse(""), Some(Mode::Major));
        assert_eq!(Mode::parse("invalid"), None);
    }

    #[test]
    fn test_meter_to_fraction() {
        assert_eq!(Meter::Common.to_fraction(), (4, 4));
        assert_eq!(Meter::Cut.to_fraction(), (2, 2));
        assert_eq!(
            Meter::Simple {
                numerator: 6,
                denominator: 8
            }
            .to_fraction(),
            (6, 8)
        );
    }
}
