//! The **device-addressed control cue** envelope (`docs/midi-next.md`
//! "`kj midi` — one emit surface", slice 1 step 4).
//!
//! `kj midi send`/`kj midi panic` need to put raw MIDI bytes out a *named
//! device's* port without the kernel ever touching hardware
//! (`docs/midi.md` "Render is a wire cue; the sink owns the hardware"). They
//! ride the SAME [`crate::RenderCue`] wire the score does — a different
//! `mime`, nothing more — because a second transport for "control" traffic
//! would be a second thing to keep in sync with the timebase, the sink
//! lifecycle, and the cue mailbox.
//!
//! ## Why a new mime rather than reusing [`crate::ABC_MIME`]
//!
//! The score path is **port-anonymous**: an ABC cue plays out the sink's one
//! auto-connected render port (`docs/scenes/patchbay.md` slice 1) and carries
//! no device identity at all. A control cue is the opposite — it is
//! *addressed*, and a sink that cannot resolve the address must drop it
//! loudly rather than spray it at whatever the render port happens to be
//! wired to. Those are different contracts, so they get different mimes:
//! [`MIDI_CONTROL_MIME`]. A sink too old to know it ignores it (the mime
//! dispatch's own default), which is the correct behaviour — better silence
//! than a note at the wrong instrument.
//!
//! ## Shape
//!
//! ```json
//! {"v":1,"device":"keystep-pro","events":[{"data":"b00b40"},
//!                                         {"offset_ms":250,"data":"800c00"}]}
//! ```
//!
//! - `device` is the **profile key** (`/etc/midi/devices/<name>`, the same key
//!   `/run/midi/<name>` presence uses). The sink resolves it through its own
//!   match/presence state to a real port address — the kernel never learns
//!   what a sequencer address is.
//! - `events` are complete MIDI messages, already composed by the emitter,
//!   each hex-encoded. **Hex, not a JSON byte array**: half the bytes on the
//!   wire and greppable in a log, which matters most for the one payload that
//!   can get big (SysEx).
//! - `offset_ms` schedules an event *after* the cue's own onset — how a gated
//!   note (`send … note … --off-ms`) carries its NoteOff without a second cue.
//!
//! Deliberately NOT here: profile-resolved control names, relative values,
//! and role-aware port selection. Those are slice 2 — this envelope carries
//! bytes a human or a script already decided on.

use serde::{Deserialize, Serialize};

/// A device-addressed raw control cue (`kj midi send` / `kj midi panic`). A
/// [`crate::RenderCue`] carrying one sets this as its `mime`, always with a
/// `CuePayload::Inline` body and `lead == 0` (onset now).
pub const MIDI_CONTROL_MIME: &str = "application/vnd.kaijutsu.midi-control+json";

/// The control-cue envelope version this build writes and accepts. Per-record
/// versioning, the [`crate::clip::CLIP_VERSION`] precedent.
pub const MIDI_CONTROL_VERSION: u32 = 1;

/// One complete MIDI message and when it fires relative to the cue's onset.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MidiControlEvent {
    /// Milliseconds after the cue's onset. `0` (the default) = with the cue.
    /// Integer because this is wall time at the hardware, not musical time —
    /// a control send has no beat.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub offset_ms: u64,
    /// One complete MIDI message, lowercase hex, no separators
    /// (`"b00b40"` = CC 11 = 64 on channel 1). One message per event: a sink
    /// hands each to its encoder whole, so a truncated or run-on message is a
    /// composition bug caught here, not a wedged device.
    pub data: String,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

impl MidiControlEvent {
    /// Build an event from raw bytes at `offset_ms`.
    pub fn new(offset_ms: u64, bytes: &[u8]) -> Self {
        Self { offset_ms, data: to_hex(bytes) }
    }

    /// Build an event that fires with the cue's onset.
    pub fn now(bytes: &[u8]) -> Self {
        Self::new(0, bytes)
    }

    /// Decode `data` back to bytes. Loud on anything that isn't clean hex —
    /// a half-decoded MIDI message is exactly the kind of quiet corruption
    /// that wedges a synth.
    pub fn bytes(&self) -> Result<Vec<u8>, MidiControlError> {
        from_hex(&self.data)
    }
}

/// The whole envelope: which device, and what to put out its port.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MidiControl {
    /// Envelope version. Must equal [`MIDI_CONTROL_VERSION`].
    pub v: u32,
    /// The device profile key to route to (`/etc/midi/devices/<device>`).
    /// **Never** a port address: the kernel composes this cue and has no idea
    /// what the rig's addresses are, by design.
    pub device: String,
    /// The messages, in emission order.
    pub events: Vec<MidiControlEvent>,
}

