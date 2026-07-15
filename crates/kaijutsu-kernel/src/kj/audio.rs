//! `kj audio` — offline audio analysis, a new top-level noun seeding future
//! PCM/clip analysis work (docs/pcm.md). `beats` is its first verb: beat and
//! downbeat tracking via the Beat This! model (ISMIR 2024) through the
//! pure-Rust `beat-this` crate.
//!
//! `beat-this` runs inference on [rten] — the same pure-Rust ONNX backend
//! kaijutsu-index's `RtenEmbedder` uses (`beat-this`'s `ort` feature is
//! deliberately NOT enabled: no external ONNX Runtime dependency, and no
//! second inference backend to keep in sync). Both crates are pinned to
//! rten 0.24 so the workspace never carries two copies of the runtime.
//!
//! The kernel never touches audio hardware here — this is pure offline CPU
//! analysis (decode via symphonia, resample via rubato, infer via rten),
//! seconds of work for a several-minute track.

use std::path::{Path, PathBuf};

use beat_this::{BeatThis, RtenRuntime};
use clap::{Parser, Subcommand};
use kaijutsu_types::ContentType;

use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug)]
#[command(
    name = "audio",
    about = "Offline audio analysis (docs/pcm.md future work surface)",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct AudioArgs {
    #[command(subcommand)]
    command: AudioCommand,
}

#[derive(Subcommand, Debug)]
enum AudioCommand {
    /// Beat and downbeat tracking via the Beat This! (ISMIR 2024) model,
    /// run through the pure-Rust `beat-this` crate (rten inference — no
    /// external ONNX Runtime). Offline CPU work: expect low seconds per
    /// track, not real-time. Models load fresh on every invocation — no
    /// caching layer for this spike (see the `run_beats` doc comment).
    ///
    /// Model directory: `~/.local/share/kaijutsu/models/beat-this/`
    ///
    /// - `mel_spectrogram.onnx` — required
    ///
    /// - `beat_this.onnx` — preferred (full model)
    ///
    /// - `beat_this_small.onnx` — fallback if the full model is absent
    ///
    /// The model actually used is always named in the output — no silent
    /// full→small fallback without saying so. Missing files point at
    /// github.com/danigb/beat-this-rs: the `models/` dir in the repo for
    /// the mel + small models, and the GitHub release tag `model-large`
    /// for the full beat model.
    ///
    /// Accepts WAV/MP3/FLAC/OGG (symphonia decode, any input sample rate —
    /// resampled internally to the model's 22050 Hz via rubato).
    Beats {
        /// Path to the audio file to analyze (OS path, not a VFS path —
        /// mirrors `kj play`/`kj cas put`).
        path: String,
    },
}

/// Result of one `kj audio beats` analysis — the payload behind both the
/// human message and the `.data` object.
#[derive(Debug)]
pub(crate) struct BeatsResult {
    /// Which beat model produced this analysis: `"beat_this"` (full) or
    /// `"beat_this_small"` (fallback). Always surfaced — never silent.
    pub model_used: &'static str,
    /// Median-inter-beat-interval BPM (`beat_this::calculate_bpm`); `None`
    /// when fewer than two usable beat intervals were found.
    pub bpm: Option<f32>,
    /// Beat times in seconds, sorted, deduplicated.
    pub beats: Vec<f32>,
    /// Downbeat times in seconds, a sorted subset of `beats`.
    pub downbeats: Vec<f32>,
}

/// The beat-this model directory: `~/.local/share/kaijutsu/models/beat-this/`.
/// Same XDG root `kaish_kernel::xdg_data_home()` the server uses for the
/// kernel's own data dir (`crates/kaijutsu-server/src/rpc.rs`), joined the
/// same way kaijutsu-index's embedding models are found.
pub(crate) fn beat_this_model_dir() -> PathBuf {
    kaish_kernel::xdg_data_home()
        .join("kaijutsu")
        .join("models")
        .join("beat-this")
}

