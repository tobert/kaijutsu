//! `kj audio` — offline audio analysis, a new top-level noun seeding future
//! PCM/clip analysis work (docs/pcm.md). `beats` is its first verb: beat and
//! downbeat tracking via the Beat This! model (ISMIR 2024) through the
//! pure-Rust `beat-this` crate.
//!
//! `beat-this` runs inference through RTen, a pure-Rust runtime for ONNX
//! models. Its `ort` feature is disabled, so no external ONNX Runtime library
//! is required. Semantic embeddings use a separate HTTP embedding service.
//!
//! The kernel never touches audio hardware here — this is pure offline CPU
//! analysis (decode via symphonia, resample via rubato, infer via rten),
//! seconds of work for a several-minute track.
//!
//! `render` is the converse: notation or MIDI to a WAV clip in the CAS, through
//! the pure-Rust `rustysynth` SoundFont synthesizer, so a model that reads
//! audio can hear what a track played. It is offline CPU work like `beats`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use beat_this::{BeatThis, RtenRuntime};
use clap::{Parser, Subcommand};
use kaijutsu_cas::ContentStore;
use kaijutsu_types::{BlockSnapshot, ContentType};

use super::effect::{Classify, Effect};
use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug)]
#[command(
    name = "audio",
    about = "Inspect audio nodes, keep retained MIDI, and analyze audio",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct AudioArgs {
    #[command(subcommand)]
    command: AudioCommand,
}

#[derive(Subcommand, Debug)]
enum AudioCommand {
    /// Read a connected audio node's device inventory and retained coverage.
    Devices {
        /// Exact connected peer name, such as audio/moltar.
        #[arg(long)]
        node: String,
    },
    /// Protect recent MIDI in daemon RAM, then upload it to kernel CAS.
    Keep {
        /// Exact connected peer name, such as audio/moltar.
        #[arg(long)]
        node: String,
        /// Source address from the node inventory.
        #[arg(long)]
        source: String,
        /// Stream generation UUID from the node inventory.
        #[arg(long)]
        generation: uuid::Uuid,
        /// Seconds preceding the current head (1–60); incomplete coverage is refused.
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=60))]
        seconds: u32,
    },
    /// Read a keep job's state, publication hash, or last error.
    KeepStatus {
        /// Job UUID returned by keep.
        id: uuid::Uuid,
    },
    /// Retry a failed upload without selecting a different retained window.
    KeepRetry {
        /// Job UUID returned by keep.
        id: uuid::Uuid,
    },
    /// Cancel a keep and release its protected material after acknowledgement.
    KeepCancel {
        /// Job UUID returned by keep.
        id: uuid::Uuid,
    },
    /// Detect beats and downbeats in WAV, MP3, FLAC, or OGG audio.
    /// Models must be installed under ~/.local/share/kaijutsu/models/beat-this/.
    /// Reports which model was used; the small model is used when the full
    /// model is absent. Missing models produce download instructions.
    Beats {
        /// Host path to the audio file; VFS paths are not accepted.
        path: String,
    },
    /// Render notation or MIDI to a 16-bit mono WAV clip in the CAS.
    /// `kj audio render <hash>...` mixes each source from time 0; a source is
    /// the CAS hash of text/vnd.abc or audio/midi content. `kj audio render
    /// --track bass --phrases 2` renders the last 2 phrases the track
    /// committed, up to its playhead. Prints the clip's hash and a summary.
    /// Needs a SoundFont at ~/.local/share/kaijutsu/models/soundfont/default.sf2
    /// unless --soundfont is given. Output is capped at 120 seconds.
    #[command(group(clap::ArgGroup::new("input").args(["sources", "track"]).required(true)))]
    Render {
        /// CAS hashes of text/vnd.abc or audio/midi content, mixed from time 0.
        sources: Vec<String>,
        /// Render the committed notation of this live track instead of hashes.
        /// A track that committed nothing in the window is refused.
        #[arg(long)]
        track: Option<String>,
        /// With --track: how many phrases before the playhead to render.
        #[arg(long, requires = "track", default_value_t = 1, value_parser = clap::value_parser!(u64).range(1..))]
        phrases: u64,
        /// Host path to an .sf2 SoundFont; the default is the model-directory file named above.
        #[arg(long)]
        soundfont: Option<String>,
        /// Sample rate in Hz, 8000 to 48000.
        #[arg(long, default_value_t = 16000)]
        rate: u32,
        /// Milliseconds of release tail after the last note.
        #[arg(long, default_value_t = 500)]
        tail_ms: u32,
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

/// Resolve beat models under `$XDG_DATA_HOME/kaijutsu/models/beat-this/`,
/// defaulting to `~/.local/share/kaijutsu/models/beat-this/`.
pub(crate) fn beat_this_model_dir() -> PathBuf {
    kaish_kernel::xdg_data_home()
        .join("kaijutsu")
        .join("models")
        .join("beat-this")
}

/// Load both ONNX graphs and analyze `audio_path` using models in `model_dir`.
/// The graphs live for this call; there is no resident model cache.
/// Async callers must use `tokio::task::spawn_blocking` for loading and inference.
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

// ── render ────────────────────────────────────────────────────────────────

const ABC_MIME: &str = "text/vnd.abc";
const MIDI_MIME: &str = "audio/midi";
const WAV_MIME: &str = "audio/wav";
/// Longest clip `render` produces, in seconds.
pub(crate) const RENDER_MAX_SECONDS: u32 = 120;
/// Sample rates `render` accepts, in Hz.
const RENDER_RATES: std::ops::RangeInclusive<u32> = 8000..=48000;
/// -3 dBFS as a linear peak: the level a clipping mix is scaled down to.
const PEAK_CEILING: f32 = 0.707_945_8;
/// `rustysynth` refuses synthesis rates below this; lower output rates are
/// synthesized at twice the rate and averaged down.
const SYNTH_MIN_RATE: u32 = 16_000;

/// The synthesizer behind `render`: one MIDI file in, mono samples at `rate`
/// out, `tail_ms` of release after the last event.
pub(crate) trait MidiSynth: Send {
    fn render(&mut self, midi: &[u8], rate: u32, tail_ms: u32) -> Result<Vec<f32>, String>;
}

/// Builds the synthesizer from a SoundFont file path.
pub(crate) type SynthLoader = Box<dyn FnOnce(&Path) -> Result<Box<dyn MidiSynth>, String> + Send>;

/// Parsed `kj audio render` arguments.
pub(crate) struct RenderRequest {
    pub sources: Vec<String>,
    pub track: Option<String>,
    pub phrases: u64,
    pub soundfont: Option<String>,
    pub rate: u32,
    pub tail_ms: u32,
}

/// One MIDI file to synthesize, starting `offset_samples` into the clip.
#[derive(Debug, PartialEq)]
pub(crate) struct RenderPart {
    pub offset_samples: usize,
    pub midi: Vec<u8>,
}

/// Resolve the SoundFont under `$XDG_DATA_HOME/kaijutsu/models/soundfont/`,
/// defaulting to `~/.local/share/kaijutsu/models/soundfont/default.sf2`.
pub(crate) fn default_soundfont_path() -> PathBuf {
    kaish_kernel::xdg_data_home()
        .join("kaijutsu")
        .join("models")
        .join("soundfont")
        .join("default.sf2")
}

/// Convert ABC text to one MIDI file per tune. Each tune keeps its own
/// `%%MIDI channel` and program handling (`kaijutsu_abc::to_midi`).
pub(crate) fn abc_to_midis(abc: &str, label: &str) -> Result<Vec<Vec<u8>>, String> {
    let parsed = kaijutsu_abc::parse(abc);
    if let Some(error) = parsed
        .feedback
        .iter()
        .find(|f| f.level == kaijutsu_abc::feedback::FeedbackLevel::Error)
    {
        return Err(format!(
            "kj audio render: {label}: ABC does not parse: {}",
            error.message
        ));
    }
    if parsed.value.is_empty() {
        return Err(format!("kj audio render: {label}: ABC holds no tune"));
    }
    Ok(parsed
        .value
        .iter()
        .map(|tune| kaijutsu_abc::to_midi(tune, &kaijutsu_abc::MidiParams::default()))
        .collect())
}

/// The committed ABC cells whose tick lies in `[playhead - span, playhead)`,
/// oldest first (ties keep block order).
pub(crate) fn window_cells(blocks: &[BlockSnapshot], playhead: i64, span: i64) -> Vec<Cell<'_>> {
    let start = playhead.saturating_sub(span);
    let mut cells: Vec<Cell<'_>> = blocks
        .iter()
        .filter(|b| b.content_type == ContentType::Abc)
        .filter_map(|b| {
            let tick = b.tick?.get();
            if tick < start || tick >= playhead {
                return None;
            }
            let midi = blocks
                .iter()
                .find(|a| a.role == kaijutsu_types::Role::Asset && a.parent_id.as_ref() == Some(&b.id))
                .map(|a| a.content.trim());
            Some(Cell { tick, abc: b.content.as_str(), midi })
        })
        .collect();
    cells.sort_by_key(|cell| cell.tick);
    cells
}

