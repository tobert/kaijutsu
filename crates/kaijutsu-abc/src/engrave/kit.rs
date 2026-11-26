use std::collections::HashMap;

use crate::ast::{Accidental, Key, Note, NoteName};
use crate::midi::{apply_pitch_offset, compute_key_accidentals, effective_accidental, midi_pitch_with_accidental};

pub(super) struct KitPitches {
    key_accidentals: HashMap<NoteName, Accidental>,
    bar_accidentals: HashMap<NoteName, Accidental>,
    tie_carry: HashMap<(NoteName, i8), Accidental>,
    offset: i16,
}

impl KitPitches {
    pub(super) fn new(key: &Key, offset: i16) -> Self {
        let key_accidentals = compute_key_accidentals(key);
        Self { bar_accidentals: key_accidentals.clone(), key_accidentals, tie_carry: HashMap::new(), offset }
    }

    pub(super) fn bar(&mut self) {
        self.bar_accidentals = self.key_accidentals.clone();
    }

    pub(super) fn key(&mut self, key: &Key) {
        self.key_accidentals = compute_key_accidentals(key);
        self.bar();
    }

    pub(super) fn note(&mut self, note: &Note) -> u8 {
        let carried = self.tie_carry.remove(&(note.pitch, note.octave));
        let acc = effective_accidental(note.pitch, note.accidental, &self.bar_accidentals, carried);
        if note.tie && let Some(acc) = acc {
            self.tie_carry.insert((note.pitch, note.octave), acc);
        }
        if let Some(acc) = note.accidental {
            self.bar_accidentals.insert(note.pitch, acc);
        }
        apply_pitch_offset(midi_pitch_with_accidental(note.pitch, note.octave, acc), self.offset)
    }

    pub(super) fn grace(&self, note: &Note) -> u8 {
        let acc = effective_accidental(note.pitch, note.accidental, &self.bar_accidentals, None);
        apply_pitch_offset(midi_pitch_with_accidental(note.pitch, note.octave, acc), self.offset)
    }
}

#[cfg(test)]
mod tests {
    use super::KitPitches;
    use crate::ast::{Accidental, Key, Note, NoteName};

    #[test]
    fn accidentals_follow_bar_and_key_changes() {
        let mut pitches = KitPitches::new(&Key::default(), 0);
        let mut f = Note::new(NoteName::F, -2);
        f.accidental = Some(Accidental::Sharp);
        assert_eq!(pitches.note(&f), 42);
        f.accidental = None;
        assert_eq!(pitches.note(&f), 42);
        pitches.bar();
        assert_eq!(pitches.note(&f), 41);
        let mut key = Key::default();
        key.explicit_accidentals.push((Accidental::Sharp, NoteName::F));
        pitches.key(&key);
        assert_eq!(pitches.note(&f), 42);
    }

    #[test]
    fn tied_accidental_survives_bar_and_grace_does_not_mutate() {
        let mut pitches = KitPitches::new(&Key::default(), 0);
        let mut f = Note::new(NoteName::F, -2);
        f.accidental = Some(Accidental::Sharp);
        f.tie = true;
        assert_eq!(pitches.note(&f), 42);
        pitches.bar();
        f.accidental = None;
        f.tie = false;
        assert_eq!(pitches.grace(&f), 41);
        assert_eq!(pitches.note(&f), 42);
        assert_eq!(pitches.note(&f), 41);
    }
}