/// A control envelope failed to parse or validate. Fail-loud all the way
/// down: a malformed control cue is dropped with a log line naming why, never
/// half-played.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MidiControlError {
    /// The envelope was not valid JSON, or the wrong shape.
    #[error("MIDI control envelope is not valid JSON: {0}")]
    Json(String),
    /// `v` names a version this build does not understand.
    #[error("unknown MIDI control envelope version {0} (this build supports v{MIDI_CONTROL_VERSION})")]
    UnknownVersion(u32),
    /// `device` was empty — an unaddressed control cue has nowhere to go, and
    /// falling back to "the render port" is precisely the mis-delivery this
    /// mime exists to prevent.
    #[error("MIDI control envelope names no device")]
    EmptyDevice,
    /// `events` was empty. A cue that says nothing is a composition bug.
    #[error("MIDI control envelope carries no events")]
    NoEvents,
    /// A `data` field was not clean hex.
    #[error("MIDI control event data '{0}' is not valid hex ({1})")]
    BadHex(String, &'static str),
}

impl MidiControl {
    /// Build an envelope for `device` from already-composed messages.
    pub fn new(device: impl Into<String>, events: Vec<MidiControlEvent>) -> Self {
        Self { v: MIDI_CONTROL_VERSION, device: device.into(), events }
    }

    /// Parse + validate an envelope off the wire (the sink's entry point).
    /// Every `data` field is hex-decoded here so a bad message is refused
    /// before ANY of the cue's messages reach hardware — all or nothing,
    /// never a half-sent phrase.
    pub fn parse(json: &str) -> Result<Self, MidiControlError> {
        let envelope: MidiControl =
            serde_json::from_str(json).map_err(|e| MidiControlError::Json(e.to_string()))?;
        if envelope.v != MIDI_CONTROL_VERSION {
            return Err(MidiControlError::UnknownVersion(envelope.v));
        }
        if envelope.device.trim().is_empty() {
            return Err(MidiControlError::EmptyDevice);
        }
        if envelope.events.is_empty() {
            return Err(MidiControlError::NoEvents);
        }
        for event in &envelope.events {
            event.bytes()?;
        }
        Ok(envelope)
    }

    /// Serialize to canonical envelope JSON. Infallible in practice (every
    /// field is a plain string/number), so this returns the bytes directly —
    /// the caller is putting them straight into a `CuePayload::Inline`.
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("MidiControl serializes to JSON")
    }

    /// Decoded messages, in order, paired with their offset. Only callable
    /// after [`Self::parse`] has already vetted them; still returns a Result
    /// rather than panicking, since a hand-built envelope can reach here too.
    pub fn decoded(&self) -> Result<Vec<(u64, Vec<u8>)>, MidiControlError> {
        self.events
            .iter()
            .map(|e| e.bytes().map(|b| (e.offset_ms, b)))
            .collect()
    }
}

// ── hex (tiny, local — no dep for 20 lines) ─────────────────────────────────