/// One committed cell of a track's score: its notation, and the CAS hash of
/// the artifact it was played from when the score holds one under it. A cell
/// that repeats an earlier phrase holds an artifact and no notation.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Cell<'a> {
    pub(crate) tick: i64,
    pub(crate) abc: &'a str,
    pub(crate) midi: Option<&'a str>,
}

/// Samples from the window start to `tick`, one tick being one beat of
/// `beat_period`.
pub(crate) fn cell_offset_samples(
    tick: i64,
    window_start: i64,
    beat_period: Duration,
    rate: u32,
) -> usize {
    let ticks = tick.saturating_sub(window_start).max(0) as u128;
    let samples = ticks
        .saturating_mul(beat_period.as_nanos())
        .saturating_mul(rate as u128)
        / 1_000_000_000;
    usize::try_from(samples).unwrap_or(usize::MAX)
}

/// Select and place the last `phrases` phrases a track committed. A window
/// with no committed notation is refused.
pub(crate) fn track_parts(
    track: &str,
    blocks: &[BlockSnapshot],
    playhead: i64,
    beats_per_phrase: u64,
    beat_period: Duration,
    phrases: u64,
    rate: u32,
    load_midi: &dyn Fn(&str) -> Result<Option<Vec<u8>>, String>,
) -> Result<(Vec<RenderPart>, Vec<String>), String> {
    if beats_per_phrase == 0 {
        return Err(format!(
            "kj audio render: track '{track}' has no phrase length (beats_per_phrase is 0)"
        ));
    }
    let span_beats = phrases
        .checked_mul(beats_per_phrase)
        .ok_or_else(|| format!("kj audio render: --phrases {phrases} is too large"))?;
    let span = i64::try_from(span_beats)
        .map_err(|_| format!("kj audio render: --phrases {phrases} is too large"))?;
    let max_ns = RENDER_MAX_SECONDS as u128 * 1_000_000_000;
    let phrase_ns = (beats_per_phrase as u128).checked_mul(beat_period.as_nanos());
    let window_ns = phrase_ns.and_then(|p| (phrases as u128).checked_mul(p));
    if window_ns.is_none_or(|ns| ns > max_ns) {
        let detail = match window_ns {
            Some(ns) => format!("{:.1} s", ns as f64 / 1_000_000_000.0),
            None => "more than the limit".to_string(),
        };
        let max_hint = phrase_ns
            .filter(|&p| p > 0)
            .map(|p| max_ns / p)
            .filter(|&m| m >= 1)
            .map(|m| format!(" (at most {m} phrase(s) at this tempo)"))
            .unwrap_or_default();
        return Err(format!(
            "kj audio render: --phrases {phrases} spans {detail}, over the {RENDER_MAX_SECONDS} s cap{max_hint}"
        ));
    }
    let nothing = || format!("kj audio render: track '{track}' committed nothing in the last {phrases} phrase(s)");
    let mut parts = Vec::new();
    let mut labels = Vec::new();
    // A cell renders from the MIDI it was played from, which is what the
    // track sounded like at the track's tempo. Without one, its notation is
    // rendered; a cell with neither played nothing.
    // The clip starts at the first cell that played, with no silence before it.
    let mut origin = None;
    for cell in window_cells(blocks, playhead, span) {
        let label = format!("{track}@{}", cell.tick);
        let played = match cell.midi {
            Some(hash) => load_midi(hash)?,
            None => None,
        };
        if played.is_none() && cell.abc.trim().is_empty() {
            match cell.midi {
                // A cell that repeats a phrase holds only its artifact.
                Some(hash) => {
                    return Err(format!(
                        "kj audio render: {label}: the cell holds no notation and the CAS does not hold its MIDI artifact {hash}"
                    ));
                }
                None => continue,
            }
        }
        let offset_samples = cell_offset_samples(cell.tick, *origin.get_or_insert(cell.tick), beat_period, rate);
        match played {
            Some(midi) => parts.push(RenderPart { offset_samples, midi }),
            None => {
                for midi in abc_to_midis(cell.abc, &label)? {
                    parts.push(RenderPart { offset_samples, midi });
                }
            }
        }
        labels.push(label);
    }
    if parts.is_empty() {
        return Err(nothing());
    }
    Ok((parts, labels))
}

