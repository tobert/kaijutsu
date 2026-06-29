//! [`TrackId`] — stable lane identity on a timeline (DAW sense).
//!
//! A **track** is where a clip lives, not who recorded it. The track persists
//! while players come and go; `BlockId.principal_id` separately records who
//! played. "Voice" is reserved for ABC `V:`; "lane" is reserved for in-track
//! automation. See `docs/chameleon.md` and the FORK 1 design.

use serde::{Deserialize, Serialize};

/// The maximum length of a track id, in chars. Charset is `[a-z0-9_-]`.
const MAX_TRACK_ID_LEN: usize = 64;

/// Stable lane identity on a timeline (DAW sense). The track persists while
/// players come and go; `BlockId.principal_id` separately records who played.
///
/// **Lane identity ONLY** — never an ordering, barrier, or authorship concept.
/// `track == None` (on a block snapshot) matches no track: a track id is a lane
/// key and a lane key alone, and the scheduling principal is never a lane key.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TrackId(String);

/// Why a candidate string is not a valid [`TrackId`]. Validation always `Err`s
/// loudly — a track id is never silently normalized into existence (use
/// [`TrackId::slugify`] for the explicit, lossy, human-label path).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TrackIdError {
    #[error("track id is empty")]
    Empty,
    #[error("track id too long: {len} chars (max {MAX_TRACK_ID_LEN})")]
    TooLong { len: usize },
    #[error("track id contains an invalid character {ch:?} (allowed: a-z, 0-9, '_', '-')")]
    InvalidChar { ch: char },
}

/// True for the strict track-id charset: lowercase ascii letters, digits, `_`,
/// `-`. Lowercase-only kills case-aliasing; the set matches chart / kj-arg
/// ergonomics ("bass", "drums", "keys").
fn is_track_char(ch: char) -> bool {
    ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-'
}

impl TrackId {
    /// Strict constructor: `1..=64` chars of `[a-z0-9_-]`. Lowercase-only kills
    /// case-aliasing; the charset matches chart / kj-arg ergonomics. `Err` on
    /// violation — **never** silent normalization (that's [`Self::slugify`]'s job).
    pub fn new(s: impl Into<String>) -> Result<Self, TrackIdError> {
        let s = s.into();
        if s.is_empty() {
            return Err(TrackIdError::Empty);
        }
        // Count chars, not bytes: the length bound is on the human-readable id.
        let len = s.chars().count();
        if len > MAX_TRACK_ID_LEN {
            return Err(TrackIdError::TooLong { len });
        }
        if let Some(ch) = s.chars().find(|c| !is_track_char(*c)) {
            return Err(TrackIdError::InvalidChar { ch });
        }
        Ok(Self(s))
    }

    /// Total, documented, deterministic mapping from a human label into a valid
    /// track id: lowercase, map every invalid char to `-`, collapse runs of `-`,
    /// trim edge `-`, truncate to 64. Returns `None` when the result is empty
    /// (e.g. a label of pure emoji or whitespace) — the caller must then decide
    /// loudly, never fall back to a silent shared lane.
    ///
    /// Emits `tracing::info` when the slug differs from the input, so a lossy
    /// normalization is always heard.
    pub fn slugify(label: &str) -> Option<Self> {
        let mut out = String::with_capacity(label.len());
        let mut prev_dash = false;
        for ch in label.chars() {
            let lowered: Option<char> = if ch.is_ascii_uppercase() {
                Some(ch.to_ascii_lowercase())
            } else if is_track_char(ch) {
                Some(ch)
            } else {
                None
            };
            match lowered {
                Some(c) if c == '-' => {
                    // Collapse runs of '-' (whether original or mapped).
                    if !prev_dash {
                        out.push('-');
                        prev_dash = true;
                    }
                }
                Some(c) => {
                    out.push(c);
                    prev_dash = false;
                }
                None => {
                    // Invalid char maps to '-', collapsing runs.
                    if !prev_dash {
                        out.push('-');
                        prev_dash = true;
                    }
                }
            }
        }
        // Trim edge '-'.
        let trimmed = out.trim_matches('-');
        // Truncate to 64 chars (by char, matching `new`), then re-trim a trailing
        // '-' that truncation could expose.
        let slug: String = trimmed.chars().take(MAX_TRACK_ID_LEN).collect();
        let slug = slug.trim_end_matches('-').to_string();
        if slug.is_empty() {
            return None;
        }
        if slug != label {
            tracing::info!(input = label, slug = %slug, "TrackId::slugify normalized label");
        }
        // By construction `slug` satisfies `new`'s charset and length; the
        // expect documents that invariant rather than papering over a bug.
        Some(Self::new(slug).expect("slugify output is a valid track id by construction"))
    }

    /// The single-lane musician's default chair until band config exists.
    pub fn solo() -> Self {
        Self("solo".to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T10 — strict validation, slugify cases, and serde-transparent round-trip.
    #[test]
    fn track_id_validation() {
        // Accepts the documented charset.
        assert!(TrackId::new("bass").is_ok());
        assert!(TrackId::new("solo").is_ok());
        assert!(TrackId::new("a-1_x").is_ok());
        assert_eq!(TrackId::new("bass").unwrap().as_str(), "bass");
        // 64 chars is the boundary — exactly 64 is OK, 65 is not.
        let len64: String = "a".repeat(64);
        let len65: String = "a".repeat(65);
        assert!(TrackId::new(len64).is_ok());
        assert!(matches!(
            TrackId::new(len65),
            Err(TrackIdError::TooLong { len: 65 })
        ));

        // Rejects empty / whitespace / uppercase / spaces / unicode.
        assert_eq!(TrackId::new(""), Err(TrackIdError::Empty));
        assert!(matches!(
            TrackId::new(" "),
            Err(TrackIdError::InvalidChar { ch: ' ' })
        ));
        assert!(matches!(
            TrackId::new("Bass"),
            Err(TrackIdError::InvalidChar { ch: 'B' })
        ));
        assert!(matches!(
            TrackId::new("two words"),
            Err(TrackIdError::InvalidChar { ch: ' ' })
        ));
        assert!(matches!(
            TrackId::new("café"),
            Err(TrackIdError::InvalidChar { ch: 'é' })
        ));

        // slugify: documented total mapping.
        assert_eq!(
            TrackId::slugify("My Musician"),
            Some(TrackId::new("my-musician").unwrap())
        );
        // Collapse runs, trim edges.
        assert_eq!(
            TrackId::slugify("  Bass!! Line  "),
            Some(TrackId::new("bass-line").unwrap())
        );
        // Already-valid label slugs to itself.
        assert_eq!(TrackId::slugify("bass"), Some(TrackId::new("bass").unwrap()));
        // Pure-emoji / non-mappable → None (never a silent default).
        assert_eq!(TrackId::slugify("🎵"), None);
        assert_eq!(TrackId::slugify("   "), None);
        assert_eq!(TrackId::slugify("---"), None);

        // solo() is the documented default.
        assert_eq!(TrackId::solo().as_str(), "solo");

        // serde is transparent: serializes as a bare string and round-trips.
        let t = TrackId::new("drums").unwrap();
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(json, "\"drums\"", "serde(transparent) emits a bare string");
        let back: TrackId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, t);
    }
}
