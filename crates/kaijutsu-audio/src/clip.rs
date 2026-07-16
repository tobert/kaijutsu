//! Shape A clip record — placed media on a track (`docs/pcm.md`).
//!
//! A **clip** is a placed media reference: a small, human/model-readable
//! symbolic record ("play this CAS hash at this offset, at this gain") that a
//! producer authors as text the same way it authors ABC. The *cell* owns where
//! in musical time (`Cell.span`); this payload owns *what media and how to
//! render it*. Bytes never ride the record — `media` is a CAS hash the sink
//! resolves under the speculation lead.
//!
//! This is the first consumer of the clip design (`docs/pcm.md` slice 5b): the
//! record type + its content-type-keyed validator, pure data and FFI-free (the
//! sibling of ABC's validator on the decouple-Act-from-ABC axis). The
//! mime-keyed emit that carries a clip as a [`crate::RenderCue`], the client
//! prefetch, and the app clip renderer are slice 5c.

use kaijutsu_cas::{ContentHash, ContentStore};
use serde::{Deserialize, Serialize};

/// The clip record MIME (`docs/pcm.md` Shape A). A [`crate::RenderCue`]
/// carrying a clip sets this as its `mime`.
pub const CLIP_MIME: &str = "application/vnd.kaijutsu.clip+json";

/// The clip record version this build writes and accepts. Per the OTIO lesson
/// the version is per-record; a bump is how a breaking field lands (growth that
/// doesn't break goes in [`Clip::ext`]).
pub const CLIP_VERSION: u32 = 1;

/// A Shape A clip record (`docs/pcm.md`). A small JSON object models author
/// as text; `serde` round-trips it, kaish `jq`s it, and the app can render it
/// as plain text until a clip renderer exists.
///
/// Milliseconds are the media-internal time domain (integer — source range is
/// wall-time, not musical; floats would invite fuzz). `gain_db` is dB, not
/// linear (`0.0` == unity). Tempo changes move *where* the clip starts in wall
/// time (the `Tick` anchor follows the beat) but never its internal playback
/// rate — no stretch/repitch in v1 (the `stretch` field name is reserved for
/// Shape B). See `docs/pcm.md` for the full rationale.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    /// Record version (per-record, OTIO-style). Must equal [`CLIP_VERSION`].
    pub v: u32,
    /// The sample bytes, in CAS. REQUIRED. Note this is a *different* object at
    /// a different altitude from the cell's own content hash (the cell hashes
    /// the clip record; the record's `media` hashes the sample) — the two-level
    /// reference of `docs/pcm.md`.
    pub media: ContentHash,
    /// What the sink decodes (an [`crate::AudioFormatHint`] source, e.g.
    /// `audio/wav`). REQUIRED, non-empty.
    pub mime: String,
    /// Human/model-readable label — CAS hashes are opaque, so the label is how
    /// the score reads (the anti-SCTE-35-hex lesson). REQUIRED, non-empty.
    pub label: String,
    /// Where in the media to start playing (default `0`).
    #[serde(default)]
    pub src_offset_ms: u64,
    /// How much of the media to play; `None` (default) = to the end.
    #[serde(default)]
    pub src_len_ms: Option<u64>,
    /// Playback gain in **dB** (default `0.0` = unity); negative attenuates.
    #[serde(default)]
    pub gain_db: f64,
    /// Extension bag: unknown keys here survive round-trips untouched, the
    /// sanctioned place for forward-compatible growth (the OTIO lesson).
    #[serde(default)]
    pub ext: serde_json::Map<String, serde_json::Value>,
}