/// Scale `samples` down to -3 dBFS when the peak exceeds full scale. A quiet
/// mix, including silence, is left as it is.
pub(crate) fn protect_clipping(samples: &mut [f32]) {
    let peak = samples.iter().fold(0.0f32, |p, s| p.max(s.abs()));
    if peak > 1.0 {
        let gain = PEAK_CEILING / peak;
        samples.iter_mut().for_each(|s| *s *= gain);
    }
}

/// Synthesize every part and sum them into one mono clip, refusing a clip
/// longer than [`RENDER_MAX_SECONDS`].
pub(crate) fn render_mix(
    synth: &mut dyn MidiSynth,
    parts: &[RenderPart],
    rate: u32,
    tail_ms: u32,
) -> Result<Vec<f32>, String> {
    let cap = RENDER_MAX_SECONDS as usize * rate as usize;
    let over = |samples: usize| {
        format!(
            "kj audio render: the clip would run {:.1} s, over the {RENDER_MAX_SECONDS} s cap; \
             render a shorter source or fewer phrases",
            samples as f64 / rate as f64
        )
    };
    let mut mix: Vec<f32> = Vec::new();
    for part in parts {
        if part.offset_samples >= cap {
            return Err(over(part.offset_samples));
        }
        let audio = synth.render(&part.midi, rate, tail_ms)?;
        let end = part.offset_samples + audio.len();
        if end > cap {
            return Err(over(end));
        }
        if mix.len() < end {
            mix.resize(end, 0.0);
        }
        for (slot, sample) in mix[part.offset_samples..end].iter_mut().zip(&audio) {
            *slot += sample;
        }
    }
    Ok(mix)
}

/// Build a minimal 44-byte-header PCM WAV header: mono, 16-bit, `sample_rate`
/// Hz, `num_samples` samples. The caller appends the little-endian `i16` data.
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

/// Encode mono float samples (clamped to full scale) as a 16-bit PCM WAV.
pub(crate) fn encode_wav(samples: &[f32], rate: u32) -> Vec<u8> {
    let mut bytes = wav_header(rate, samples.len() as u32);
    bytes.reserve(samples.len() * 2);
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    bytes
}

/// Synthesizer backed by a SoundFont, through `rustysynth`.
struct SoundFontSynth {
    font: std::sync::Arc<rustysynth::SoundFont>,
}

impl MidiSynth for SoundFontSynth {
    fn render(&mut self, midi: &[u8], rate: u32, tail_ms: u32) -> Result<Vec<f32>, String> {
        let file = rustysynth::MidiFile::new(&mut std::io::Cursor::new(midi))
            .map_err(|e| format!("kj audio render: reading MIDI: {e}"))?;
        let seconds = file.get_length() + tail_ms as f64 / 1000.0;
        if seconds > RENDER_MAX_SECONDS as f64 {
            return Err(format!(
                "kj audio render: the clip would run {seconds:.1} s, over the \
                 {RENDER_MAX_SECONDS} s cap; render a shorter source"
            ));
        }
        let halve = rate < SYNTH_MIN_RATE;
        let synth_rate = if halve { rate * 2 } else { rate };
        let settings = rustysynth::SynthesizerSettings::new(synth_rate as i32);
        let synth = rustysynth::Synthesizer::new(&self.font, &settings)
            .map_err(|e| format!("kj audio render: starting the synthesizer: {e}"))?;
        let mut sequencer = rustysynth::MidiFileSequencer::new(synth);
        sequencer.play(&std::sync::Arc::new(file), false);
        let mut frames = (seconds * synth_rate as f64).ceil() as usize;
        if halve {
            frames += frames % 2;
        }
        let mut left = vec![0.0f32; frames];
        let mut right = vec![0.0f32; frames];
        sequencer.render(&mut left, &mut right);
        Ok(if halve {
            left.chunks_exact(2)
                .zip(right.chunks_exact(2))
                .map(|(l, r)| (l[0] + l[1] + r[0] + r[1]) / 4.0)
                .collect()
        } else {
            left.iter().zip(&right).map(|(l, r)| (l + r) / 2.0).collect()
        })
    }
}

fn load_soundfont_synth(path: &Path) -> Result<Box<dyn MidiSynth>, String> {
    let file = std::fs::File::open(path)
        .map_err(|e| format!("kj audio render: opening SoundFont {}: {e}", path.display()))?;
    let font = rustysynth::SoundFont::new(&mut std::io::BufReader::new(file))
        .map_err(|e| format!("kj audio render: reading SoundFont {}: {e}", path.display()))?;
    Ok(Box::new(SoundFontSynth { font: std::sync::Arc::new(font) }))
}

impl KjDispatcher {
    /// Sources named by CAS hash: each is ABC text or MIDI, recorded as such.
    fn render_hash_parts(&self, sources: &[String]) -> Result<Vec<RenderPart>, String> {
        let cas = self.kernel().cas();
        let mut parts = Vec::new();
        for source in sources {
            let hash = source
                .parse::<kaijutsu_cas::ContentHash>()
                .map_err(|e| format!("kj audio render: invalid hash {source}: {e}"))?;
            let reference = cas
                .inspect(&hash)
                .map_err(|e| format!("kj audio render: CAS error for {source}: {e}"))?
                .ok_or_else(|| format!("kj audio render: not found in CAS: {source}"))?;
            let mime = reference.mime_type.as_str();
            if mime != ABC_MIME && mime != MIDI_MIME {
                return Err(format!(
                    "kj audio render: {source} is recorded in the CAS as '{mime}'; \
                     a source must be {ABC_MIME} or {MIDI_MIME}"
                ));
            }
            let bytes = cas
                .retrieve(&hash)
                .map_err(|e| format!("kj audio render: CAS error for {source}: {e}"))?
                .ok_or_else(|| format!("kj audio render: not found in CAS: {source}"))?;
            if mime == MIDI_MIME {
                parts.push(RenderPart { offset_samples: 0, midi: bytes });
            } else {
                let text = String::from_utf8(bytes)
                    .map_err(|_| format!("kj audio render: {source}: ABC is not valid UTF-8"))?;
                for midi in abc_to_midis(&text, source)? {
                    parts.push(RenderPart { offset_samples: 0, midi });
                }
            }
        }
        Ok(parts)
    }

