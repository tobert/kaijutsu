//! `kj play <path>` — resolve an audio sample and push a render cue over the
//! FlowBus (docs/pcm.md "The wire"); `kj play --track <t>` — commit a clip
//! record onto a track's score instead (docs/pcm.md R2, "the producer verb").
//!
//! Two modes, one verb, switched on `--track`:
//!
//! - **Bare `kj play <path|--cas>`** (unchanged): the kernel never touches
//!   audio hardware — it reads the file, sniffs its format from the extension
//!   to derive the wire MIME, wraps the bytes in a play-now [`RenderCue`]
//!   (inline, `lead == 0`), and publishes `BlockFlow::RenderCue` — the same
//!   FlowBus every `BlockEvents` subscription bridge already drains, so every
//!   attached client's render sink receives it with no new transport.
//! - **`kj play <path|--cas> --track <t>`**: instead of playing now, the same
//!   media (cas-put from `<path>`, or an existing `--cas` hash) is wrapped in
//!   a Shape A [`Clip`](kaijutsu_audio::Clip) record and committed onto
//!   track `t`'s timeline via [`crate::hyoushigi::schedule_clip_cell`] — the
//!   producer side of a placed sample. `--at <tick>` places it at an
//!   absolute tick (default: ASAP, `playhead + 1`); `--label` names it in the
//!   score (defaults to the file stem for `<path>`, REQUIRED for `--cas`,
//!   which has no derivable name). This mode does **not** publish a play-now
//!   `RenderCue` — the *fire* cue still rides the crossing at the write
//!   barrier (docs/pcm.md R3) — but `schedule_clip_cell` itself now publishes
//!   the R4 *prepare* directive (`PREPARE_MIME`) at commit, so every sink
//!   starts warming its cache immediately, long before the crossing.

use clap::Parser;
use kaijutsu_audio::{AudioFormatHint, Clip, CuePayload, RenderCue, CLIP_VERSION};
use kaijutsu_cas::{ContentHash, ContentStore};
use kaijutsu_types::{ContentType, TrackId};

use super::refs;
use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};
use crate::flows::BlockFlow;
use crate::hyoushigi::schedule_clip_cell;

#[derive(Parser, Debug)]
#[command(
    name = "play",
    about = "Play an audio sample now, or commit it as a clip cell onto a track with --track (docs/pcm.md)",
    long_about = "Bare `kj play <path|--cas>` plays now over every attached client's render \
                  target (unchanged). With `--track <t>`, the same media is committed as a \
                  Shape A clip record onto track <t>'s score instead of playing immediately — \
                  the producer side of a placed sample (docs/pcm.md R2). `--at <tick>` places \
                  it at an absolute tick (default: ASAP, playhead + 1); `--label` names the clip \
                  in the score (defaults to the file stem for a <path>; REQUIRED for --cas, which \
                  has no derivable name).",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct PlayArgs {
    /// Path to the audio file to play (OS path, not a VFS path — mirrors `kj cas
    /// put`). Mutually exclusive with `--cas`; exactly one is required.
    path: Option<String>,
    /// Play an object already in the CAS by its content hash. The MIME is resolved
    /// from the object's CAS metadata, and the cue carries a `Cas` payload — so the
    /// sink resolves the bytes from its XDG cache / SFTP `/v/cas` (the
    /// clip-prefetch path, docs/pcm.md 5c / docs/slash-v.md track B).
    #[arg(long, conflicts_with = "path")]
    cas: Option<String>,
    /// Target context: . (default) | <label> | <hex prefix>. Reserved for
    /// future per-listener routing; the standalone slice forwards to every
    /// attached client regardless of which context is named.
    #[arg(long, short = 'c')]
    context: Option<String>,
    /// Commit a clip cell onto this track's score instead of playing now
    /// (docs/pcm.md R2). The track must already be armed (`kj transport
    /// attach --track <t>` first) — an un-armed track is a loud error naming
    /// how to arm it.
    #[arg(long)]
    track: Option<String>,
    /// With `--track`: the absolute tick to place the clip at (must be ≥ 0).
    /// Omitted = ASAP (`playhead + 1` at schedule time). Meaningless without
    /// `--track`, so clap rejects that combination outright (kaibo review
    /// 2026-07-16: silent-ignore was a UX footgun).
    #[arg(long, requires = "track")]
    at: Option<i64>,
    /// With `--track`: the clip's human label — hashes are opaque, the label
    /// is how the score reads. Defaults to the file stem for the `<path>`
    /// form (`kick-808.wav` → `kick-808`); REQUIRED for the `--cas` form
    /// (a hash has no derivable name). Meaningless without `--track`, so
    /// clap rejects that combination outright.
    #[arg(long, requires = "track")]
    label: Option<String>,
}

