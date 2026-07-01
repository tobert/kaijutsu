//! FFI-free audio render seam (see `docs/pcm.md` "The seam").
//!
//! This crate is pure data + one trait: no `alsa`/`pipewire`/`symphonia`, no
//! `tokio`, no kernel-ward crates. The kernel depends on it for
//! orchestration; hardware emission lives in the consuming binaries
//! (`kaijutsu-app`'s Bevy sink today, an edge-node ALSA agent later).

use kaijutsu_cas::ContentHash;
use std::time::Instant;

/// One audio sink. Implemented in the app (Bevy) and, later, an edge-node
/// agent (ALSA). `&self` (not `&mut self`) because the Bevy sink spawns
/// entities via `Commands` rather than mutating sink state, and the ALSA
/// sink's handle lives behind internal mutability — see `docs/pcm.md`.
pub trait AudioRenderTarget: Send {
    /// Play one sample. `at == None` means "now" (first slice); a scheduled
    /// instant arrives with the track integration (speculation lead).
    fn play(&self, sample: AudioRef, at: Option<Instant>) -> anyhow::Result<()>;
}

/// What crosses the wire / the seam. Encoded bytes + a format tag, or a CAS
/// ref the sink resolves. Decoding lives at the sink (Bevy decoders /
/// Symphonia) — the wire never carries raw PCM.
///
/// `Debug` is hand-written (not derived) so it NEVER formats the raw sample
/// bytes: a directive carrying an inline sample can be large, and a stray
/// `tracing::debug!(?flow)` deriving down to this type would otherwise dump the
/// whole buffer as an int array into a log line. We print the byte *count*
/// instead (gemini-pro review, 2026-07-01).
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AudioRef {
    /// Small samples inline.
    Encoded {
        bytes: Vec<u8>,
        format: AudioFormatHint,
    },
    /// Larger samples: fetch from CAS (the primary path — see "How it
    /// converges" in `docs/pcm.md`).
    Cas {
        hash: ContentHash,
        format: AudioFormatHint,
    },
}

impl std::fmt::Debug for AudioRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioRef::Encoded { bytes, format } => f
                .debug_struct("Encoded")
                .field("format", format)
                // The count, never the bytes — see the type doc.
                .field("bytes", &format_args!("[{} bytes]", bytes.len()))
                .finish(),
            AudioRef::Cas { hash, format } => f
                .debug_struct("Cas")
                .field("format", format)
                .field("hash", hash)
                .finish(),
        }
    }
}

impl AudioRef {
    /// The format tag, regardless of variant.
    pub fn format(&self) -> AudioFormatHint {
        match self {
            AudioRef::Encoded { format, .. } => *format,
            AudioRef::Cas { format, .. } => *format,
        }
    }

    /// Convenience: `self.format().mime()`.
    pub fn mime(&self) -> &'static str {
        self.format().mime()
    }
}

/// Aligns with Symphonia's codec set; the wire MIME derives from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum AudioFormatHint {
    Wav,
    Flac,
    Mp3,
    Ogg,
    Aac,
}