    /// The bytes `hash` names when the CAS records them as MIDI, else `None`.
    fn cas_midi(&self, hash: &str) -> Result<Option<Vec<u8>>, String> {
        let Ok(hash) = hash.parse::<kaijutsu_cas::ContentHash>() else { return Ok(None) };
        let cas = self.kernel().cas();
        let recorded = cas.inspect(&hash).map_err(|e| format!("kj audio render: CAS error for {hash}: {e}"))?;
        if recorded.is_none_or(|r| r.mime_type != MIDI_MIME) {
            return Ok(None);
        }
        cas.retrieve(&hash).map_err(|e| format!("kj audio render: CAS error for {hash}: {e}"))
    }

    /// The last `phrases` phrases of a live track's score context.
    async fn render_track_parts(
        &self,
        track: &str,
        phrases: u64,
        rate: u32,
    ) -> Result<(Vec<RenderPart>, Vec<String>), String> {
        let Some(reply) = self.kernel().request_track_snapshot() else {
            return Err(format!(
                "kj audio render: no beat scheduler is running, so track '{track}' has no live state"
            ));
        };
        let snapshots = reply.await.map_err(|_| {
            "kj audio render: the beat scheduler dropped the snapshot request without a reply"
                .to_string()
        })?;
        let Some(snapshot) = snapshots.iter().find(|s| s.id.as_str() == track) else {
            let live: Vec<&str> = snapshots.iter().map(|s| s.id.as_str()).collect();
            let live_list = if live.is_empty() { "none".to_string() } else { live.join(", ") };
            let persisted = if track.is_empty() {
                None
            } else {
                self.kernel_db()
                    .lock()
                    .get_track(track)
                    .map_err(|e| format!("kj audio render: checking track '{track}': {e}"))?
            };
            if persisted.is_some() {
                return Err(format!(
                    "kj audio render: track '{track}' is dormant (not live in the beat scheduler; live tracks: {live_list})"
                ));
            }
            return Err(format!(
                "kj audio render: unknown track '{track}' (live tracks: {live_list})"
            ));
        };
        let blocks = self
            .block_store()
            .block_snapshots(snapshot.score_context)
            .map_err(|e| format!("kj audio render: reading the score of track '{track}': {e}"))?;
        track_parts(
            track,
            &blocks,
            snapshot.playhead,
            snapshot.beats_per_phrase,
            snapshot.period,
            phrases,
            rate,
            &|hash| self.cas_midi(hash),
        )
    }