/// Run the beat-this pipeline against `audio_path`, resolving models from
/// `model_dir`. Factored out of the `kj audio beats` verb so both the CLI
/// dispatch and the integration test drive the exact same code path.
///
/// Synchronous/blocking (model load + rten inference is CPU-bound and can
/// take low seconds) — async callers MUST run this inside
/// `tokio::task::spawn_blocking`, mirroring the index embed path
/// (`kaijutsu-index`'s `Embedder` trait: "Methods are sync — callers should
/// use `spawn_blocking`").
///
/// Deliberately builds a fresh `BeatThis` (and reloads both ONNX graphs) on
/// every call — model load is tens of milliseconds against an analysis that
/// takes seconds, and this is a low-traffic spike verb. No caching /
/// resident-model machinery here on purpose; revisit if `kj audio beats`
/// becomes a hot path.
pub(crate) fn run_beats(model_dir: &Path, audio_path: &Path) -> Result<BeatsResult, String> {
    if !audio_path.is_file() {
        return Err(format!(
            "kj audio beats: {}: no such file",
            audio_path.display()
        ));
    }

    let mel_path = model_dir.join("mel_spectrogram.onnx");
    if !mel_path.is_file() {
        return Err(format!(
            "kj audio beats: required model missing: {} — download it from \
             github.com/danigb/beat-this-rs (the `models/` dir in the repo carries \
             the mel spectrogram model)",
            mel_path.display()
        ));
    }

    // Full model preferred; small is the documented fallback. Whichever is
    // chosen rides in the result so the caller never has to guess.
    let full_path = model_dir.join("beat_this.onnx");
    let small_path = model_dir.join("beat_this_small.onnx");
    let (beat_path, model_used): (PathBuf, &'static str) = if full_path.is_file() {
        (full_path, "beat_this")
    } else if small_path.is_file() {
        (small_path, "beat_this_small")
    } else {
        return Err(format!(
            "kj audio beats: no beat model found — need {} (full, preferred; GitHub \
             release tag `model-large` on github.com/danigb/beat-this-rs) or {} \
             (small, fallback; the `models/` dir in the repo) under {}",
            full_path.display(),
            small_path.display(),
            model_dir.display(),
        ));
    };

    let runtime = RtenRuntime;
    let mut bt = BeatThis::new(&runtime, &mel_path, &beat_path)
        .map_err(|e| format!("kj audio beats: loading models: {e}"))?;
    let analysis = bt
        .analyze_file(audio_path)
        .map_err(|e| format!("kj audio beats: analyzing {}: {e}", audio_path.display()))?;
    let bpm = beat_this::calculate_bpm(&analysis);

    Ok(BeatsResult {
        model_used,
        bpm,
        beats: analysis.beats,
        downbeats: analysis.downbeats,
    })
}