/// Lowercase hex, no separators.
pub fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Strict hex decode: even length, `[0-9a-fA-F]` only. No whitespace/`0x`
/// tolerance HERE on purpose — the wire form is canonical; a human's messy
/// "F0 7E 7F" is normalized at the CLI surface, before it becomes an event.
pub fn from_hex(s: &str) -> Result<Vec<u8>, MidiControlError> {
    if !s.len().is_multiple_of(2) {
        return Err(MidiControlError::BadHex(s.to_string(), "odd number of hex digits"));
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = hex_digit(pair[0])
            .ok_or_else(|| MidiControlError::BadHex(s.to_string(), "non-hex digit"))?;
        let lo = hex_digit(pair[1])
            .ok_or_else(|| MidiControlError::BadHex(s.to_string(), "non-hex digit"))?;
        out.push(hi << 4 | lo);
    }
    Ok(out)
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_control_mime_is_its_own_distinct_directive() {
        // A sink dispatching on `cue.mime` must never confuse a device-addressed
        // control cue with the port-anonymous score path.
        assert_ne!(MIDI_CONTROL_MIME, crate::ABC_MIME);
        assert_ne!(MIDI_CONTROL_MIME, crate::RENDER_FLUSH_MIME);
        assert_ne!(MIDI_CONTROL_MIME, crate::PREPARE_MIME);
        assert_ne!(MIDI_CONTROL_MIME, crate::CLIP_MIME);
        assert_eq!(MIDI_CONTROL_MIME, "application/vnd.kaijutsu.midi-control+json");
    }

    #[test]
    fn hex_round_trips_every_byte() {
        let all: Vec<u8> = (0..=255u8).collect();
        assert_eq!(from_hex(&to_hex(&all)).unwrap(), all);
        assert_eq!(to_hex(&[0xf0, 0x0d]), "f00d");
    }

    #[test]
    fn hex_decode_refuses_junk_rather_than_guessing() {
        assert!(matches!(from_hex("abc"), Err(MidiControlError::BadHex(_, _))));
        assert!(matches!(from_hex("zz"), Err(MidiControlError::BadHex(_, _))));
        assert!(matches!(from_hex("b0 0b"), Err(MidiControlError::BadHex(_, _))));
        // Uppercase is accepted (gear manuals print it that way).
        assert_eq!(from_hex("F07E").unwrap(), vec![0xf0, 0x7e]);
        assert!(from_hex("").unwrap().is_empty());
    }

    #[test]
    fn an_envelope_round_trips_through_the_wire_form() {
        let control = MidiControl::new(
            "keystep-pro",
            vec![
                MidiControlEvent::now(&[0x90, 60, 100]),
                MidiControlEvent::new(250, &[0x80, 60, 0]),
            ],
        );
        let bytes = control.to_bytes();
        let back = MidiControl::parse(std::str::from_utf8(&bytes).unwrap()).unwrap();
        assert_eq!(back, control);
        assert_eq!(
            back.decoded().unwrap(),
            vec![(0, vec![0x90, 60, 100]), (250, vec![0x80, 60, 0])]
        );
    }

    /// The device name is the whole point of this envelope — it must survive
    /// the wire verbatim, since it is the sink's only routing key.
    #[test]
    fn the_device_name_rides_the_wire_verbatim() {
        let json = String::from_utf8(
            MidiControl::new("minibrute", vec![MidiControlEvent::now(&[0xb0, 74, 64])]).to_bytes(),
        )
        .unwrap();
        assert!(json.contains("\"device\":\"minibrute\""), "json: {json}");
        assert!(json.contains("\"data\":\"b04a40\""), "hex payload, not a byte array: {json}");
    }

    /// A zero offset is the common case and is omitted from the wire form —
    /// terse enough that a 32-message panic cue stays small.
    #[test]
    fn a_zero_offset_is_omitted_from_the_wire_form() {
        let json =
            String::from_utf8(MidiControl::new("d", vec![MidiControlEvent::now(&[0xb0, 123, 0])]).to_bytes())
                .unwrap();
        assert!(!json.contains("offset_ms"), "json: {json}");
        let json = String::from_utf8(
            MidiControl::new("d", vec![MidiControlEvent::new(5, &[0xb0, 123, 0])]).to_bytes(),
        )
        .unwrap();
        assert!(json.contains("\"offset_ms\":5"), "json: {json}");
    }

    #[test]
    fn a_wrong_version_envelope_is_refused_not_guessed_at() {
        let json = r#"{"v":99,"device":"d","events":[{"data":"b07b00"}]}"#;
        assert_eq!(MidiControl::parse(json), Err(MidiControlError::UnknownVersion(99)));
    }

    /// An unaddressed control cue must never fall back to "the render port" —
    /// mis-delivery to whatever the synth happens to be is exactly what this
    /// mime exists to prevent.
    #[test]
    fn an_envelope_with_no_device_is_refused() {
        let json = r#"{"v":1,"device":"   ","events":[{"data":"b07b00"}]}"#;
        assert_eq!(MidiControl::parse(json), Err(MidiControlError::EmptyDevice));
    }

    #[test]
    fn an_envelope_with_no_events_is_refused() {
        let json = r#"{"v":1,"device":"d","events":[]}"#;
        assert_eq!(MidiControl::parse(json), Err(MidiControlError::NoEvents));
    }

    /// All-or-nothing: one bad message refuses the WHOLE envelope, so a cue
    /// can never half-play.
    #[test]
    fn one_bad_hex_message_refuses_the_whole_envelope() {
        let json = r#"{"v":1,"device":"d","events":[{"data":"b07b00"},{"data":"nope"}]}"#;
        assert!(matches!(MidiControl::parse(json), Err(MidiControlError::BadHex(_, _))));
    }

    #[test]
    fn malformed_json_is_a_loud_parse_error() {
        assert!(matches!(MidiControl::parse("{"), Err(MidiControlError::Json(_))));
    }
}
