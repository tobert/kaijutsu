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
}
