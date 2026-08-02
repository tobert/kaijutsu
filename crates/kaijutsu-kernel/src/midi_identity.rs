//! The universal MIDI Identity Request/Reply — pure bytes in, facts out
//! (`docs/midi-next.md` "SysEx: the exchange pattern", slice 1 step 5).
//!
//! This is the one MIDI dialogue every device is *supposed* to answer, and
//! the first exchange kaijutsu speaks: `F0 7E 7F 06 01 F7` out, an Identity
//! Reply back. Everything here is a pure function over bytes — the kernel
//! never touches hardware (`docs/midi.md` "the sink owns the hardware"), it
//! composes the request, hands it to a sink, and parses whatever comes back.
//!
//! **Refuse rather than guess.** A malformed reply is an error naming what
//! was wrong, never a partially-filled record: a fabricated manufacturer id
//! would end up stored as a *pulled* fact — the most-trusted provenance in
//! the store — and outlive every chance to notice it was invented.

/// The universal non-realtime Identity Request, broadcast device id (`7F`).
/// Six bytes, the same on every device that answers at all.
pub const IDENTITY_REQUEST: [u8; 6] = [0xF0, 0x7E, 0x7F, 0x06, 0x01, 0xF7];

/// What a reply must START with for a sink to hand it back: `F0 7E`
/// (universal non-realtime). Deliberately only two bytes — byte 2 is the
/// answering device's own id, which we don't know before it tells us, so
/// matching further would filter out the very answer we asked for.
pub const IDENTITY_REPLY_PREFIX: [u8; 2] = [0xF0, 0x7E];

/// A parsed Identity Reply. `raw` rides along because the parsed view is our
/// reading and the bytes are the evidence — a device with an off-spec reply
/// is a thing we want to be able to look at later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiIdentity {
    /// The answering device's id byte (`7F` = "the broadcast one answered").
    pub device_id: u8,
    /// Manufacturer id: one byte, or three when the first is `00`
    /// (the extended-id escape). Never normalized to a single number — the
    /// SysEx id space genuinely has both shapes.
    pub manufacturer: Vec<u8>,
    /// Our name for `manufacturer`, when we know one. `None` is honest and
    /// common: the id is the fact, the name is a convenience.
    pub manufacturer_name: Option<&'static str>,
    /// Device family code (two 7-bit bytes, LSB first on the wire).
    pub family: u16,
    /// Family member / model code (two 7-bit bytes, LSB first).
    pub model: u16,
    /// Four version bytes, exactly as sent. Vendors disagree wildly about
    /// what these mean (BCD, ASCII digits, plain ints), so we never render
    /// them as a version *number* — only as bytes.
    pub version: [u8; 4],
    /// The complete reply, verbatim.
    pub raw: Vec<u8>,
}

/// The handful of manufacturer ids on Amy's bench and their obvious
/// neighbours (`docs/midi-next.md` "The roster"). Deliberately tiny: this is
/// a display convenience, and a wrong name is worse than no name. Grow it
/// when a real device shows up unnamed.
const MANUFACTURERS: &[(&[u8], &str)] = &[
    (&[0x00, 0x20, 0x6B], "Arturia"),
    (&[0x00, 0x20, 0x29], "Focusrite/Novation"),
    (&[0x00, 0x20, 0x32], "Behringer"),
    (&[0x00, 0x01, 0x05], "M-Audio"),
    (&[0x04], "Moog"),
    (&[0x41], "Roland"),
    (&[0x42], "Korg"),
    (&[0x43], "Yamaha"),
    (&[0x47], "Akai"),
    (&[0x7D], "Non-commercial / educational"),
];

fn manufacturer_name(id: &[u8]) -> Option<&'static str> {
    MANUFACTURERS
        .iter()
        .find(|(bytes, _)| *bytes == id)
        .map(|(_, name)| *name)
}

/// Render a byte slice as lowercase hex — how raw MIDI is written everywhere
/// else in this codebase (`kj midi send … sysex`, the control envelope).
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl MidiIdentity {
    /// Manufacturer id as hex (`"00206b"`), the form a profile document or a
    /// human comparing against a manual would write.
    pub fn manufacturer_hex(&self) -> String {
        hex(&self.manufacturer)
    }

    /// A one-line summary for a result message: name (or bare id), family,
    /// model, version bytes.
    pub fn summary(&self) -> String {
        let maker = match self.manufacturer_name {
            Some(name) => format!("{name} ({})", self.manufacturer_hex()),
            None => format!("manufacturer {}", self.manufacturer_hex()),
        };
        format!(
            "{maker}, family {}, model {}, version {}",
            self.family,
            self.model,
            hex(&self.version)
        )
    }

    /// The JSON shape stored as the `pulled` fact in `/run/midi/<device>` and
    /// echoed by `kj midi identify --json`.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "deviceId": self.device_id,
            "manufacturer": self.manufacturer_hex(),
            "manufacturerName": self.manufacturer_name,
            "family": self.family,
            "model": self.model,
            "version": hex(&self.version),
            "raw": hex(&self.raw),
        })
    }
}