impl KjDispatcher {
    pub(crate) fn dispatch_play(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<PlayArgs>();
        }
        let parsed = match PlayArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj play: {e}"));
            }
        };

        // `--track` switches modes entirely: commit a clip cell instead of
        // playing now. Bare `kj play` (no `--track`) falls through unchanged.
        if let Some(track_name) = parsed.track.clone() {
            return self.commit_clip_cell(parsed, track_name, caller);
        }

        // Build the cue payload from exactly one source: a CAS hash (bytes stay
        // out-of-band, the sink resolves them from /v/cas) or a local file
        // (bytes inline). `mime` + `desc` + `payload` fall out of whichever.
        let (mime, payload, desc): (String, CuePayload, String) =
            match (parsed.cas.as_deref(), parsed.path.as_deref()) {
                (Some(hash_str), _) => {
                    let hash = match hash_str.parse::<ContentHash>() {
                        Ok(h) => h,
                        Err(e) => return KjResult::Err(format!("kj play --cas: invalid hash: {e}")),
                    };
                    // The MIME comes from the object's own CAS metadata — no
                    // extension to sniff. An object not in the pool is a loud error.
                    let mime = match self.kernel().cas().inspect(&hash) {
                        Ok(Some(r)) => r.mime_type,
                        Ok(None) => {
                            return KjResult::Err(format!("kj play --cas: not found: {hash}"));
                        }
                        Err(e) => return KjResult::Err(format!("kj play --cas: {e}")),
                    };
                    let desc = format!("cas:{hash}");
                    (mime, CuePayload::Cas(hash), desc)
                }
                (None, Some(path)) => {
                    // Derive the wire MIME from the extension BEFORE reading the
                    // file — an unrecognized/missing extension is a loud error,
                    // never a silently guessed default. An `.abc` score renders
                    // to MIDI at the sink; audio extensions play as a sample.
                    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
                    let mime: String = match ext.as_deref() {
                        Some("abc") => "text/vnd.abc".to_string(),
                        _ => match AudioFormatHint::from_path_extension(path) {
                            Some(f) => f.mime().to_string(),
                            None => {
                                return KjResult::Err(format!(
                                    "kj play: {path}: unrecognized or missing extension \
                                     (expected .abc or one of .wav/.flac/.mp3/.ogg/.aac/.m4a)"
                                ));
                            }
                        },
                    };
                    let bytes = match std::fs::read(path) {
                        Ok(b) => b,
                        Err(e) => return KjResult::Err(format!("kj play: {path}: {e}")),
                    };
                    let desc = format!("{path} ({} bytes)", bytes.len());
                    (mime, CuePayload::Inline(bytes), desc)
                }
                // Neither given — show help (clap already rejects both via
                // conflicts_with).
                (None, None) => return clap_help_for::<PlayArgs>(),
            };

        let context_id = {
            let db = self.kernel_db().lock();
            match refs::resolve_context_arg(parsed.context.as_deref(), caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj play: {e}")),
            }
        };

        let cue = RenderCue {
            mime: mime.clone(),
            payload,
            lead: std::time::Duration::ZERO,
            // Unstamped: `kj play` is a direct play-now directive (lead ZERO,
            // fired at receipt) — there's no phrase-boundary transfer latency
            // to back-date against, unlike the track render seam's cues.
            epoch_ns: 0,
        };
        let receivers = self.kernel().block_flows().publish(BlockFlow::RenderCue {
            context_id,
            cue,
        });

        KjResult::ok_ephemeral(
            format!("playing {desc} ({mime}) — {receivers} listener(s)"),
            ContentType::Plain,
        )
    }

    /// `kj play --track <t> <path|--cas>` — the commit-a-clip-cell mode
    /// (docs/pcm.md R2). Resolves the media (cas-put a file, or an existing
    /// `--cas` hash), builds a Shape A [`Clip`] record, and hands it to
    /// [`schedule_clip_cell`] to land on track `t`'s timeline.
    ///
    /// Track resolution mirrors `kj transport`'s `--track <name>` path
    /// (`TrackId::new`, loud on an invalid name) with one addition: unlike
    /// `kj transport`, this verb never falls back to the caller's context
    /// attachment when `--track` is omitted — omitting `--track` entirely
    /// means "stay in play-now mode" (dispatched before this method is ever
    /// called). Here `--track` is always `Some`, so only the name needs
    /// parsing. The track must already be **armed** (a live
    /// [`crate::hyoushigi::SharedTimeline`], independent of whether its clock
    /// is currently playing — a clip may be placed ahead of `kj transport
    /// play`), or this is a loud error naming how to arm it.
    fn commit_clip_cell(&self, parsed: PlayArgs, track_name: String, caller: &KjCaller) -> KjResult {
        let track = match TrackId::new(track_name.as_str()) {
            Ok(t) => t,
            Err(e) => {
                return KjResult::Err(format!(
                    "kj play --track: invalid track name {track_name:?}: {e}"
                ));
            }
        };
        if self.kernel().track_timeline(&track).is_none() {
            return KjResult::Err(format!(
                "kj play --track: track '{}' is not armed — attach it first \
                 (`kj transport attach --track {}`)",
                track.as_str(),
                track.as_str()
            ));
        }

        // Resolve media + mime + label from exactly one source, mirroring the
        // play-now branch's (cas, path) split.
        let (media, mime, label): (ContentHash, String, String) =
            match (parsed.cas.as_deref(), parsed.path.as_deref()) {
                (Some(hash_str), _) => {
                    let hash = match hash_str.parse::<ContentHash>() {
                        Ok(h) => h,
                        Err(e) => return KjResult::Err(format!("kj play --cas: invalid hash: {e}")),
                    };
                    let mime = match self.kernel().cas().inspect(&hash) {
                        Ok(Some(r)) => r.mime_type,
                        Ok(None) => {
                            return KjResult::Err(format!("kj play --cas: not found: {hash}"));
                        }
                        Err(e) => return KjResult::Err(format!("kj play --cas: {e}")),
                    };
                    // A hash has no derivable name — --label is REQUIRED here.
                    let label = match parsed.label.as_deref().map(str::trim) {
                        Some(l) if !l.is_empty() => l.to_string(),
                        _ => {
                            return KjResult::Err(
                                "kj play --track --cas: --label is required (a hash has no \
                                 derivable name — labels are how the score reads)"
                                    .to_string(),
                            );
                        }
                    };
                    (hash, mime, label)
                }
                (None, Some(path)) => {
                    // The clip's mime is what the SINK decodes — only real audio
                    // formats make sense here (unlike bare `kj play`, `.abc` isn't
                    // accepted: an ABC phrase is a musician's committed notation,
                    // scheduled through `schedule_abc_cell`, never a placed clip).
                    let mime = match AudioFormatHint::from_path_extension(path) {
                        Some(f) => f.mime().to_string(),
                        None => {
                            return KjResult::Err(format!(
                                "kj play --track: {path}: unrecognized or missing extension \
                                 (expected one of .wav/.flac/.mp3/.ogg/.aac/.m4a)"
                            ));
                        }
                    };
                    let bytes = match std::fs::read(path) {
                        Ok(b) => b,
                        Err(e) => return KjResult::Err(format!("kj play: {path}: {e}")),
                    };
                    let hash = match self.kernel().cas().store(&bytes, &mime) {
                        Ok(h) => h,
                        Err(e) => {
                            return KjResult::Err(format!("kj play --track: storing {path}: {e}"));
                        }
                    };
                    let label = match parsed.label.as_deref().map(str::trim) {
                        Some(l) if !l.is_empty() => l.to_string(),
                        _ => {
                            let stem = std::path::Path::new(path)
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .unwrap_or("");
                            if stem.trim().is_empty() {
                                // A whitespace-only stem (`"   .wav"`) or an
                                // underivable one would flow into Clip validation
                                // as an opaque "label must be non-empty" — say
                                // what actually went wrong instead (kaibo review;
                                // NB a plain dotfile `.wav` is FINE: file_stem()
                                // returns the whole name ".wav", a usable label).
                                return KjResult::Err(format!(
                                    "kj play --track: cannot derive a label from {path:?} \
                                     (empty file stem) — pass --label"
                                ));
                            }
                            stem.to_string()
                        }
                    };
                    (hash, mime, label)
                }
                (None, None) => return clap_help_for::<PlayArgs>(),
            };

        let clip = Clip {
            v: CLIP_VERSION,
            media: media.clone(),
            mime: mime.clone(),
            label: label.clone(),
            src_offset_ms: 0,
            src_len_ms: None,
            gain_db: 0.0,
            ext: serde_json::Map::new(),
        };
        let clip_json = match clip.to_json() {
            Ok(j) => j,
            Err(e) => return KjResult::Err(format!("kj play --track: building clip record: {e}")),
        };
        // The record's own CAS hash — distinct from `media` (the two-level
        // reference, docs/pcm.md). Pure/deterministic (content-addressed by
        // bytes alone), so computing it here for the report doesn't duplicate
        // the store `schedule_clip_cell` performs internally under the same hash.
        let record = ContentHash::from_data(clip_json.as_bytes());

        // A negative tick would fall through to Timeline::schedule's in-the-past
        // gate with a technically-true-but-confusing "InThePast { start: Tick(-1) }"
        // error; the user didn't pick a past tick, they picked a nonsense one —
        // reject it here with the real reason (kaibo review 2026-07-16).
        if let Some(at) = parsed.at {
            if at < 0 {
                return KjResult::Err(format!("kj play --track: --at must be ≥ 0 (got {at})"));
            }
        }
        let at = parsed.at.map(kaijutsu_types::Tick::new);
        let tick = match schedule_clip_cell(
            self.kernel(),
            &clip_json,
            at,
            track.clone(),
            caller.principal_id,
        ) {
            Ok(t) => t,
            Err(e) => return KjResult::Err(format!("kj play --track: {e}")),
        };

        KjResult::Ok {
            message: format!(
                "clip '{label}' ({mime}) scheduled onto track '{}' at tick {}",
                track.as_str(),
                tick.get()
            ),
            content_type: ContentType::Plain,
            ephemeral: false,
            data: Some(serde_json::json!({
                "track": track.as_str(),
                "tick": tick.get(),
                "media": media.to_string(),
                "record": record.to_string(),
                "label": label,
                "mime": mime,
            })),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::flows::BlockFlow;
    use crate::kj::test_helpers::{register_context, test_dispatcher};
    use kaijutsu_audio::CuePayload;
    use kaijutsu_types::PrincipalId;
    use std::sync::Arc;
    use std::time::Duration;

    /// TDD anchor: `kj play <tempwav>` must publish a `BlockFlow::RenderCue`
    /// on the FlowBus, carrying the file's bytes verbatim (inline), the MIME
    /// sniffed from the extension, and a zero lead (play now). Subscribe
    /// BEFORE dispatch — the FlowBus is a live broadcast, not a queue a late
    /// subscriber can catch up on.
    #[tokio::test]
    async fn play_publishes_block_flow_render_cue() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let principal = PrincipalId::new();
        let ctx = register_context(&dispatcher, Some("c"), None, principal);
        let caller = crate::kj::test_helpers::caller_with_context(ctx);

        let mut sub = dispatcher.kernel().block_flows().subscribe("block.render_cue");

        let dir = tempfile::tempdir().expect("tmpdir");
        let wav_path = dir.path().join("kick.wav");
        let sample_bytes = b"RIFF....WAVEfmt not-a-real-wav-but-bytes-are-bytes".to_vec();
        std::fs::write(&wav_path, &sample_bytes).expect("write sample wav");

        let result = dispatcher.dispatch_play(
            &[wav_path.to_string_lossy().into_owned()],
            &caller,
        );
        assert!(result.is_ok(), "kj play failed: {result:?}");

        let msg = sub
            .try_recv()
            .expect("BlockFlow::RenderCue should have been published");
        match msg.payload {
            BlockFlow::RenderCue { context_id, cue } => {
                assert_eq!(context_id, ctx, "directive names the resolved context");
                assert_eq!(cue.mime, "audio/wav", "extension sniffed to the wav MIME");
                assert_eq!(cue.lead, Duration::ZERO, "kj play is play-now (zero lead)");
                match cue.payload {
                    CuePayload::Inline(bytes) => {
                        assert_eq!(bytes, sample_bytes, "bytes ride the cue verbatim inline");
                    }
                    other => panic!("expected Inline, got {other:?}"),
                }
            }
            other => panic!("expected RenderCue, got {other:?}"),
        }
    }

    /// `kj play <tune.abc>` emits a `text/vnd.abc` cue carrying the ABC text
    /// inline (rendered to MIDI at the sink, docs/midi.md), not an audio cue.
    #[tokio::test]
    async fn play_abc_emits_a_text_vnd_abc_cue() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let principal = PrincipalId::new();
        let ctx = register_context(&dispatcher, Some("c"), None, principal);
        let caller = crate::kj::test_helpers::caller_with_context(ctx);

        let mut sub = dispatcher.kernel().block_flows().subscribe("block.render_cue");

        let dir = tempfile::tempdir().expect("tmpdir");
        let abc_path = dir.path().join("bass.abc");
        let abc = b"X:1\nT:t\nM:4/4\nL:1/4\nQ:1/4=120\nK:C\nCDEF|\n".to_vec();
        std::fs::write(&abc_path, &abc).expect("write abc");

        let result = dispatcher.dispatch_play(&[abc_path.to_string_lossy().into_owned()], &caller);
        assert!(result.is_ok(), "kj play .abc failed: {result:?}");

        let msg = sub.try_recv().expect("RenderCue should have been published");
        match msg.payload {
            BlockFlow::RenderCue { cue, .. } => {
                assert_eq!(cue.mime, "text/vnd.abc", "abc extension → the abc MIME");
                assert_eq!(cue.lead, Duration::ZERO);
                match cue.payload {
                    CuePayload::Inline(bytes) => assert_eq!(bytes, abc, "abc text rides inline"),
                    other => panic!("expected Inline, got {other:?}"),
                }
            }
            other => panic!("expected RenderCue, got {other:?}"),
        }
    }

    /// `kj play --cas <hash>` emits a `CuePayload::Cas` cue whose MIME comes
    /// from the CAS metadata the object was stored with — the clip-prefetch path:
    /// the app sink resolves the hash from its XDG cache / SFTP `/v/cas`.
    #[tokio::test]
    async fn play_cas_emits_a_cas_cue_with_the_stored_mime() {
        use kaijutsu_cas::ContentStore;

        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let principal = PrincipalId::new();
        let ctx = register_context(&dispatcher, Some("c"), None, principal);
        let caller = crate::kj::test_helpers::caller_with_context(ctx);

        // Seed an object into the kernel CAS with a known mime.
        let sample = b"RIFF....WAVE fake but hashable".to_vec();
        let hash = dispatcher
            .kernel()
            .cas()
            .store(&sample, "audio/wav")
            .expect("seed cas");

        let mut sub = dispatcher.kernel().block_flows().subscribe("block.render_cue");

        let result =
            dispatcher.dispatch_play(&["--cas".to_string(), hash.to_string()], &caller);
        assert!(result.is_ok(), "kj play --cas failed: {result:?}");

        let msg = sub.try_recv().expect("RenderCue should have been published");
        match msg.payload {
            BlockFlow::RenderCue { cue, .. } => {
                assert_eq!(cue.mime, "audio/wav", "mime resolved from CAS metadata");
                assert_eq!(cue.lead, Duration::ZERO);
                match cue.payload {
                    CuePayload::Cas(h) => assert_eq!(h, hash, "the cue names the CAS hash"),
                    other => panic!("expected Cas, got {other:?}"),
                }
            }
            other => panic!("expected RenderCue, got {other:?}"),
        }
    }

    /// `kj play --cas <unknown-hash>` is a loud error — no directive published.
    #[tokio::test]
    async fn play_cas_unknown_hash_errors() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let principal = PrincipalId::new();
        let ctx = register_context(&dispatcher, Some("c"), None, principal);
        let caller = crate::kj::test_helpers::caller_with_context(ctx);

        let mut sub = dispatcher.kernel().block_flows().subscribe("block.render_cue");

        let result = dispatcher.dispatch_play(
            &["--cas".to_string(), "00000000000000000000000000000000".to_string()],
            &caller,
        );
        assert!(!result.is_ok(), "unknown CAS hash should error: {result:?}");
        assert!(sub.try_recv().is_none(), "no directive published on error");
    }

    /// A nonexistent file is a loud error — no directive published.
    #[tokio::test]
    async fn play_nonexistent_file_errors() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let principal = PrincipalId::new();
        let ctx = register_context(&dispatcher, Some("c"), None, principal);
        let caller = crate::kj::test_helpers::caller_with_context(ctx);

        let mut sub = dispatcher.kernel().block_flows().subscribe("block.render_cue");

        let result = dispatcher.dispatch_play(
            &["/nonexistent/path/to/x.wav".to_string()],
            &caller,
        );
        assert!(!result.is_ok(), "nonexistent file should error: {result:?}");
        assert!(sub.try_recv().is_none(), "no directive published on error");
    }

    /// An unrecognized extension is a loud error — never a silently guessed
    /// default format. No directive published, and the file is never even
    /// read (format is sniffed before the read).
    #[tokio::test]
    async fn play_unknown_extension_errors() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let principal = PrincipalId::new();
        let ctx = register_context(&dispatcher, Some("c"), None, principal);
        let caller = crate::kj::test_helpers::caller_with_context(ctx);

        let mut sub = dispatcher.kernel().block_flows().subscribe("block.render_cue");

        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("notes.xyz");
        std::fs::write(&path, b"whatever").expect("write file");

        let result = dispatcher.dispatch_play(
            &[path.to_string_lossy().into_owned()],
            &caller,
        );
        assert!(!result.is_ok(), "unknown extension should error: {result:?}");
        assert!(sub.try_recv().is_none(), "no directive published on error");
    }

    // ── kj play --track: commit a clip cell (docs/pcm.md R2) ────────────────

    use kaijutsu_cas::ContentHash;
    use kaijutsu_hyoushigi::TickClock;
    use kaijutsu_types::{Tick, TrackId};

    /// Arm `track` directly on the dispatcher's kernel — the kernel-crate
    /// equivalent of `kj transport attach --track <t>` (a unit test has no
    /// beat scheduler to answer the async attach round trip, and
    /// `schedule_clip_cell` only needs the timeline to be armed, not playing).
    fn arm_track(dispatcher: &crate::kj::KjDispatcher, track: &str) {
        dispatcher.kernel().arm_track_timeline(
            TrackId::new(track).unwrap(),
            TickClock::default(),
            Tick::ZERO,
        );
    }

    /// `kj play <path> --track <t>` commits a clip cell instead of playing
    /// now: no *play-now* `RenderCue` is published (the fire cue still rides
    /// the crossing, a later slice, R3) — but `schedule_clip_cell` DOES now
    /// publish the R4 prepare directive at commit (`PREPARE_MIME`, naming the
    /// clip's `media` hash). The label defaults to the file stem, and `.data`
    /// carries full ids for track/tick/media/record/label/mime — the
    /// two-level reference (docs/pcm.md): `media` is the sample bytes' hash,
    /// `record` is the clip JSON's own (different) hash.
    #[tokio::test]
    async fn play_track_commits_a_clip_cell_from_path() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let principal = PrincipalId::new();
        let ctx = register_context(&dispatcher, Some("c"), None, principal);
        let caller = crate::kj::test_helpers::caller_with_context(ctx);
        arm_track(&dispatcher, "clips");

        let mut sub = dispatcher.kernel().block_flows().subscribe("block.render_cue");

        let dir = tempfile::tempdir().expect("tmpdir");
        let wav_path = dir.path().join("kick-808.wav");
        let sample_bytes = b"RIFF....WAVEfmt not-a-real-wav-but-bytes-are-bytes".to_vec();
        std::fs::write(&wav_path, &sample_bytes).expect("write sample wav");

        let result = dispatcher.dispatch_play(
            &[
                wav_path.to_string_lossy().into_owned(),
                "--track".to_string(),
                "clips".to_string(),
            ],
            &caller,
        );
        assert!(result.is_ok(), "kj play --track failed: {result:?}");
        assert!(
            result.message().contains("kick-808"),
            "message names the (file-stem) label: {}",
            result.message()
        );
        assert!(
            result.message().contains("clips"),
            "message names the track: {}",
            result.message()
        );

        let crate::kj::KjResult::Ok { data, .. } = result else {
            panic!("expected Ok result");
        };
        let data = data.expect("a clip commit must attach structured data");
        assert_eq!(data["track"], "clips");
        assert_eq!(data["label"], "kick-808", "label defaults to the file stem");
        assert_eq!(data["mime"], "audio/wav");
        let expected_media = ContentHash::from_data(&sample_bytes).to_string();
        assert_eq!(data["media"], expected_media, "media hash matches the stored file bytes");

        // The commit mode publishes exactly one directive: the R4 prepare
        // cue (naming the clip's media hash) — never a play-now fire cue.
        let msg = sub
            .try_recv()
            .expect("schedule_clip_cell must publish the R4 prepare directive at commit");
        match msg.payload {
            crate::flows::BlockFlow::RenderCue { cue, .. } => {
                assert_eq!(
                    cue.mime,
                    kaijutsu_audio::PREPARE_MIME,
                    "kj play --track publishes a prepare cue, never a play-now RenderCue"
                );
                match cue.payload {
                    CuePayload::Cas(hash) => {
                        assert_eq!(hash.to_string(), expected_media, "prepare cue names the media hash")
                    }
                    other => panic!("expected Cas(media), got {other:?}"),
                }
            }
            other => panic!("expected RenderCue, got {other:?}"),
        }
        assert!(sub.try_recv().is_none(), "exactly one directive published at commit");
        assert_eq!(data["record"].as_str().unwrap().len(), 32, "record is a 32-hex CAS hash");
        assert_ne!(
            data["record"], data["media"],
            "the record hash and the media hash are different objects (two-level reference)"
        );
        assert!(
            data["tick"].as_i64().unwrap() > 0,
            "the ASAP tick is ahead of the zero-seeded playhead"
        );
    }

    /// `--label` overrides the file-stem default.
    #[tokio::test]
    async fn play_track_label_flag_overrides_file_stem() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let principal = PrincipalId::new();
        let ctx = register_context(&dispatcher, Some("c"), None, principal);
        let caller = crate::kj::test_helpers::caller_with_context(ctx);
        arm_track(&dispatcher, "clips");

        let dir = tempfile::tempdir().expect("tmpdir");
        let wav_path = dir.path().join("kick-808.wav");
        std::fs::write(&wav_path, b"RIFF....WAVEfake").expect("write sample wav");

        let result = dispatcher.dispatch_play(
            &[
                wav_path.to_string_lossy().into_owned(),
                "--track".to_string(),
                "clips".to_string(),
                "--label".to_string(),
                "rimshot dry".to_string(),
            ],
            &caller,
        );
        assert!(result.is_ok(), "kj play --track --label failed: {result:?}");
        let crate::kj::KjResult::Ok { data, .. } = result else {
            panic!("expected Ok result");
        };
        assert_eq!(data.unwrap()["label"], "rimshot dry", "--label overrides the file stem");
    }

    /// `--cas --track` with no `--label` is a loud error — a hash has no
    /// derivable name, and labels are how the score reads.
    #[tokio::test]
    async fn play_track_cas_without_label_errors() {
        use kaijutsu_cas::ContentStore;

        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let principal = PrincipalId::new();
        let ctx = register_context(&dispatcher, Some("c"), None, principal);
        let caller = crate::kj::test_helpers::caller_with_context(ctx);
        arm_track(&dispatcher, "clips");

        let sample = b"RIFF....WAVE fake but hashable".to_vec();
        let hash = dispatcher.kernel().cas().store(&sample, "audio/wav").expect("seed cas");

        let result = dispatcher.dispatch_play(
            &[
                "--cas".to_string(),
                hash.to_string(),
                "--track".to_string(),
                "clips".to_string(),
            ],
            &caller,
        );
        assert!(!result.is_ok(), "--cas --track with no --label must error");
        assert!(
            result.message().contains("--label"),
            "error names the missing flag: {}",
            result.message()
        );
    }

    /// `--cas --track --label` commits, resolving mime from the object's CAS
    /// metadata (no extension to sniff for a bare hash).
    #[tokio::test]
    async fn play_track_cas_with_label_commits() {
        use kaijutsu_cas::ContentStore;

        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let principal = PrincipalId::new();
        let ctx = register_context(&dispatcher, Some("c"), None, principal);
        let caller = crate::kj::test_helpers::caller_with_context(ctx);
        arm_track(&dispatcher, "clips");

        let sample = b"RIFF....WAVE fake but hashable".to_vec();
        let hash = dispatcher.kernel().cas().store(&sample, "audio/wav").expect("seed cas");

        let result = dispatcher.dispatch_play(
            &[
                "--cas".to_string(),
                hash.to_string(),
                "--track".to_string(),
                "clips".to_string(),
                "--label".to_string(),
                "snare".to_string(),
            ],
            &caller,
        );
        assert!(result.is_ok(), "kj play --cas --track --label failed: {result:?}");
        let crate::kj::KjResult::Ok { data, .. } = result else {
            panic!("expected Ok result");
        };
        let data = data.unwrap();
        assert_eq!(data["mime"], "audio/wav", "mime resolved from CAS metadata, not an extension");
        assert_eq!(data["media"], hash.to_string());
        assert_eq!(data["label"], "snare");
    }

    /// A `--track` naming an un-armed track is a loud error explaining how to
    /// arm it — never a silent no-op.
    #[tokio::test]
    async fn play_track_unarmed_errors() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let principal = PrincipalId::new();
        let ctx = register_context(&dispatcher, Some("c"), None, principal);
        let caller = crate::kj::test_helpers::caller_with_context(ctx);
        // Deliberately NOT armed.

        let dir = tempfile::tempdir().expect("tmpdir");
        let wav_path = dir.path().join("kick.wav");
        std::fs::write(&wav_path, b"RIFF....WAVEfake").expect("write sample wav");

        let result = dispatcher.dispatch_play(
            &[
                wav_path.to_string_lossy().into_owned(),
                "--track".to_string(),
                "never-armed".to_string(),
            ],
            &caller,
        );
        assert!(!result.is_ok(), "an unarmed track must error");
        assert!(
            result.message().contains("not armed"),
            "error explains the track isn't armed: {}",
            result.message()
        );
    }

    /// `--at <tick>` places the clip at an explicit absolute tick, verbatim.
    #[tokio::test]
    async fn play_track_at_places_the_clip_at_the_given_tick() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let principal = PrincipalId::new();
        let ctx = register_context(&dispatcher, Some("c"), None, principal);
        let caller = crate::kj::test_helpers::caller_with_context(ctx);
        arm_track(&dispatcher, "clips");

        let dir = tempfile::tempdir().expect("tmpdir");
        let wav_path = dir.path().join("kick.wav");
        std::fs::write(&wav_path, b"RIFF....WAVEfake").expect("write sample wav");

        let result = dispatcher.dispatch_play(
            &[
                wav_path.to_string_lossy().into_owned(),
                "--track".to_string(),
                "clips".to_string(),
                "--at".to_string(),
                "50".to_string(),
            ],
            &caller,
        );
        assert!(result.is_ok(), "kj play --track --at failed: {result:?}");
        let crate::kj::KjResult::Ok { data, .. } = result else {
            panic!("expected Ok result");
        };
        assert_eq!(data.unwrap()["tick"], 50, "explicit --at tick is used verbatim, no ASAP default");
    }

    /// `--at` behind the playhead is a loud error — the same in-the-past gate
    /// every scheduled cell gets (`Timeline::schedule`).
    #[tokio::test]
    async fn play_track_at_in_the_past_errors() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let principal = PrincipalId::new();
        let ctx = register_context(&dispatcher, Some("c"), None, principal);
        let caller = crate::kj::test_helpers::caller_with_context(ctx);
        // Seed the playhead ahead so tick 5 is unambiguously in the past.
        dispatcher.kernel().arm_track_timeline(
            TrackId::new("clips").unwrap(),
            TickClock::default(),
            Tick::new(10),
        );

        let dir = tempfile::tempdir().expect("tmpdir");
        let wav_path = dir.path().join("kick.wav");
        std::fs::write(&wav_path, b"RIFF....WAVEfake").expect("write sample wav");

        let result = dispatcher.dispatch_play(
            &[
                wav_path.to_string_lossy().into_owned(),
                "--track".to_string(),
                "clips".to_string(),
                "--at".to_string(),
                "5".to_string(),
            ],
            &caller,
        );
        assert!(!result.is_ok(), "a tick behind the playhead must error");
    }

    /// `--track` with an unrecognized extension is a loud error too — never a
    /// silently guessed mime (mirrors the play-now path's own gate; `.abc` is
    /// deliberately NOT accepted here — a placed clip is audio, never notation).
    #[tokio::test]
    async fn play_track_unknown_extension_errors() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let principal = PrincipalId::new();
        let ctx = register_context(&dispatcher, Some("c"), None, principal);
        let caller = crate::kj::test_helpers::caller_with_context(ctx);
        arm_track(&dispatcher, "clips");

        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("notes.xyz");
        std::fs::write(&path, b"whatever").expect("write file");

        let result = dispatcher.dispatch_play(
            &[
                path.to_string_lossy().into_owned(),
                "--track".to_string(),
                "clips".to_string(),
            ],
            &caller,
        );
        assert!(!result.is_ok(), "unknown extension should error under --track too");
    }

    // ── Polish guards (kaibo review 2026-07-16) ─────────────────────────────

    /// `--at` (and `--label`) without `--track` is meaningless — clap's
    /// `requires = "track"` rejects it outright instead of silently ignoring
    /// it (the review's UX-footgun finding: `kj play kick.wav --at 50` used
    /// to succeed with `--at` doing nothing).
    #[tokio::test]
    async fn play_at_without_track_is_rejected_not_ignored() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let principal = PrincipalId::new();
        let ctx = register_context(&dispatcher, Some("c"), None, principal);
        let caller = crate::kj::test_helpers::caller_with_context(ctx);

        let dir = tempfile::tempdir().expect("tmpdir");
        let wav_path = dir.path().join("kick.wav");
        std::fs::write(&wav_path, b"RIFF....WAVEfake").expect("write sample wav");

        for stray in [["--at", "50"], ["--label", "kick"]] {
            let result = dispatcher.dispatch_play(
                &[
                    wav_path.to_string_lossy().into_owned(),
                    stray[0].to_string(),
                    stray[1].to_string(),
                ],
                &caller,
            );
            assert!(
                !result.is_ok(),
                "{} without --track must be rejected, not silently ignored: {result:?}",
                stray[0]
            );
        }
    }

    /// A negative `--at` is rejected with the REAL reason (a nonsense tick),
    /// not Timeline::schedule's confusing "InThePast {{ start: Tick(-1) }}".
    #[tokio::test]
    async fn play_track_negative_at_errors_clearly() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let principal = PrincipalId::new();
        let ctx = register_context(&dispatcher, Some("c"), None, principal);
        let caller = crate::kj::test_helpers::caller_with_context(ctx);
        arm_track(&dispatcher, "clips");

        let dir = tempfile::tempdir().expect("tmpdir");
        let wav_path = dir.path().join("kick.wav");
        std::fs::write(&wav_path, b"RIFF....WAVEfake").expect("write sample wav");

        // NB `--at -3` (space-separated) never reaches our guard — clap reads
        // `-3` as a flag and errors at parse. `--at=-3` is the form that
        // parses and must hit the clear-message gate.
        let result = dispatcher.dispatch_play(
            &[
                wav_path.to_string_lossy().into_owned(),
                "--track".to_string(),
                "clips".to_string(),
                "--at=-3".to_string(),
            ],
            &caller,
        );
        assert!(!result.is_ok(), "negative --at must error");
        let msg = format!("{result:?}");
        assert!(
            msg.contains("must be ≥ 0"),
            "the error names the real problem (nonsense tick), not InThePast: {msg}"
        );
    }

    /// A whitespace-only file stem (`"   .wav"`) derives an unusable label —
    /// the error says so and points at `--label`, instead of the opaque
    /// downstream "clip label must be non-empty". (A plain dotfile `.wav` is
    /// NOT this case: `file_stem()` returns the whole name ".wav", which is a
    /// perfectly usable label.)
    #[tokio::test]
    async fn play_track_whitespace_stem_errors_clearly() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let principal = PrincipalId::new();
        let ctx = register_context(&dispatcher, Some("c"), None, principal);
        let caller = crate::kj::test_helpers::caller_with_context(ctx);
        arm_track(&dispatcher, "clips");

        let dir = tempfile::tempdir().expect("tmpdir");
        let ws_path = dir.path().join("   .wav");
        std::fs::write(&ws_path, b"RIFF....WAVEfake").expect("write sample wav");

        let result = dispatcher.dispatch_play(
            &[
                ws_path.to_string_lossy().into_owned(),
                "--track".to_string(),
                "clips".to_string(),
            ],
            &caller,
        );
        assert!(!result.is_ok(), "an underivable label must error");
        let msg = format!("{result:?}");
        assert!(
            msg.contains("--label"),
            "the error points the user at --label: {msg}"
        );
    }
}