impl AudioFormatHint {
    /// The canonical MIME for this format — what rides the wire.
    pub fn mime(self) -> &'static str {
        match self {
            AudioFormatHint::Wav => "audio/wav",
            AudioFormatHint::Flac => "audio/flac",
            AudioFormatHint::Mp3 => "audio/mpeg",
            AudioFormatHint::Ogg => "audio/ogg",
            AudioFormatHint::Aac => "audio/aac",
        }
    }

    /// Exact inverse of `mime()`; unknown MIME types return `None`.
    pub fn from_mime(s: &str) -> Option<Self> {
        match s {
            "audio/wav" => Some(AudioFormatHint::Wav),
            "audio/flac" => Some(AudioFormatHint::Flac),
            "audio/mpeg" => Some(AudioFormatHint::Mp3),
            "audio/ogg" => Some(AudioFormatHint::Ogg),
            "audio/aac" => Some(AudioFormatHint::Aac),
            _ => None,
        }
    }

    /// Case-insensitive match on a file extension (`"kick.wav"` -> `Wav`);
    /// unknown / missing extension returns `None`.
    pub fn from_path_extension(path: &str) -> Option<Self> {
        let ext = path.rsplit_once('.')?.1.to_ascii_lowercase();
        match ext.as_str() {
            "wav" => Some(AudioFormatHint::Wav),
            "flac" => Some(AudioFormatHint::Flac),
            "mp3" => Some(AudioFormatHint::Mp3),
            "ogg" => Some(AudioFormatHint::Ogg),
            "aac" | "m4a" => Some(AudioFormatHint::Aac),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    const ALL_FORMATS: [AudioFormatHint; 5] = [
        AudioFormatHint::Wav,
        AudioFormatHint::Flac,
        AudioFormatHint::Mp3,
        AudioFormatHint::Ogg,
        AudioFormatHint::Aac,
    ];

    #[test]
    fn mime_returns_expected_string_for_each_variant() {
        assert_eq!(AudioFormatHint::Wav.mime(), "audio/wav");
        assert_eq!(AudioFormatHint::Flac.mime(), "audio/flac");
        assert_eq!(AudioFormatHint::Mp3.mime(), "audio/mpeg");
        assert_eq!(AudioFormatHint::Ogg.mime(), "audio/ogg");
        assert_eq!(AudioFormatHint::Aac.mime(), "audio/aac");
    }

    #[test]
    fn from_mime_round_trips_every_variant() {
        for format in ALL_FORMATS {
            assert_eq!(AudioFormatHint::from_mime(format.mime()), Some(format));
        }
        assert_eq!(AudioFormatHint::from_mime("application/json"), None);
    }

    #[test]
    fn mp3_mime_is_the_non_obvious_audio_mpeg() {
        assert_eq!(AudioFormatHint::Mp3.mime(), "audio/mpeg");
    }

    #[test]
    fn from_path_extension_matches_case_insensitively() {
        assert_eq!(
            AudioFormatHint::from_path_extension("kick.WAV"),
            Some(AudioFormatHint::Wav)
        );
        assert_eq!(
            AudioFormatHint::from_path_extension("loop.m4a"),
            Some(AudioFormatHint::Aac)
        );
        assert_eq!(AudioFormatHint::from_path_extension("notes.txt"), None);
        assert_eq!(AudioFormatHint::from_path_extension("noext"), None);
    }

    #[test]
    fn audio_ref_format_and_mime_delegate_regardless_of_variant() {
        let cas = AudioRef::Cas {
            hash: ContentHash::from_data(b"whatever"),
            format: AudioFormatHint::Flac,
        };
        assert_eq!(cas.format(), AudioFormatHint::Flac);
        assert_eq!(cas.mime(), "audio/flac");

        let encoded = AudioRef::Encoded {
            bytes: vec![1, 2, 3],
            format: AudioFormatHint::Ogg,
        };
        assert_eq!(encoded.format(), AudioFormatHint::Ogg);
        assert_eq!(encoded.mime(), "audio/ogg");
    }

    #[test]
    fn debug_elides_sample_bytes() {
        // A directive's Debug must never dump the raw sample buffer (a stray
        // `debug!(?flow)` would otherwise log MB of int array). Derived Debug
        // would print `bytes: [1, 2, 3, 4, 5]`; the hand-written one prints the
        // count. This test fails on the derive.
        let r = AudioRef::Encoded {
            bytes: vec![1, 2, 3, 4, 5],
            format: AudioFormatHint::Wav,
        };
        let s = format!("{r:?}");
        assert!(s.contains("[5 bytes]"), "debug shows the byte count: {s}");
        assert!(!s.contains("1, 2, 3"), "debug must NOT dump raw bytes: {s}");
        assert!(s.contains("Wav"), "debug still shows the format: {s}");

        // Cas has no bytes to leak — but confirm it still Debugs its hash.
        let c = AudioRef::Cas {
            hash: ContentHash::from_data(b"x"),
            format: AudioFormatHint::Flac,
        };
        let cs = format!("{c:?}");
        assert!(cs.contains("Cas") && cs.contains("Flac"), "cas debug: {cs}");
    }

    #[test]
    fn serde_round_trip_for_format_and_encoded_ref() {
        let format = AudioFormatHint::Wav;
        let json = serde_json::to_string(&format).expect("serialize format");
        let back: AudioFormatHint = serde_json::from_str(&json).expect("deserialize format");
        assert_eq!(back, format);

        let audio_ref = AudioRef::Encoded {
            bytes: vec![1, 2, 3],
            format: AudioFormatHint::Wav,
        };
        let json = serde_json::to_string(&audio_ref).expect("serialize AudioRef");
        let back: AudioRef = serde_json::from_str(&json).expect("deserialize AudioRef");
        assert_eq!(back, audio_ref);
    }

    /// Object-safety + `&self`/interior-mutability shape check: a test-only
    /// sink stored as `Box<dyn AudioRenderTarget>`. `play` takes `&self`, so
    /// the recorder needs interior mutability (a shared `Arc<Mutex<..>>` so
    /// the test can still observe what the boxed trait object captured).
    struct Recorder {
        captured: Arc<Mutex<Vec<AudioFormatHint>>>,
    }

    impl AudioRenderTarget for Recorder {
        fn play(&self, sample: AudioRef, _at: Option<Instant>) -> anyhow::Result<()> {
            self.captured.lock().unwrap().push(sample.format());
            Ok(())
        }
    }

    #[test]
    fn audio_render_target_is_object_safe_and_usable_behind_shared_ref() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let sink: Box<dyn AudioRenderTarget> = Box::new(Recorder {
            captured: captured.clone(),
        });

        sink.play(
            AudioRef::Cas {
                hash: ContentHash::from_data(b"sample"),
                format: AudioFormatHint::Mp3,
            },
            None,
        )
        .expect("play should succeed");

        assert_eq!(*captured.lock().unwrap(), vec![AudioFormatHint::Mp3]);
    }
}