/// A clip record failed to parse or validate. Fail-loud: a bad clip is a crash
/// at schedule time, never a silently corrupted or dropped sound.
#[derive(Debug, thiserror::Error)]
pub enum ClipError {
    /// The record was not valid JSON, or the wrong shape.
    #[error("clip record is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// `v` names a version this build does not understand.
    #[error("unknown clip record version {0} (this build supports v1)")]
    UnknownVersion(u32),
    /// `label` was empty (or whitespace only).
    #[error("clip label must be non-empty")]
    EmptyLabel,
    /// `mime` was empty.
    #[error("clip mime must be non-empty")]
    EmptyMime,
    /// `media` was well-formed but absent from CAS at schedule time. Caught
    /// here, loudly — not two phrases later at prefetch.
    #[error("clip media {0} is not present in CAS")]
    MediaMissing(ContentHash),
}

impl Clip {
    /// Parse + **structurally** validate a Shape A record: `v` known, `label`
    /// and `mime` non-empty, `media` a well-formed hash. Does NOT check CAS
    /// presence — that is [`Clip::parse_validated`] (schedule time, needs a
    /// store). Unknown `ext` keys pass through untouched.
    pub fn parse(json: &str) -> Result<Clip, ClipError> {
        let clip: Clip = serde_json::from_str(json)?;
        clip.validate_structure()?;
        Ok(clip)
    }

    /// Full schedule-time validation: structural (see [`Clip::parse`]) **plus**
    /// `media` present in `store`. This is the content-type-keyed validator the
    /// scheduler runs — an absent sample fails loud here, not silently later.
    pub fn parse_validated(json: &str, store: &dyn ContentStore) -> Result<Clip, ClipError> {
        let clip = Self::parse(json)?;
        if !store.exists(&clip.media) {
            return Err(ClipError::MediaMissing(clip.media.clone()));
        }
        Ok(clip)
    }

    /// Serialize back to canonical Shape A JSON.
    pub fn to_json(&self) -> Result<String, ClipError> {
        Ok(serde_json::to_string(self)?)
    }

    fn validate_structure(&self) -> Result<(), ClipError> {
        if self.v != CLIP_VERSION {
            return Err(ClipError::UnknownVersion(self.v));
        }
        if self.label.trim().is_empty() {
            return Err(ClipError::EmptyLabel);
        }
        if self.mime.trim().is_empty() {
            return Err(ClipError::EmptyMime);
        }
        // `media` well-formedness is enforced by `ContentHash`'s validating
        // deserialize (CAS B0): a malformed hash fails as `ClipError::Json`
        // during `parse`, before we ever reach here — no re-check needed.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A well-formed 32-hex hash for tests (`ContentHash::from_data` produces
    /// exactly this shape).
    fn a_hash() -> ContentHash {
        ContentHash::from_data(b"rimshot")
    }

    /// A minimal `ContentStore` stub for validator tests: `exists` answers a
    /// fixed bool; the rest are unused. FFI-free, no filesystem.
    struct StubStore {
        present: bool,
    }
    impl ContentStore for StubStore {
        fn store(&self, _: &[u8], _: &str) -> Result<ContentHash, kaijutsu_cas::StoreError> {
            unimplemented!("stub")
        }
        fn retrieve(&self, _: &ContentHash) -> Result<Option<Vec<u8>>, kaijutsu_cas::StoreError> {
            unimplemented!("stub")
        }
        fn exists(&self, _: &ContentHash) -> bool {
            self.present
        }
        fn path(&self, _: &ContentHash) -> Option<std::path::PathBuf> {
            None
        }
        fn inspect(
            &self,
            _: &ContentHash,
        ) -> Result<Option<kaijutsu_cas::CasReference>, kaijutsu_cas::StoreError> {
            Ok(None)
        }
        fn remove(&self, _: &ContentHash) -> Result<bool, kaijutsu_cas::StoreError> {
            unimplemented!("stub")
        }
    }

    #[test]
    fn full_record_round_trips() {
        let json = format!(
            r#"{{
              "v": 1,
              "media": "{}",
              "mime": "audio/wav",
              "label": "rimshot, dry",
              "src_offset_ms": 100,
              "src_len_ms": 500,
              "gain_db": -6.0,
              "ext": {{}}
            }}"#,
            a_hash()
        );
        let clip = Clip::parse(&json).expect("valid record parses");
        assert_eq!(clip.v, 1);
        assert_eq!(clip.media, a_hash());
        assert_eq!(clip.mime, "audio/wav");
        assert_eq!(clip.label, "rimshot, dry");
        assert_eq!(clip.src_offset_ms, 100);
        assert_eq!(clip.src_len_ms, Some(500));
        assert_eq!(clip.gain_db, -6.0);