/// Parse an Identity Reply (`F0 7E <dev> 06 02 … F7`) into its facts.
///
/// Every refusal names what was wrong and includes nothing invented. The
/// checks, in order: framing (`F0`…`F7`), the universal-non-realtime +
/// sub-id pair that makes it an Identity *Reply* rather than some other
/// dialogue, 7-bit cleanliness of the body, and finally the length the
/// manufacturer-id shape implies.
pub fn parse_identity_reply(bytes: &[u8]) -> Result<MidiIdentity, String> {
    if bytes.len() < 15 {
        return Err(format!(
            "identity reply is {} byte(s); the shortest legal one is 15 \
             (F0 7E dd 06 02 <mm> <family x2> <model x2> <version x4> F7)",
            bytes.len()
        ));
    }
    if bytes[0] != 0xF0 || *bytes.last().expect("checked non-empty") != 0xF7 {
        return Err(format!(
            "identity reply is not a complete SysEx message (starts {:#04x}, ends {:#04x}; \
             expected F0 … F7)",
            bytes[0],
            bytes.last().copied().unwrap_or(0)
        ));
    }
    if bytes[1] != 0x7E {
        return Err(format!(
            "not a universal non-realtime message (byte 1 is {:#04x}, expected 7E)",
            bytes[1]
        ));
    }
    if bytes[3] != 0x06 || bytes[4] != 0x02 {
        return Err(format!(
            "not an Identity Reply (sub-ids are {:#04x} {:#04x}, expected 06 02)",
            bytes[3], bytes[4]
        ));
    }
    // Everything between the framing bytes must be 7-bit; a high bit inside a
    // SysEx body means we are looking at a mangled or mis-reassembled message,
    // and reading fields out of it would store a fiction.
    if let Some((i, b)) = bytes[1..bytes.len() - 1]
        .iter()
        .enumerate()
        .find(|(_, b)| **b & 0x80 != 0)
    {
        return Err(format!(
            "identity reply byte {} is {:#04x} — SysEx payload must be 7-bit",
            i + 1,
            b
        ));
    }

    let device_id = bytes[2];
    // The extended-manufacturer escape: a leading 00 means the id is three
    // bytes, not one.
    let manufacturer: Vec<u8> = if bytes[5] == 0x00 {
        bytes
            .get(5..8)
            .ok_or_else(|| {
                "identity reply claims a 3-byte manufacturer id but is too short".to_string()
            })?
            .to_vec()
    } else {
        vec![bytes[5]]
    };
    let body = 5 + manufacturer.len();
    // family(2) + model(2) + version(4) + F7
    if bytes.len() < body + 9 {
        return Err(format!(
            "identity reply is {} byte(s); a {}-byte manufacturer id needs {}",
            bytes.len(),
            manufacturer.len(),
            body + 9
        ));
    }
    let word = |lsb: u8, msb: u8| u16::from(lsb) | (u16::from(msb) << 7);
    let family = word(bytes[body], bytes[body + 1]);
    let model = word(bytes[body + 2], bytes[body + 3]);
    let version = [
        bytes[body + 4],
        bytes[body + 5],
        bytes[body + 6],
        bytes[body + 7],
    ];

    Ok(MidiIdentity {
        device_id,
        manufacturer_name: manufacturer_name(&manufacturer),
        manufacturer,
        family,
        model,
        version,
        raw: bytes.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A KeyStep-Pro-shaped reply: Arturia's 3-byte id, family 1, model 2.
    fn arturia_reply() -> Vec<u8> {
        vec![
            0xF0, 0x7E, 0x00, 0x06, 0x02, // header + Identity Reply sub-ids
            0x00, 0x20, 0x6B, // Arturia
            0x01, 0x00, // family 1
            0x02, 0x00, // model 2
            0x01, 0x00, 0x03, 0x04, // version bytes
            0xF7,
        ]
    }

    /// A single-byte-manufacturer reply (Moog): the 15-byte minimum.
    fn moog_reply() -> Vec<u8> {
        vec![
            0xF0, 0x7E, 0x7F, 0x06, 0x02, 0x04, // Moog
            0x05, 0x00, // family 5
            0x01, 0x00, // model 1
            0x00, 0x01, 0x00, 0x02, 0xF7,
        ]
    }

    #[test]
    fn the_request_is_the_universal_six_byte_broadcast() {
        assert_eq!(hex(&IDENTITY_REQUEST), "f07e7f0601f7");
    }

    #[test]
    fn a_three_byte_manufacturer_reply_parses_whole() {
        let id = parse_identity_reply(&arturia_reply()).expect("valid reply");
        assert_eq!(id.manufacturer, vec![0x00, 0x20, 0x6B]);
        assert_eq!(id.manufacturer_hex(), "00206b");
        assert_eq!(id.manufacturer_name, Some("Arturia"));
        assert_eq!(id.family, 1);
        assert_eq!(id.model, 2);
        assert_eq!(id.version, [0x01, 0x00, 0x03, 0x04]);
        assert_eq!(id.device_id, 0x00);
        assert_eq!(id.raw, arturia_reply(), "the evidence rides along verbatim");
    }

    #[test]
    fn a_one_byte_manufacturer_reply_parses_at_the_minimum_length() {
        let bytes = moog_reply();
        assert_eq!(bytes.len(), 15, "this fixture IS the minimum legal length");
        let id = parse_identity_reply(&bytes).expect("valid reply");
        assert_eq!(id.manufacturer, vec![0x04]);
        assert_eq!(id.manufacturer_name, Some("Moog"));
        assert_eq!(id.family, 5);
        assert_eq!(id.model, 1);
        assert_eq!(id.device_id, 0x7F);
    }

    /// Family/model are 14-bit values split LSB-first across two 7-bit bytes
    /// — the classic place to get MIDI wrong by a factor of 128.
    #[test]
    fn family_and_model_are_lsb_first_fourteen_bit_words() {
        let mut bytes = moog_reply();
        bytes[6] = 0x7F; // family LSB
        bytes[7] = 0x01; // family MSB
        bytes[8] = 0x00; // model LSB
        bytes[9] = 0x02; // model MSB
        let id = parse_identity_reply(&bytes).unwrap();
        assert_eq!(id.family, 0x7F | (1 << 7), "127 + 128 = 255");
        assert_eq!(id.model, 2 << 7, "256");
    }

    /// An unknown manufacturer keeps its id and simply has no name — never a
    /// guessed one.
    #[test]
    fn an_unknown_manufacturer_has_an_id_but_no_name() {
        let mut bytes = moog_reply();
        bytes[5] = 0x66;
        let id = parse_identity_reply(&bytes).unwrap();
        assert_eq!(id.manufacturer_hex(), "66");
        assert_eq!(id.manufacturer_name, None);
        assert!(id.summary().contains("manufacturer 66"), "{}", id.summary());
    }

    #[test]
    fn a_truncated_reply_is_refused() {
        let bytes = moog_reply();
        let err = parse_identity_reply(&bytes[..10]).unwrap_err();
        assert!(err.contains("byte(s)"), "err: {err}");
    }

    /// A three-byte manufacturer id in a 15-byte message is exactly long
    /// enough to pass the first length gate and still be two bytes short —
    /// the boundary that would silently read past the end.
    #[test]
    fn a_short_message_claiming_an_extended_manufacturer_id_is_refused() {
        let mut bytes = moog_reply();
        bytes[5] = 0x00; // claim the 3-byte escape without the length for it
        let err = parse_identity_reply(&bytes).unwrap_err();
        assert!(err.contains("manufacturer id needs 17"), "err: {err}");
    }

    #[test]
    fn a_non_sysex_or_unterminated_message_is_refused() {
        let mut bytes = moog_reply();
        bytes[0] = 0x90;
        assert!(parse_identity_reply(&bytes).unwrap_err().contains("complete SysEx"));

        let mut bytes = moog_reply();
        let last = bytes.len() - 1;
        bytes[last] = 0x00;
        assert!(parse_identity_reply(&bytes).unwrap_err().contains("complete SysEx"));
    }

    /// The reply to some OTHER universal request (a GM On, a sample dump)
    /// must not be read as an identity: same framing, different sub-ids.
    #[test]
    fn a_different_universal_message_is_not_an_identity_reply() {
        let mut bytes = moog_reply();
        bytes[3] = 0x06;
        bytes[4] = 0x01; // an identity REQUEST, not a reply
        let err = parse_identity_reply(&bytes).unwrap_err();
        assert!(err.contains("not an Identity Reply"), "err: {err}");

        let mut bytes = moog_reply();
        bytes[1] = 0x7F; // universal REALtime
        assert!(
            parse_identity_reply(&bytes)
                .unwrap_err()
                .contains("universal non-realtime")
        );
    }

    /// A high bit inside the body means the bytes were mangled (a bad
    /// reassembly, a dropped fragment). Refuse: a "pulled" fact is the most
    /// trusted provenance in the store, so it must never be salvaged.
    #[test]
    fn a_high_bit_in_the_body_is_refused_not_masked() {
        let mut bytes = moog_reply();
        bytes[8] = 0x81;
        let err = parse_identity_reply(&bytes).unwrap_err();
        assert!(err.contains("7-bit"), "err: {err}");
    }

    #[test]
    fn the_json_view_carries_id_and_evidence() {
        let json = parse_identity_reply(&arturia_reply()).unwrap().to_json();
        assert_eq!(json["manufacturer"], "00206b");
        assert_eq!(json["manufacturerName"], "Arturia");
        assert_eq!(json["family"], 1);
        assert_eq!(json["model"], 2);
        assert_eq!(json["version"], "01000304");
        assert!(json["raw"].as_str().unwrap().starts_with("f07e00060200206b"));
    }
}