impl KjDispatcher {
    pub(crate) async fn dispatch_audio(&self, argv: &[String], _caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<AudioArgs>();
        }
        let parsed = match AudioArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj audio: {e}"));
            }
        };

        match parsed.command {
            AudioCommand::Beats { path } => self.audio_beats(path).await,
        }
    }

    async fn audio_beats(&self, path: String) -> KjResult {
        let audio_path = PathBuf::from(&path);
        let model_dir = beat_this_model_dir();

        // CPU-bound (model load + inference) — off the async runtime, same
        // pattern as the index embed/synth path (runtime/kj_builtin.rs).
        let join_result = tokio::task::spawn_blocking({
            let audio_path = audio_path.clone();
            move || run_beats(&model_dir, &audio_path)
        })
        .await;

        let result = match join_result {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return KjResult::Err(e),
            Err(e) => {
                return KjResult::Err(format!(
                    "kj audio beats: analysis task panicked: {e}"
                ));
            }
        };

        let bpm_str = result
            .bpm
            .map(|b| format!("{b:.1}"))
            .unwrap_or_else(|| "n/a".to_string());
        let preview: Vec<String> = result
            .beats
            .iter()
            .take(8)
            .map(|b| format!("{b:.3}"))
            .collect();

        let message = format!(
            "{path}: model={} bpm={} beats={} downbeats={} first_beats=[{}]",
            result.model_used,
            bpm_str,
            result.beats.len(),
            result.downbeats.len(),
            preview.join(", "),
        );

        let data = serde_json::json!({
            "file": path,
            "model": result.model_used,
            "bpm": result.bpm,
            "beats": result.beats,
            "downbeats": result.downbeats,
        });

        KjResult::ok_with_data(message, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal 44-byte-header PCM WAV: mono, 16-bit, `sample_rate`
    /// Hz, `num_samples` samples of `data` (already `i16`-encoded LE bytes
    /// appended by the caller). No `hound`/dev-dep — hand-rolled per the
    /// canonical RIFF/WAVE layout so the test carries no new dependency.
    fn wav_header(sample_rate: u32, num_samples: u32) -> Vec<u8> {
        let bits_per_sample: u16 = 16;
        let num_channels: u16 = 1;
        let byte_rate = sample_rate * num_channels as u32 * (bits_per_sample as u32 / 8);
        let block_align: u16 = num_channels * (bits_per_sample / 8);
        let data_size = num_samples * (bits_per_sample as u32 / 8);
        let riff_size = 36 + data_size;

        let mut h = Vec::with_capacity(44);
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&riff_size.to_le_bytes());
        h.extend_from_slice(b"WAVE");
        h.extend_from_slice(b"fmt ");
        h.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size (PCM)
        h.extend_from_slice(&1u16.to_le_bytes()); // audio format = PCM
        h.extend_from_slice(&num_channels.to_le_bytes());
        h.extend_from_slice(&sample_rate.to_le_bytes());
        h.extend_from_slice(&byte_rate.to_le_bytes());
        h.extend_from_slice(&block_align.to_le_bytes());
        h.extend_from_slice(&bits_per_sample.to_le_bytes());
        h.extend_from_slice(b"data");
        h.extend_from_slice(&data_size.to_le_bytes());
        h
    }

    /// Synthesize a ~12s, 44.1kHz mono click track: a 10ms 1kHz sine burst
    /// every 0.5s (120 BPM). A short linear attack/decay ramp on each burst
    /// avoids a hard edge/click that would otherwise pollute the spectrum
    /// with broadband energy unrelated to the 1kHz tone.
    fn synth_click_track_wav() -> Vec<u8> {
        const SAMPLE_RATE: u32 = 44_100;
        const DURATION_SECS: f32 = 12.0;
        const INTERVAL_SECS: f32 = 0.5;
        const BURST_SECS: f32 = 0.010;
        const TONE_HZ: f32 = 1000.0;
        const AMPLITUDE: f32 = 0.8;

        let total_samples = (DURATION_SECS * SAMPLE_RATE as f32) as u32;
        let mut samples = vec![0i16; total_samples as usize];
        let burst_len = (BURST_SECS * SAMPLE_RATE as f32) as usize;
        let ramp_len = burst_len / 4;

        let mut beat_time = 0.0f32;
        while beat_time < DURATION_SECS {
            let start = (beat_time * SAMPLE_RATE as f32) as usize;
            for i in 0..burst_len {
                let idx = start + i;
                if idx >= samples.len() {
                    break;
                }
                let t = i as f32 / SAMPLE_RATE as f32;
                let envelope = if i < ramp_len {
                    i as f32 / ramp_len as f32
                } else if i >= burst_len - ramp_len {
                    (burst_len - i) as f32 / ramp_len as f32
                } else {
                    1.0
                };
                let sample =
                    envelope * AMPLITUDE * (2.0 * std::f32::consts::PI * TONE_HZ * t).sin();
                samples[idx] = (sample * i16::MAX as f32) as i16;
            }
            beat_time += INTERVAL_SECS;
        }

        let mut bytes = wav_header(SAMPLE_RATE, total_samples);
        for s in samples {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        bytes
    }

    fn median(values: &mut [f32]) -> f32 {
        values.sort_by(|a, b| a.total_cmp(b));
        let n = values.len();
        if n.is_multiple_of(2) {
            (values[n / 2 - 1] + values[n / 2]) / 2.0
        } else {
            values[n / 2]
        }
    }

    /// TDD anchor: `run_beats` (the same function `kj audio beats` calls)
    /// against a synthetic 120 BPM click track must recover a beat grid
    /// whose median inter-beat interval is close to the true 0.5s spacing.
    /// Asserts the median interval directly (not the `bpm` field) — bpm can
    /// octave-flip (60/120/240 all "explain" a click train), the median
    /// interval doesn't.
    ///
    /// Gated on the model files existing locally
    /// (`~/.local/share/kaijutsu/models/beat-this/`) — skips with an
    /// `eprintln!` when absent rather than failing CI environments that
    /// haven't fetched the (83 MB) models.
    #[test]
    fn beats_recovers_120_bpm_click_track() {
        let model_dir = beat_this_model_dir();
        if !model_dir.join("mel_spectrogram.onnx").is_file()
            || !(model_dir.join("beat_this.onnx").is_file()
                || model_dir.join("beat_this_small.onnx").is_file())
        {
            eprintln!(
                "skipping beats_recovers_120_bpm_click_track: beat-this models not \
                 found under {} (see github.com/danigb/beat-this-rs)",
                model_dir.display()
            );
            return;
        }

        let dir = tempfile::tempdir().expect("tmpdir");
        let wav_path = dir.path().join("click_120bpm.wav");
        std::fs::write(&wav_path, synth_click_track_wav()).expect("write synthetic wav");

        let result = run_beats(&model_dir, &wav_path).expect("beat analysis should succeed");

        assert!(!result.beats.is_empty(), "expected at least one detected beat");
        assert!(
            result.beats.windows(2).all(|w| w[1] > w[0]),
            "beats must be strictly monotonically increasing: {:?}",
            result.beats
        );

        let mut intervals: Vec<f32> = result.beats.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(
            !intervals.is_empty(),
            "need at least two beats to measure an interval"
        );
        let median_interval = median(&mut intervals);
        assert!(
            (median_interval - 0.5).abs() < 0.05,
            "median inter-beat interval {median_interval:.3}s should be within 0.05s of the \
             true 0.5s (120 BPM) click spacing; beats={:?}",
            result.beats
        );
    }

    // ── error paths (deterministic — no real model files required) ──────────

    #[test]
    fn run_beats_missing_audio_file_errors_with_path() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let result = run_beats(&dir.path().join("models"), &dir.path().join("nope.wav"));
        let err = result.expect_err("nonexistent audio file must error");
        assert!(err.contains("nope.wav"), "error names the missing path: {err}");
        assert!(err.contains("no such file"), "error is explicit: {err}");
    }

    #[test]
    fn run_beats_missing_mel_model_errors_with_download_hint() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let audio_path = dir.path().join("silence.wav");
        std::fs::write(&audio_path, wav_header(44_100, 0)).expect("write empty wav");
        let model_dir = dir.path().join("models"); // does not exist

        let result = run_beats(&model_dir, &audio_path);
        let err = result.expect_err("missing mel model must error");
        assert!(
            err.contains("mel_spectrogram.onnx"),
            "error names the exact missing file: {err}"
        );
        assert!(
            err.contains("github.com/danigb/beat-this-rs"),
            "error cites the download source: {err}"
        );
    }

    #[test]
    fn run_beats_missing_beat_model_names_both_full_and_small_candidates() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let audio_path = dir.path().join("silence.wav");
        std::fs::write(&audio_path, wav_header(44_100, 0)).expect("write empty wav");
        let model_dir = dir.path().join("models");
        std::fs::create_dir_all(&model_dir).expect("create model dir");
        // Only the mel model is present — no beat_this.onnx / beat_this_small.onnx.
        std::fs::write(model_dir.join("mel_spectrogram.onnx"), b"not a real onnx file")
            .expect("write stub mel model");

        let result = run_beats(&model_dir, &audio_path);
        let err = result.expect_err("missing beat model must error");
        assert!(err.contains("beat_this.onnx"), "names the preferred full model: {err}");
        assert!(err.contains("beat_this_small.onnx"), "names the fallback model: {err}");
        assert!(
            err.contains("model-large"),
            "cites the GitHub release tag for the full model: {err}"
        );
    }

    // ── dispatch-level: help + no-context-required routing ──────────────────

    #[tokio::test]
    async fn dispatch_audio_bare_renders_help() {
        let d = crate::kj::test_helpers::test_dispatcher().await;
        let caller = crate::kj::test_helpers::test_caller();
        let result = d.dispatch(&["audio".to_string()], &caller).await;
        assert!(result.is_ok(), "bare `kj audio` should render help: {result:?}");
        assert!(
            result.message().to_lowercase().contains("usage"),
            "expected clap help text: {}",
            result.message()
        );
    }

    /// `kj audio beats` on a host path never requires an active context —
    /// same exemption as `kj play`/`kj cas` (mirrors the mod.rs routing
    /// comment). A caller with no joined context still reaches the verb
    /// and gets the (model-independent) missing-file error, not the
    /// generic "no active context joined" refusal.
    #[tokio::test]
    async fn dispatch_audio_beats_needs_no_active_context() {
        let d = crate::kj::test_helpers::test_dispatcher().await;
        let caller = kaijutsu_types::PrincipalId::new();
        let unjoined = super::super::KjCaller {
            principal_id: caller,
            context_id: None,
            session_id: kaijutsu_types::SessionId::new(),
            confirmed: false,
            rc_depth: 0,
            privileged: false,
        };
        let result = d
            .dispatch(
                &[
                    "audio".to_string(),
                    "beats".to_string(),
                    "/nonexistent/path/to/track.wav".to_string(),
                ],
                &unjoined,
            )
            .await;
        assert!(!result.is_ok(), "nonexistent file should error: {result:?}");
        assert!(
            result.message().contains("no such file"),
            "should reach the verb's own file-not-found error, not the active-context \
             gate: {}",
            result.message()
        );
    }
}