    /// Render sources or a track window to a WAV clip in the CAS. `loader`
    /// builds the synthesizer from the SoundFont path.
    pub(crate) async fn audio_render(&self, req: RenderRequest, loader: SynthLoader) -> KjResult {
        if !RENDER_RATES.contains(&req.rate) {
            return KjResult::Err(format!(
                "kj audio render: --rate {} is outside {}..={} Hz",
                req.rate,
                RENDER_RATES.start(),
                RENDER_RATES.end()
            ));
        }
        let resolved = match &req.track {
            Some(track) => self.render_track_parts(track, req.phrases, req.rate).await,
            None => self.render_hash_parts(&req.sources).map(|parts| (parts, req.sources.clone())),
        };
        let (parts, sources) = match resolved {
            Ok(r) => r,
            Err(e) => return KjResult::Err(e),
        };

        let font = req
            .soundfont
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(default_soundfont_path);
        if !font.is_file() {
            return KjResult::Err(format!(
                "kj audio render: no SoundFont at {} — install a General MIDI .sf2 file there \
                 or pass --soundfont <path>",
                font.display()
            ));
        }

        let cas = self.kernel().cas().clone();
        let (rate, tail_ms) = (req.rate, req.tail_ms);
        let joined = tokio::task::spawn_blocking(move || {
            let mut synth = loader(&font)?;
            let mut mix = render_mix(synth.as_mut(), &parts, rate, tail_ms)?;
            protect_clipping(&mut mix);
            let seconds = mix.len() as f64 / rate as f64;
            let wav = encode_wav(&mix, rate);
            let bytes = wav.len();
            let hash = cas
                .store(&wav, WAV_MIME)
                .map(|h| h.to_string())
                .map_err(|e| format!("kj audio render: storing the clip: {e}"))?;
            Ok::<_, String>((seconds, bytes, hash))
        })
        .await;
        let (seconds, wav_bytes, hash) = match joined {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => return KjResult::Err(e),
            Err(e) => return KjResult::Err(format!("kj audio render: render task panicked: {e}")),
        };

        let message = format!(
            "{hash}\n{seconds:.2} s, {rate} Hz, {wav_bytes} bytes, {} source(s)",
            sources.len()
        );
        KjResult::ok_with_data(
            message,
            serde_json::json!({
                "hash": hash,
                "seconds": seconds,
                "rate": rate,
                "bytes": wav_bytes,
                "sources": sources,
            }),
        )
    }
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
            AudioCommand::Devices { node } => self.audio_devices(node).await,
            AudioCommand::Keep { node, source, generation, seconds } => self.audio_keep(node, source, generation, seconds).await,
            AudioCommand::KeepStatus { id } => self.audio_keep_status(id),
            AudioCommand::KeepRetry { id } => self.audio_keep_retry(id).await,
            AudioCommand::KeepCancel { id } => self.audio_keep_cancel(id).await,
            AudioCommand::Beats { path } => self.audio_beats(path).await,
            AudioCommand::Render { sources, track, phrases, soundfont, rate, tail_ms } => {
                let request = RenderRequest { sources, track, phrases, soundfont, rate, tail_ms };
                self.audio_render(request, Box::new(load_soundfont_synth)).await
            }
        }
    }

    async fn audio_beats(&self, path: String) -> KjResult {
        let audio_path = PathBuf::from(&path);
        let model_dir = beat_this_model_dir();

        // Keep graph loading, decoding, resampling, and inference off the
        // async executor. Started blocking work outlives a canceled waiter.
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

// Verb class: kj/effect.rs
impl Classify for AudioArgs {
    fn effect(&self) -> Effect {
        self.command.effect()
    }
}

impl Classify for AudioCommand {
    fn effect(&self) -> Effect {
        match self {
            AudioCommand::Devices { .. }
            | AudioCommand::KeepStatus { .. }
            | AudioCommand::Beats { .. } => Effect::Read,
            AudioCommand::Keep { .. }
            | AudioCommand::Render { .. }
            | AudioCommand::KeepRetry { .. }
            | AudioCommand::KeepCancel { .. } => Effect::Write,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_help_is_inspectable() {
        for verb in ["devices", "keep", "keep-status", "keep-retry", "keep-cancel", "beats", "render"] {
            let help = AudioArgs::try_parse_from([verb, "--help"]).unwrap_err();
            assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
            println!("{help}");
        }
        assert!(AudioArgs::try_parse_from(["keep", "--node", "audio/test", "--source", "24:0", "--generation", "00000000-0000-0000-0000-000000000001", "--seconds", "0"]).is_err());
        assert!(AudioArgs::try_parse_from(["keep", "--node", "audio/test", "--source", "24:0", "--generation", "00000000-0000-0000-0000-000000000001", "--seconds", "61"]).is_err());
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
            actor_id: caller,
            reviewer_id: None,
            context_id: None,
            session_id: kaijutsu_types::SessionId::new(),
            confirmed: false,
            rc_depth: 0,
            privileged: false,
            cancel: tokio_util::sync::CancellationToken::new(),
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

    // ── render ──────────────────────────────────────────────────────────────

    use std::sync::{Arc, Mutex};

    const BURST_AMPLITUDE: f32 = 0.2;

    /// Variable-length quantity at `bytes[*i..]`, advancing `*i`.
    fn read_vlq(bytes: &[u8], i: &mut usize) -> u32 {
        let mut value = 0u32;
        loop {
            let b = bytes[*i];
            *i += 1;
            value = (value << 7) | (b & 0x7f) as u32;
            if b & 0x80 == 0 {
                return value;
            }
        }
    }

    /// Absolute tick of every note-on in a format-0 SMF without running status.
    fn note_on_ticks(midi: &[u8]) -> Vec<u32> {
        let mut i = 14 + 8; // MThd chunk, MTrk header
        let mut tick = 0u32;
        let mut ons = Vec::new();
        while i < midi.len() {
            tick += read_vlq(midi, &mut i);
            let status = midi[i];
            i += 1;
            match status {
                0xFF => {
                    i += 1;
                    let len = read_vlq(midi, &mut i) as usize;
                    i += len;
                }
                s if s & 0xF0 == 0x90 => {
                    if midi[i + 1] > 0 {
                        ons.push(tick);
                    }
                    i += 2;
                }
                s if matches!(s & 0xF0, 0x80 | 0xA0 | 0xB0 | 0xE0) => i += 2,
                s if matches!(s & 0xF0, 0xC0 | 0xD0) => i += 1,
                other => panic!("unexpected status {other:#x} in test MIDI"),
            }
        }
        ons
    }

    /// Writes a 0.1 s burst of `BURST_AMPLITUDE` at each note-on, assuming
    /// 480 ticks per quarter at 120 BPM, and runs `tail_ms` past the last tick.
    struct FakeSynth {
        calls: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl MidiSynth for FakeSynth {
        fn render(&mut self, midi: &[u8], rate: u32, tail_ms: u32) -> Result<Vec<f32>, String> {
            self.calls.lock().unwrap().push(midi.to_vec());
            let at = |tick: u32| (tick as u64 * rate as u64 / 960) as usize;
            let ons = note_on_ticks(midi);
            let last_tick = {
                // End of the track: the sum of every delta.
                let mut i = 22;
                let mut t = 0;
                while i < midi.len() {
                    t += read_vlq(midi, &mut i);
                    match midi[i] {
                        0xFF => {
                            i += 2;
                            let len = read_vlq(midi, &mut i) as usize;
                            i += len;
                        }
                        s if matches!(s & 0xF0, 0xC0 | 0xD0) => i += 2,
                        _ => i += 3,
                    }
                }
                t
            };
            let len = at(last_tick) + (tail_ms as u64 * rate as u64 / 1000) as usize;
            let mut out = vec![0.0f32; len];
            for tick in ons {
                let start = at(tick);
                for s in out.iter_mut().skip(start).take(rate as usize / 10) {
                    *s = BURST_AMPLITUDE;
                }
            }
            Ok(out)
        }
    }

    fn fake_loader() -> (SynthLoader, Arc<Mutex<Vec<Vec<u8>>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let shared = calls.clone();
        (Box::new(move |_| Ok(Box::new(FakeSynth { calls: shared }))), calls)
    }

    /// An empty file standing in for the SoundFont the fake never reads.
    fn dummy_soundfont(dir: &tempfile::TempDir) -> String {
        let path = dir.path().join("fake.sf2");
        std::fs::write(&path, b"not read").unwrap();
        path.display().to_string()
    }

    fn request(sources: &[&str], soundfont: String) -> RenderRequest {
        RenderRequest {
            sources: sources.iter().map(|s| s.to_string()).collect(),
            track: None,
            phrases: 1,
            soundfont: Some(soundfont),
            rate: 16000,
            tail_ms: 500,
        }
    }

    fn data_of(result: &KjResult) -> &serde_json::Value {
        match result {
            KjResult::Ok { data: Some(data), .. } => data,
            other => panic!("expected a result with data: {other:?}"),
        }
    }

    fn one_note_abc(note: &str) -> String {
        format!("X:1\nT:t\nM:4/4\nL:1/4\nQ:1/4=120\nK:C\n{note}|\n")
    }

    /// A format-0 SMF: one note-on at tick 0 and its note-off at tick 480.
    fn tiny_midi() -> Vec<u8> {
        let track: Vec<u8> = vec![
            0x00, 0x90, 60, 80, // note on
            0x83, 0x60, 0x80, 60, 0, // 480 ticks later, note off
            0x00, 0xFF, 0x2F, 0x00, // end of track
        ];
        let mut smf = b"MThd".to_vec();
        smf.extend_from_slice(&6u32.to_be_bytes());
        smf.extend_from_slice(&[0, 0, 0, 1, 0x01, 0xE0]);
        smf.extend_from_slice(b"MTrk");
        smf.extend_from_slice(&(track.len() as u32).to_be_bytes());
        smf.extend_from_slice(&track);
        smf
    }

    /// (rate, channels, bits, samples) from a canonical 44-byte-header WAV.
    fn decode_wav(wav: &[u8]) -> (u32, u16, u16, Vec<i16>) {
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        let channels = u16::from_le_bytes([wav[22], wav[23]]);
        let rate = u32::from_le_bytes(wav[24..28].try_into().unwrap());
        let bits = u16::from_le_bytes([wav[34], wav[35]]);
        let data_len = u32::from_le_bytes(wav[40..44].try_into().unwrap()) as usize;
        assert_eq!(wav.len(), 44 + data_len, "data chunk size matches the file");
        let samples = wav[44..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        (rate, channels, bits, samples)
    }

    #[tokio::test]
    async fn render_mixes_positional_abc_sources_from_time_zero_into_a_wav_clip() {
        let d = crate::kj::test_helpers::test_dispatcher().await;
        let cas = d.kernel().cas();
        let a = cas.store(one_note_abc("C").as_bytes(), ABC_MIME).unwrap().to_string();
        let b = cas.store(one_note_abc("E").as_bytes(), ABC_MIME).unwrap().to_string();
        let dir = tempfile::tempdir().unwrap();
        let (loader, calls) = fake_loader();

        let result = d.audio_render(request(&[&a, &b], dummy_soundfont(&dir)), loader).await;

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(calls.lock().unwrap().len(), 2, "one synthesis per source");
        let data = data_of(&result).clone();
        let hash = data["hash"].as_str().unwrap().to_string();
        assert!(result.message().starts_with(&hash), "text leads with the hash: {}", result.message());
        assert_eq!(data["rate"], 16000);
        assert_eq!(data["sources"], serde_json::json!([a, b]));
        // 0.5 s note + 0.5 s tail.
        assert!((data["seconds"].as_f64().unwrap() - 1.0).abs() < 0.01, "{data}");

        let stored: kaijutsu_cas::ContentHash = hash.parse().unwrap();
        let reference = cas.inspect(&stored).unwrap().expect("clip is in the CAS");
        assert_eq!(reference.mime_type, "audio/wav");
        let wav = cas.retrieve(&stored).unwrap().unwrap();
        assert_eq!(data["bytes"].as_u64().unwrap() as usize, wav.len());
        let (rate, channels, bits, samples) = decode_wav(&wav);
        assert_eq!((rate, channels, bits), (16000, 1, 16));
        let expected = (2.0 * BURST_AMPLITUDE * i16::MAX as f32).round() as i16;
        assert_eq!(samples[0], expected, "both sources sound at sample 0");
        assert_eq!(samples[3000], 0, "the bursts end after 0.1 s");
    }

    #[tokio::test]
    async fn render_passes_midi_through_and_refuses_other_types_and_missing_hashes() {
        let d = crate::kj::test_helpers::test_dispatcher().await;
        let cas = d.kernel().cas();
        let midi = tiny_midi();
        let midi_hash = cas.store(&midi, MIDI_MIME).unwrap().to_string();
        let text_hash = cas.store(b"just words", "text/plain").unwrap().to_string();
        let gone = cas.store(b"soon gone", ABC_MIME).unwrap();
        cas.remove(&gone).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let font = dummy_soundfont(&dir);

        let (loader, calls) = fake_loader();
        let result = d.audio_render(request(&[&midi_hash], font.clone()), loader).await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(*calls.lock().unwrap(), vec![midi], "MIDI reaches the synthesizer unchanged");

        let (loader, _) = fake_loader();
        let refused = d.audio_render(request(&[&text_hash], font.clone()), loader).await;
        assert!(!refused.is_ok());
        assert!(refused.message().contains(&text_hash), "{}", refused.message());
        assert!(refused.message().contains("text/plain"), "{}", refused.message());

        let (loader, _) = fake_loader();
        let missing = d.audio_render(request(&[&gone.to_string()], font), loader).await;
        assert!(!missing.is_ok());
        assert!(missing.message().contains(&gone.to_string()), "{}", missing.message());
        assert!(missing.message().contains("not found"), "{}", missing.message());
    }

    fn abc_block(seq: u64, tick: i64, abc: &str) -> BlockSnapshot {
        use kaijutsu_types::{BlockId, BlockKind, BlockSnapshotBuilder, ContextId, PrincipalId, Role, TrackId};
        BlockSnapshotBuilder::new(
            BlockId::new(ContextId::new(), PrincipalId::new(), seq),
            BlockKind::Text,
        )
        .role(Role::Model)
        .content(abc)
        .content_type(ContentType::Abc)
        .tick(kaijutsu_types::Tick::new(tick))
        .track(TrackId::new("bass").unwrap())
        .build()
    }

    #[test]
    fn window_cells_keep_only_the_span_before_the_playhead() {
        let blocks = vec![
            abc_block(1, 0, "a"),
            abc_block(2, 16, "b"),
            abc_block(3, 32, "c"),
            abc_block(4, 48, "d"),
            abc_block(5, 64, "at the playhead"),
        ];
        let cells: Vec<(i64, &str)> = window_cells(&blocks, 64, 32).iter().map(|c| (c.tick, c.abc)).collect();
        assert_eq!(cells, vec![(32, "c"), (48, "d")], "the window is [playhead - span, playhead)");
        assert!(window_cells(&blocks, 64, 0).is_empty());
    }

    #[test]
    fn cell_offsets_follow_the_beat_period() {
        let beat = Duration::from_millis(500);
        assert_eq!(cell_offset_samples(32, 32, beat, 16000), 0, "the window start is sample 0");
        // 16 beats of 0.5 s at 16 kHz.
        assert_eq!(cell_offset_samples(48, 32, beat, 16000), 128_000);
        assert_eq!(cell_offset_samples(48, 32, Duration::from_millis(250), 16000), 64_000);
    }

    /// The MIDI artifact a cell was played from: an asset block under it.
    fn midi_asset(seq: u64, cell: &BlockSnapshot, hash: &str) -> BlockSnapshot {
        use kaijutsu_types::{BlockId, BlockKind, BlockSnapshotBuilder, PrincipalId, Role};
        BlockSnapshotBuilder::new(BlockId::new(cell.id.context_id, PrincipalId::new(), seq), BlockKind::Text)
            .role(Role::Asset)
            .parent_id(cell.id.clone())
            .content(hash)
            .build()
    }

    #[test]
    fn a_cell_renders_from_its_midi_artifact_and_an_empty_cell_without_one_is_left_out() {
        let abc = one_note_abc("C");
        // A fallback cell repeats a phrase: it holds no notation, only the MIDI it played.
        let repeat = abc_block(1, 32, "");
        let written = abc_block(3, 48, &abc);
        let nothing = abc_block(5, 40, "");
        let blocks = vec![
            repeat.clone(), midi_asset(2, &repeat, "aaaa"),
            written.clone(), midi_asset(4, &written, "bbbb"),
            nothing,
        ];
        let beat = Duration::from_millis(500);
        let load = |hash: &str| -> Result<Option<Vec<u8>>, String> {
            Ok(match hash { "aaaa" => Some(vec![1]), "bbbb" => Some(vec![2]), _ => None })
        };
        let (parts, labels) = track_parts("bass", &blocks, 64, 16, beat, 2, 16000, &load).unwrap();
        assert_eq!(parts.iter().map(|p| (p.offset_samples, p.midi.clone())).collect::<Vec<_>>(), vec![(0, vec![1]), (128_000, vec![2])]);
        assert_eq!(labels, vec!["bass@32".to_string(), "bass@48".to_string()]);

        // An artifact the CAS does not hold as MIDI: the notation is rendered.
        let (parts, _) = track_parts("bass", &blocks, 64, 16, beat, 1, 16000, &|_| Ok(None)).unwrap();
        assert_eq!(parts.len(), 1, "the written cell's own notation");
        assert_ne!(parts[0].midi, vec![2]);

        // Only empty cells with no artifact: nothing was committed.
        let silent = vec![abc_block(1, 32, ""), abc_block(2, 48, "  ")];
        let err = track_parts("bass", &silent, 64, 16, beat, 2, 16000, &|_| Ok(None)).unwrap_err();
        assert_eq!(err, "kj audio render: track 'bass' committed nothing in the last 2 phrase(s)");
    }

    #[test]
    fn a_clip_starts_at_its_first_cell_and_a_lost_artifact_is_refused() {
        let abc = one_note_abc("C");
        let beat = Duration::from_millis(500);
        // The playhead is mid-phrase: the window is [40, 72) and the cells start at 48 and 64.
        let blocks = vec![abc_block(1, 48, &abc), abc_block(2, 64, &abc)];
        let (parts, _) = track_parts("bass", &blocks, 72, 32, beat, 1, 16000, &|_| Ok(None)).unwrap();
        assert_eq!(parts.iter().map(|p| p.offset_samples).collect::<Vec<_>>(), vec![0, 128_000], "no silence before the first cell");

        // A repeat cell whose MIDI the CAS no longer holds played something we cannot render.
        let repeat = abc_block(1, 48, "");
        let blocks = vec![repeat.clone(), midi_asset(2, &repeat, "gone")];
        let err = track_parts("bass", &blocks, 72, 32, beat, 1, 16000, &|_| Ok(None)).unwrap_err();
        assert!(err.contains("bass@48") && err.contains("gone") && !err.contains("committed nothing"), "{err}");
    }

    #[test]
    fn track_parts_places_cells_and_refuses_an_empty_window() {
        let abc = one_note_abc("C");
        let blocks = vec![abc_block(1, 0, &abc), abc_block(2, 32, &abc), abc_block(3, 48, &abc)];
        let beat = Duration::from_millis(500);

        let (parts, labels) = track_parts("bass", &blocks, 64, 16, beat, 2, 16000, &|_| Ok(None)).unwrap();
        assert_eq!(parts.iter().map(|p| p.offset_samples).collect::<Vec<_>>(), vec![0, 128_000]);
        assert_eq!(labels, vec!["bass@32".to_string(), "bass@48".to_string()]);

        let err = track_parts("bass", &blocks, 64, 16, beat, 1, 16000, &|_| Ok(None)).map(|(p, _)| p.len()).unwrap();
        assert_eq!(err, 1, "one phrase holds only the cell at tick 48");

        let empty = track_parts("bass", &blocks, 200, 16, beat, 3, 16000, &|_| Ok(None)).unwrap_err();
        assert_eq!(empty, "kj audio render: track 'bass' committed nothing in the last 3 phrase(s)");
    }

    #[test]
    fn protect_clipping_scales_a_hot_mix_to_minus_three_dbfs_and_leaves_a_quiet_one() {
        let mut hot = vec![0.5, -2.0, 1.0];
        protect_clipping(&mut hot);
        let peak = hot.iter().fold(0.0f32, |p, s| p.max(s.abs()));
        assert!((peak - 10f32.powf(-3.0 / 20.0)).abs() < 1e-4, "peak {peak}");
        assert!((hot[0] / hot[1] + 0.25).abs() < 1e-6, "the mix keeps its shape");

        let mut quiet = vec![0.1, -0.2, 0.0];
        protect_clipping(&mut quiet);
        assert_eq!(quiet, vec![0.1, -0.2, 0.0]);
        let mut silence = vec![0.0; 4];
        protect_clipping(&mut silence);
        assert_eq!(silence, vec![0.0; 4], "silence is never amplified");
    }

    #[tokio::test]
    async fn render_refuses_a_missing_soundfont_a_bad_rate_and_an_over_long_clip() {
        let d = crate::kj::test_helpers::test_dispatcher().await;
        let hash = d.kernel().cas().store(one_note_abc("C").as_bytes(), ABC_MIME).unwrap().to_string();
        let dir = tempfile::tempdir().unwrap();

        let (loader, _) = fake_loader();
        let missing = dir.path().join("absent.sf2").display().to_string();
        let result = d.audio_render(request(&[&hash], missing.clone()), loader).await;
        assert!(!result.is_ok());
        assert!(result.message().contains(&missing), "names the path: {}", result.message());
        assert!(result.message().contains("--soundfont"), "{}", result.message());

        for rate in [7999, 48001] {
            let (loader, _) = fake_loader();
            let mut req = request(&[&hash], dummy_soundfont(&dir));
            req.rate = rate;
            let result = d.audio_render(req, loader).await;
            assert!(!result.is_ok(), "rate {rate}");
            assert!(result.message().contains("8000..=48000"), "{}", result.message());
        }
        for rate in [8000, 48000] {
            let (loader, _) = fake_loader();
            let mut req = request(&[&hash], dummy_soundfont(&dir));
            req.rate = rate;
            assert!(d.audio_render(req, loader).await.is_ok(), "rate {rate}");
        }

        let (loader, _) = fake_loader();
        let mut req = request(&[&hash], dummy_soundfont(&dir));
        req.tail_ms = 121_000;
        let result = d.audio_render(req, loader).await;
        assert!(!result.is_ok());
        assert!(result.message().contains("120 s cap"), "{}", result.message());
    }

    #[test]
    fn render_mix_refuses_a_part_that_starts_past_the_cap() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut synth = FakeSynth { calls: calls.clone() };
        let part = RenderPart { offset_samples: 120 * 8000, midi: tiny_midi() };
        let err = render_mix(&mut synth, &[part], 8000, 0).unwrap_err();
        assert!(err.contains("120 s cap"), "{err}");
        assert!(calls.lock().unwrap().is_empty(), "nothing is synthesized for a part past the cap");
    }

    /// Answer snapshot requests with `snapshots`, as the beat scheduler would.
    fn serve_snapshots(
        mut ingress: tokio::sync::mpsc::UnboundedReceiver<crate::hyoushigi::BeatRequest>,
        snapshots: Vec<crate::hyoushigi::TrackSnapshot>,
    ) {
        tokio::spawn(async move {
            while let Some(req) = ingress.recv().await {
                if let crate::hyoushigi::BeatRequest::Snapshot { reply } = req {
                    let _ = reply.send(snapshots.clone());
                }
            }
        });
    }

    #[tokio::test]
    async fn render_track_reads_the_live_score_and_refuses_unknown_and_silent_tracks() {
        use crate::hyoushigi::TrackSnapshot;
        use kaijutsu_types::{ContextId, PrincipalId, TrackId};
        let d = crate::kj::test_helpers::test_dispatcher().await;
        let score = ContextId::new();
        d.block_store().create_document(score, crate::DocumentKind::Conversation, None).unwrap();
        let abc = one_note_abc("C");
        let writer = PrincipalId::new();
        let mut after = None;
        for (seq, tick) in [(1u64, 0i64), (2, 16), (3, 32)] {
            let mut block = abc_block(seq, tick, &abc);
            block.id = kaijutsu_types::BlockId::new(score, writer, seq);
            after = Some(d.block_store().insert_from_snapshot(score, block, after.as_ref()).unwrap());
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let hush_score = ContextId::new();
        d.block_store().create_document(hush_score, crate::DocumentKind::Conversation, None).unwrap();
        let snapshot = |id: &str, score_context| TrackSnapshot {
            id: TrackId::new(id).unwrap(),
            score_context,
            playing: true,
            playhead: 48,
            period: Duration::from_millis(500),
            beats_per_phrase: 16,
            beat_count: 48,
            last_epoch_ns: 0,
            clock_kind: "system".to_string(),
            attached: vec![],
        };
        serve_snapshots(rx, vec![snapshot("bass", score), snapshot("hush", hush_score)]);
        let dir = tempfile::tempdir().unwrap();
        let track_request = |track: &str, phrases: u64| RenderRequest {
            sources: vec![],
            track: Some(track.to_string()),
            phrases,
            soundfont: Some(dummy_soundfont(&dir)),
            rate: 16000,
            tail_ms: 500,
        };

        let (loader, calls) = fake_loader();
        let result = d.audio_render(track_request("bass", 2), loader).await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(data_of(&result)["sources"], serde_json::json!(["bass@16", "bass@32"]));
        assert_eq!(calls.lock().unwrap().len(), 2);
        // Second cell starts 16 beats of 0.5 s after the first; each runs 1 s.
        assert!((data_of(&result)["seconds"].as_f64().unwrap() - 9.0).abs() < 0.01, "{:?}", data_of(&result));

        let (loader, _) = fake_loader();
        let unknown = d.audio_render(track_request("nope", 1), loader).await;
        assert!(!unknown.is_ok());
        assert!(unknown.message().contains("unknown track 'nope'"), "{}", unknown.message());

        let (loader, calls) = fake_loader();
        let silent = d.audio_render(track_request("hush", 1), loader).await;
        assert!(!silent.is_ok());
        assert_eq!(
            silent.message(),
            "kj audio render: track 'hush' committed nothing in the last 1 phrase(s)"
        );
        assert!(calls.lock().unwrap().is_empty());

        d.kernel_db()
            .lock()
            .upsert_track(&crate::kernel_db::PersistedTrack {
                track_id: "dormant-keys".into(),
                period_ms: 500,
                beats_per_phrase: 16,
                playhead_tick: Some(32),
                playing: false,
                score_context_id: Some(score),
                clock_kind: "system".into(),
            })
            .unwrap();
        let (loader, calls) = fake_loader();
        let dormant = d.audio_render(track_request("dormant-keys", 1), loader).await;
        assert!(!dormant.is_ok());
        assert!(dormant.message().contains("track 'dormant-keys' is dormant"), "{}", dormant.message());
        assert!(!dormant.message().contains("unknown track"), "{}", dormant.message());
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn track_parts_bounds_phrases_against_render_max_seconds() {
        let abc = one_note_abc("C");
        let bad_abc = "X:1\nnot valid abc!!";
        // Put unparseable ABC far back in the score to confirm bounding happens
        // before scanning/parsing earlier cells.
        let blocks = vec![abc_block(1, 0, bad_abc), abc_block(2, 1000, &abc)];
        let beat = Duration::from_millis(500);
        // 16 beats * 0.5 s = 8 s/phrase; 15 phrases = 120 s (fits), 16 phrases = 128 s (over).
        let over = track_parts("bass", &blocks, 1016, 16, beat, 16, 16000, &|_| Ok(None)).unwrap_err();
        assert!(over.contains("120 s cap") && over.contains("at most 15 phrase(s)"), "{over}");

        let huge = track_parts("bass", &blocks, 1016, 16, beat, u64::MAX, 16000, &|_| Ok(None)).unwrap_err();
        assert!(huge.contains("too large"), "{huge}");

        let ok = track_parts("bass", &blocks, 1016, 16, beat, 15, 16000, &|_| Ok(None)).unwrap();
        assert_eq!(ok.0.len(), 1);
    }

    /// Needs a real SoundFont: set KAIJUTSU_TEST_SOUNDFONT to an .sf2 path.
    #[test]
    fn real_synth_renders_a_one_bar_tune() {
        let Ok(path) = std::env::var("KAIJUTSU_TEST_SOUNDFONT") else {
            eprintln!("skipped: set KAIJUTSU_TEST_SOUNDFONT to an .sf2 file to run the real-synth render");
            return;
        };
        let midis = abc_to_midis("X:1\nT:bar\nM:4/4\nL:1/4\nQ:1/4=120\nK:C\nCDEF|\n", "bar").unwrap();
        let mut synth = load_soundfont_synth(Path::new(&path)).expect("load SoundFont");
        let parts: Vec<RenderPart> =
            midis.into_iter().map(|midi| RenderPart { offset_samples: 0, midi }).collect();
        let mut mix = render_mix(synth.as_mut(), &parts, 16000, 500).expect("render");
        protect_clipping(&mut mix);
        let seconds = mix.len() as f64 / 16000.0;
        // One bar of four beats at 120 BPM is 2 s, plus the 0.5 s tail.
        assert!((seconds - 2.5).abs() < 0.2, "rendered {seconds} s");
        let peak = mix.iter().fold(0.0f32, |p, s| p.max(s.abs()));
        assert!(peak > 0.01, "the render is not silent (peak {peak})");
        let wav = encode_wav(&mix, 16000);
        let (rate, channels, bits, samples) = decode_wav(&wav);
        assert_eq!((rate, channels, bits), (16000, 1, 16));
        assert_eq!(samples.len(), mix.len());
    }
}