        // Round-trips through serialize → parse unchanged.
        let back = Clip::parse(&clip.to_json().expect("serialize")).expect("reparse");
        assert_eq!(back, clip);
    }

    #[test]
    fn minimal_record_uses_documented_defaults() {
        let json = format!(
            r#"{{ "v": 1, "media": "{}", "mime": "audio/wav", "label": "kick" }}"#,
            a_hash()
        );
        let clip = Clip::parse(&json).expect("minimal record parses");
        assert_eq!(clip.src_offset_ms, 0, "offset defaults to 0");
        assert_eq!(clip.src_len_ms, None, "len defaults to to-end (None)");
        assert_eq!(clip.gain_db, 0.0, "gain defaults to unity (0 dB)");
        assert!(clip.ext.is_empty(), "ext defaults to empty");
    }

    #[test]
    fn unknown_ext_keys_survive_round_trip() {
        // The OTIO forward-compat lesson: a newer writer's ext keys must not be
        // dropped on a round-trip through an older reader.
        let json = format!(
            r##"{{ "v": 1, "media": "{}", "mime": "audio/wav", "label": "snare",
                  "ext": {{ "warp_anchors": [1, 2], "color": "#ff0000" }} }}"##,
            a_hash()
        );
        let clip = Clip::parse(&json).expect("record with ext parses");
        assert_eq!(clip.ext.get("color").and_then(|v| v.as_str()), Some("#ff0000"));

        let back = Clip::parse(&clip.to_json().expect("serialize")).expect("reparse");
        assert_eq!(back.ext, clip.ext, "ext keys survive the round trip");
    }

    #[test]
    fn unknown_version_is_rejected_loud() {
        let json = format!(
            r#"{{ "v": 2, "media": "{}", "mime": "audio/wav", "label": "x" }}"#,
            a_hash()
        );
        assert!(matches!(
            Clip::parse(&json),
            Err(ClipError::UnknownVersion(2))
        ));
    }

    #[test]
    fn empty_label_is_rejected() {
        let json = format!(
            r#"{{ "v": 1, "media": "{}", "mime": "audio/wav", "label": "   " }}"#,
            a_hash()
        );
        assert!(matches!(Clip::parse(&json), Err(ClipError::EmptyLabel)));
    }

    #[test]
    fn empty_mime_is_rejected() {
        let json = format!(
            r#"{{ "v": 1, "media": "{}", "mime": "", "label": "x" }}"#,
            a_hash()
        );
        assert!(matches!(Clip::parse(&json), Err(ClipError::EmptyMime)));
    }

    #[test]
    fn malformed_media_hash_is_rejected() {
        // Not 32 hex chars. `ContentHash`'s validating deserialize (CAS B0)
        // rejects it at the serde boundary, so `parse` surfaces it as `Json`
        // before validate_structure ever runs — still loud, still no bad clip.
        let json = r#"{ "v": 1, "media": "not-a-hash", "mime": "audio/wav", "label": "x" }"#;
        assert!(matches!(Clip::parse(json), Err(ClipError::Json(_))));
    }

    #[test]
    fn media_absent_from_cas_is_rejected_at_schedule() {
        let json = format!(
            r#"{{ "v": 1, "media": "{}", "mime": "audio/wav", "label": "x" }}"#,
            a_hash()
        );
        let absent = StubStore { present: false };
        assert!(matches!(
            Clip::parse_validated(&json, &absent),
            Err(ClipError::MediaMissing(_))
        ));

        let present = StubStore { present: true };
        assert!(
            Clip::parse_validated(&json, &present).is_ok(),
            "a present sample validates"
        );
    }
}
