//! FFI-free render + capture seam (see `docs/pcm.md` "The seam",
//! `docs/midi.md` "Render is a wire cue" / M2 "the ear is the sink's twin").
//!
//! Both directions of the app↔kernel media boundary live here as pure data:
//! `RenderCue` rides kernel→sink (score out), `CaptureBatch` rides ear→kernel
//! (room in), and `timebase` is the local phasor both sides keep musical time
//! with. No `alsa`/`pipewire`/`symphonia`, no `tokio`, no kernel-ward crates.
//! The kernel depends on it for orchestration; hardware emission *and
//! audition* live in the consuming binaries (`kaijutsu-app`'s Bevy sink + ear
//! today, an edge-node ALSA agent later).

use kaijutsu_cas::ContentHash;
use std::time::Duration;

pub mod capture;
pub use capture::{
    keep_at_ingest, CaptureBatch, CaptureError, CaptureEvent, CaptureRing, Tracker,
    MIDI_CAPTURE_MIME, MIDI_CAPTURE_VERSION,
};

pub mod clip;
pub use clip::{Clip, ClipError, CLIP_MIME, CLIP_VERSION};

pub mod clockin;
pub use clockin::{ClockEstimate, ClockEstimator, ClockEvent, PULSES_PER_BEAT};

pub mod timebase;
pub use timebase::{
    beat_onsets_in, stamp_age, BeatRef, LocalBeat, RefDisposition, Slew, REF_FOLD_MAX,
    REF_STALE_MAX,
};

/// A committed ABC score, rendered to MIDI at the sink (`docs/midi.md` "Render
/// is a wire cue"). The payload is the ABC text; a render sink that can render
/// ABC (the app) turns it into MIDI, a dumb sink ignores it.
pub const ABC_MIME: &str = "text/vnd.abc";

/// A transport flush directive (`stop`/`pause`): a [`RenderCue`] with this mime
/// and an empty payload tells every sink to drop its scheduled-but-unplayed
/// events and silence sounding notes, so the speculation lead's buffered phrase
/// doesn't play on after the clock stops. `lead` is `ZERO` (flush now).
pub const RENDER_FLUSH_MIME: &str = "application/vnd.kaijutsu.render-flush";

/// One render sink. Implemented in the app (Bevy) and, later, an edge-node
/// agent (ALSA). `&self` (not `&mut self`) because the Bevy sink spawns
/// entities via `Commands` rather than mutating sink state, and the ALSA
/// sink's handle lives behind internal mutability — see `docs/pcm.md`.
pub trait RenderSink: Send {
    /// Emit one render cue. The sink dispatches on `cue.mime` and schedules at
    /// `receipt + cue.lead` (`Duration::ZERO` == now).
    fn emit(&self, cue: RenderCue) -> anyhow::Result<()>;
}

/// A render directive crossing the wire / the seam to an off-box sink
/// (`docs/midi.md` "Render is a wire cue"; `docs/pcm.md` "How it converges").
/// Mime-keyed and content-agnostic: an audio sample, a clip record
/// (`docs/clips.md`), timed MIDI events, or ABC all ride this one directive and
/// the sink dispatches on `mime`. Generalizes the slice-3 play-now
/// `PlayAudio`/`AudioRef` pair. The wire never carries raw decoded PCM — the
/// payload is symbolic content (or encoded sample bytes for tiny inline
/// samples); decoding lives at the sink (Bevy decoders / Symphonia).
///
/// `Debug` is hand-written (not derived) so it NEVER formats the inline
/// payload bytes: a directive carrying an inline sample can be large, and a
/// stray `tracing::debug!(?flow)` deriving down to this type would otherwise
/// dump the whole buffer as an int array into a log line. We print the byte
/// *count* instead (gemini-pro review, 2026-07-01).
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RenderCue {
    /// MIME of `payload` — the sink's dispatch key. e.g. `audio/wav`,
    /// `audio/midi`, `text/vnd.abc`, `application/vnd.kaijutsu.clip+json`.
    pub mime: String,
    /// The symbolic content inline, or a CAS ref the sink resolves (the
    /// primary path under the speculation-lead prefetch — `docs/pcm.md`).
    pub payload: CuePayload,
    /// Relative schedule lead: the sink fires at `receipt + lead`. A
    /// *relative* `Duration` because a process-local `Instant` can't cross the
    /// wire; `Duration::ZERO` == play now (the old `PlayAudio` semantics).
    pub lead: Duration,
    /// Sender wallclock (ns since UNIX_EPOCH) at emission — mirrors
    /// `timebase::BeatRef::epoch_ns` (phase-align Slice 2). `0` = unstamped
    /// (an old peer, or a synthetic cue with no meaningful emission instant,
    /// e.g. a `RENDER_FLUSH` directive or `now_inline`). A sink back-dates
    /// `lead` by however old the cue reads on receipt (`age = now_epoch_ns -
    /// epoch_ns`) — a per-cue transfer-latency jump can't walk the render out
    /// of phase with the click, the same fix `BeatRef` got in Slice 3.
    /// `#[serde(default)]` so an old wire payload without this field still
    /// deserializes (additive schema evolution; wire-only, no CBOR at rest).
    #[serde(default)]
    pub epoch_ns: u64,
}

