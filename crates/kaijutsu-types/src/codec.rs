//! Central, versioned CBOR codec for Kaijutsu.
//!
//! Every encoded buffer begins with a single format byte so the on-disk and
//! on-wire representation can evolve. `FORMAT_V1` is CBOR via `ciborium`.

/// Format byte for version 1: CBOR (ciborium) payload.
const FORMAT_V1: u8 = 1;

/// Errors produced while encoding or decoding through the central codec.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("cbor encode: {0}")]
    Encode(String),
    #[error("cbor decode: {0}")]
    Decode(String),
    #[error("unknown serialization format byte: {0}")]
    UnknownFormat(u8),
    #[error("empty buffer")]
    Empty,
}

/// Encode `value` as a versioned CBOR buffer: a `FORMAT_V1` byte followed by
/// the ciborium-encoded payload.
pub fn encode<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, CodecError> {
    let mut buf = vec![FORMAT_V1];
    ciborium::into_writer(value, &mut buf).map_err(|e| CodecError::Encode(e.to_string()))?;
    Ok(buf)
}

/// Decode a versioned CBOR buffer produced by [`encode`].
pub fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, CodecError> {
    match bytes.split_first() {
        Some((&FORMAT_V1, rest)) => {
            ciborium::from_reader(rest).map_err(|e| CodecError::Decode(e.to_string()))
        }
        Some((&other, _)) => Err(CodecError::UnknownFormat(other)),
        None => Err(CodecError::Empty),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_vec_u8() {
        let value: Vec<u8> = vec![0, 1, 2, 3, 255, 128];
        let encoded = encode(&value).expect("encode");
        let decoded: Vec<u8> = decode(&encoded).expect("decode");
        assert_eq!(value, decoded);
    }

    #[test]
    fn round_trip_string_u64_tuple() {
        let value: (String, u64) = ("hello".to_string(), 42_u64);
        let encoded = encode(&value).expect("encode");
        let decoded: (String, u64) = decode(&encoded).expect("decode");
        assert_eq!(value, decoded);
    }

    #[test]
    fn unknown_format_byte() {
        let buf = [0x02_u8, 0x00, 0x00];
        let err = decode::<Vec<u8>>(&buf).expect_err("should reject unknown format");
        assert!(matches!(err, CodecError::UnknownFormat(2)));
    }

    #[test]
    fn empty_slice_is_empty_error() {
        let err = decode::<Vec<u8>>(&[]).expect_err("should reject empty buffer");
        assert!(matches!(err, CodecError::Empty));
    }

    #[test]
    fn first_byte_is_format_v1() {
        let value: u64 = 7;
        let encoded = encode(&value).expect("encode");
        assert_eq!(encoded[0], 1);
    }

    // ── T16 (design §8 Phase 5): BlockSnapshot track CBOR evolution ──────────
    //
    // The additive-evolution contract for `BlockSnapshot.track`. ciborium +
    // serde derive encodes structs as CBOR maps keyed by field NAME, and there
    // is no `deny_unknown_fields` anywhere in this crate, so:
    //   (a) an OLD payload (no `track` key) decodes with `track = None`
    //       (`#[serde(default)]`), and
    //   (b) a NEW payload (with `track`) decodes under an OLD-shape reader that
    //       simply ignores the unknown key.
    // This is the permanent CI net the issues.md fixture ask wants.

    use crate::{BlockKind, BlockSnapshot, BlockSnapshotBuilder, ContextId, PrincipalId, TrackId};

    /// An old-shape mimic of `BlockSnapshot` — the SAME field names ciborium keys
    /// on, but deliberately WITHOUT `track`. Encoding this and decoding it as the
    /// real (track-bearing) `BlockSnapshot` proves the old→new direction; decoding
    /// a real snapshot as this proves the new→old (unknown-key-tolerance) direction.
    /// The required (no-`serde(default)`) fields of `BlockSnapshot` plus `tick`,
    /// in declaration order — a genuine minimal old-shape payload. The full
    /// `BlockSnapshot` fills every other field (incl. `track`) from its
    /// `#[serde(default)]`s on decode.
    #[derive(serde::Serialize, serde::Deserialize)]
    struct OldBlockSnapshotMimic {
        id: crate::BlockId,
        role: crate::Role,
        status: crate::Status,
        kind: BlockKind,
        content: String,
        created_at: u64,
        tick: Option<crate::Tick>,
    }

    fn fixture_id() -> crate::BlockId {
        // Deterministic id so a frozen byte literal stays stable. UUIDv5-derived
        // principals + a fixed-bytes context keep the encoding reproducible.
        let ctx = ContextId::from_bytes([7u8; 16]);
        crate::BlockId::new(ctx, PrincipalId::beat(), 3)
    }

    /// FROZEN pre-track CBOR for an old-shape snapshot (format byte 1 + ciborium
    /// map). Captured once from `encode(&OldBlockSnapshotMimic{..})`; it must keep
    /// decoding into a track-less `BlockSnapshot` forever. If serde/ciborium ever
    /// changes the encoding, this literal makes the break LOUD instead of silent.
    const FROZEN_PRE_TRACK_SNAPSHOT: &[u8] = &[
        0x01, 0xa7, 0x62, 0x69, 0x64, 0xa3, 0x6a, 0x63, 0x6f, 0x6e, 0x74, 0x65, 0x78, 0x74, 0x5f,
        0x69, 0x64, 0x50, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07,
        0x07, 0x07, 0x07, 0x07, 0x6c, 0x70, 0x72, 0x69, 0x6e, 0x63, 0x69, 0x70, 0x61, 0x6c, 0x5f,
        0x69, 0x64, 0x50, 0xfc, 0x40, 0x50, 0x99, 0xa1, 0x98, 0x5e, 0x40, 0x99, 0x48, 0xf6, 0xfa,
        0x1e, 0x55, 0xd8, 0x4e, 0x63, 0x73, 0x65, 0x71, 0x03, 0x64, 0x72, 0x6f, 0x6c, 0x65, 0x65,
        0x6d, 0x6f, 0x64, 0x65, 0x6c, 0x66, 0x73, 0x74, 0x61, 0x74, 0x75, 0x73, 0x64, 0x64, 0x6f,
        0x6e, 0x65, 0x64, 0x6b, 0x69, 0x6e, 0x64, 0x64, 0x74, 0x65, 0x78, 0x74, 0x67, 0x63, 0x6f,
        0x6e, 0x74, 0x65, 0x6e, 0x74, 0x65, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x6a, 0x63, 0x72, 0x65,
        0x61, 0x74, 0x65, 0x64, 0x5f, 0x61, 0x74, 0x1b, 0x00, 0x00, 0x01, 0x8b, 0xcf, 0xe5, 0x68,
        0x00, 0x64, 0x74, 0x69, 0x63, 0x6b, 0x18, 0x2a,
    ];

    /// T16(a) — the FROZEN pre-track blob decodes into a `BlockSnapshot` with
    /// `track == None` (and the other fields intact). Permanent regression net.
    #[test]
    fn frozen_pre_track_snapshot_decodes_with_track_none() {
        let snap: BlockSnapshot =
            decode(FROZEN_PRE_TRACK_SNAPSHOT).expect("frozen old-shape blob must still decode");
        assert_eq!(snap.track, None, "a pre-track payload has no track");
        assert_eq!(snap.id, fixture_id());
        assert_eq!(snap.kind, BlockKind::Text);
        assert_eq!(snap.content, "hello");
        assert_eq!(snap.tick, Some(crate::Tick::new(42)));
    }

    /// Guards that FROZEN_PRE_TRACK_SNAPSHOT is the genuine encoding of the mimic
    /// (so the literal isn't quietly wrong). If serde/ciborium's encoding shifts,
    /// THIS fails loudly — the cue to re-freeze deliberately, not silently.
    #[test]
    fn frozen_literal_matches_current_old_shape_encoding() {
        let old = OldBlockSnapshotMimic {
            id: fixture_id(),
            role: crate::Role::Model,
            status: crate::Status::Done,
            kind: BlockKind::Text,
            content: "hello".to_string(),
            created_at: 1_700_000_000_000,
            tick: Some(crate::Tick::new(42)),
        };
        let bytes = encode(&old).expect("encode old mimic");
        assert_eq!(
            bytes, FROZEN_PRE_TRACK_SNAPSHOT,
            "frozen literal drifted from the current encoding — re-freeze intentionally"
        );
    }

    /// T16(b) — a NEW (track-bearing) snapshot decoded under the OLD-shape mimic:
    /// the unknown `track` key is tolerated and dropped (no deny_unknown_fields).
    #[test]
    fn new_track_snapshot_decodes_under_old_shape_reader() {
        let snap = BlockSnapshotBuilder::new(fixture_id(), BlockKind::Text)
            .content("hello")
            .tick(crate::Tick::new(42))
            .track(TrackId::new("bass").unwrap())
            .build();
        let bytes = encode(&snap).expect("encode new snapshot");
        let old: OldBlockSnapshotMimic =
            decode(&bytes).expect("old reader must tolerate the unknown track key");
        assert_eq!(old.content, "hello");
        assert_eq!(old.tick, Some(crate::Tick::new(42)));
    }
}