/// The two forms a cue's content takes on the wire: small content inline, or a
/// CAS hash the sink resolves from its cache / SFTP under the lead.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CuePayload {
    /// Symbolic content (ABC/clip JSON/MIDI) or a tiny encoded sample, inline.
    Inline(Vec<u8>),
    /// Larger content: fetch from CAS (the primary path — see "How it
    /// converges" in `docs/pcm.md`).
    Cas(ContentHash),
}

impl std::fmt::Debug for RenderCue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenderCue")
            .field("mime", &self.mime)
            .field("lead", &self.lead)
            .field("epoch_ns", &self.epoch_ns)
            .field("payload", &self.payload)
            .finish()
    }
}

impl std::fmt::Debug for CuePayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // The count, never the bytes — see the RenderCue type doc.
            CuePayload::Inline(bytes) => {
                write!(f, "Inline([{} bytes])", bytes.len())
            }
            CuePayload::Cas(hash) => f.debug_tuple("Cas").field(hash).finish(),
        }
    }
}

impl RenderCue {
    /// A play-now cue carrying inline bytes (`lead == ZERO`). The slice-5a
    /// shape of the old `kj play` directive.
    pub fn now_inline(mime: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self {
            mime: mime.into(),
            payload: CuePayload::Inline(bytes),
            lead: Duration::ZERO,
            epoch_ns: 0,
        }
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
    fn now_inline_builds_a_zero_lead_inline_cue() {
        let cue = RenderCue::now_inline("audio/wav", vec![1, 2, 3]);
        assert_eq!(cue.mime, "audio/wav");
        assert_eq!(cue.lead, Duration::ZERO, "play-now == zero lead");
        assert_eq!(cue.payload, CuePayload::Inline(vec![1, 2, 3]));
    }

    #[test]
    fn debug_elides_inline_payload_bytes() {
        // A directive's Debug must never dump the raw payload buffer (a stray
        // `debug!(?flow)` would otherwise log MB of int array). Derived Debug
        // would print `[1, 2, 3, 4, 5]`; the hand-written one prints the
        // count. This test fails on the derive.
        let cue = RenderCue {
            mime: "audio/wav".into(),
            payload: CuePayload::Inline(vec![1, 2, 3, 4, 5]),
            lead: Duration::from_millis(250),
            epoch_ns: 0,
        };
        let s = format!("{cue:?}");
        assert!(s.contains("[5 bytes]"), "debug shows the byte count: {s}");
        assert!(!s.contains("1, 2, 3"), "debug must NOT dump raw bytes: {s}");
        assert!(s.contains("audio/wav"), "debug still shows the mime: {s}");

        // Cas has no bytes to leak — but confirm it still Debugs its hash.
        let cas = RenderCue {
            mime: "audio/flac".into(),
            payload: CuePayload::Cas(ContentHash::from_data(b"x")),
            lead: Duration::ZERO,
            epoch_ns: 0,
        };
        let cs = format!("{cas:?}");
        assert!(cs.contains("Cas"), "cas debug names the variant: {cs}");
    }

    #[test]
    fn serde_round_trip_for_format_and_render_cue() {
        let format = AudioFormatHint::Wav;
        let json = serde_json::to_string(&format).expect("serialize format");
        let back: AudioFormatHint = serde_json::from_str(&json).expect("deserialize format");
        assert_eq!(back, format);

        // Inline payload + non-zero lead + a stamped epoch round-trips whole.
        let cue = RenderCue {
            mime: "audio/wav".into(),
            payload: CuePayload::Inline(vec![1, 2, 3]),
            lead: Duration::from_millis(500),
            epoch_ns: 123_456_789,
        };
        let json = serde_json::to_string(&cue).expect("serialize RenderCue");
        let back: RenderCue = serde_json::from_str(&json).expect("deserialize RenderCue");
        assert_eq!(back, cue);

        // Cas payload round-trips too.
        let cas = RenderCue {
            mime: "application/vnd.kaijutsu.clip+json".into(),
            payload: CuePayload::Cas(ContentHash::from_data(b"clip")),
            lead: Duration::ZERO,
            epoch_ns: 0,
        };
        let json = serde_json::to_string(&cas).expect("serialize cas cue");
        let back: RenderCue = serde_json::from_str(&json).expect("deserialize cas cue");
        assert_eq!(back, cas);
    }

    /// An old wire payload (pre-`epoch_ns` peer) has no `epoch_ns` key at all —
    /// `#[serde(default)]` must fill it with `0` (unstamped), the same
    /// additive-schema-evolution contract `BeatRef` got in Slice 3, not fail to
    /// parse. Built by round-tripping a real cue through `serde_json::Value`
    /// and stripping the key, rather than hand-writing the JSON shape, so the
    /// test doesn't depend on `Duration`'s exact serde representation.
    #[test]
    fn render_cue_without_epoch_ns_field_deserializes_to_zero() {
        let cue = RenderCue {
            mime: "audio/wav".into(),
            payload: CuePayload::Inline(vec![1, 2, 3]),
            lead: Duration::from_millis(500),
            epoch_ns: 999,
        };
        let mut value: serde_json::Value =
            serde_json::to_value(&cue).expect("serialize to Value");
        value.as_object_mut().expect("cue is a JSON object").remove("epoch_ns");
        let old_payload = serde_json::to_string(&value).expect("re-serialize the stripped value");

        let back: RenderCue = serde_json::from_str(&old_payload).expect("old payload still parses");
        assert_eq!(back.epoch_ns, 0, "missing field defaults to unstamped");
        assert_eq!(back.mime, cue.mime);
        assert_eq!(back.lead, cue.lead);
    }

    /// Object-safety + `&self`/interior-mutability shape check: a test-only
    /// sink stored as `Box<dyn RenderSink>`. `emit` takes `&self`, so the
    /// recorder needs interior mutability (a shared `Arc<Mutex<..>>` so the
    /// test can still observe what the boxed trait object captured).
    struct Recorder {
        captured: Arc<Mutex<Vec<String>>>,
    }

    impl RenderSink for Recorder {
        fn emit(&self, cue: RenderCue) -> anyhow::Result<()> {
            self.captured.lock().unwrap().push(cue.mime);
            Ok(())
        }
    }

    #[test]
    fn render_sink_is_object_safe_and_usable_behind_shared_ref() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let sink: Box<dyn RenderSink> = Box::new(Recorder {
            captured: captured.clone(),
        });

        sink.emit(RenderCue {
            mime: "audio/mpeg".into(),
            payload: CuePayload::Cas(ContentHash::from_data(b"sample")),
            lead: Duration::ZERO,
            epoch_ns: 0,
        })
        .expect("emit should succeed");

        assert_eq!(*captured.lock().unwrap(), vec!["audio/mpeg".to_string()]);
    }
}
