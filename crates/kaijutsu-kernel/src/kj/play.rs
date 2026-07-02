//! `kj play <path>` — resolve an audio sample and push a render cue over the
//! FlowBus (docs/pcm.md "The wire").
//!
//! The kernel never touches audio hardware. `kj play` reads the file, sniffs
//! its format from the extension to derive the wire MIME, wraps the bytes in a
//! play-now [`RenderCue`] (inline, `lead == 0` — correct for this slice; a
//! `Cas` payload is the primary path once the speculation-lead prefetch lands,
//! slice 5c), and publishes `BlockFlow::RenderCue` — the same FlowBus every
//! `BlockEvents` subscription bridge already drains, so every attached
//! client's render sink receives it with no new transport.

use clap::Parser;
use kaijutsu_audio::{AudioFormatHint, CuePayload, RenderCue};
use kaijutsu_cas::{ContentHash, ContentStore};
use kaijutsu_types::ContentType;

use super::refs;
use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};
use crate::flows::BlockFlow;

#[derive(Parser, Debug)]
#[command(
    name = "play",
    about = "Play an audio sample over the connected clients' render targets (docs/pcm.md)",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct PlayArgs {
    /// Path to the audio file to play (OS path, not a VFS path — mirrors `kj cas
    /// put`). Mutually exclusive with `--cas`; exactly one is required.
    path: Option<String>,
    /// Play a blob already in the CAS by its content hash. The MIME is resolved
    /// from the blob's CAS metadata, and the cue carries a `Cas` payload — so the
    /// sink resolves the bytes from its XDG cache / SFTP `/v/blobs` (the
    /// clip-prefetch path, docs/pcm.md 5c / docs/slash-v.md track B).
    #[arg(long, conflicts_with = "path")]
    cas: Option<String>,
    /// Target context: . (default) | <label> | <hex prefix>. Reserved for
    /// future per-listener routing; the standalone slice forwards to every
    /// attached client regardless of which context is named.
    #[arg(long, short = 'c')]
    context: Option<String>,
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

        // Build the cue payload from exactly one source: a CAS hash (bytes stay
        // out-of-band, the sink resolves them from /v/blobs) or a local file
        // (bytes inline). `mime` + `desc` + `payload` fall out of whichever.
        let (mime, payload, desc): (String, CuePayload, String) =
            match (parsed.cas.as_deref(), parsed.path.as_deref()) {
                (Some(hash_str), _) => {
                    let hash = match hash_str.parse::<ContentHash>() {
                        Ok(h) => h,
                        Err(e) => return KjResult::Err(format!("kj play --cas: invalid hash: {e}")),
                    };
                    // The MIME comes from the blob's own CAS metadata — no
                    // extension to sniff. A blob not in the pool is a loud error.
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
    /// from the CAS metadata the blob was stored with — the clip-prefetch path:
    /// the app sink resolves the hash from its XDG cache / SFTP `/v/blobs`.
    #[tokio::test]
    async fn play_cas_emits_a_cas_cue_with_the_stored_mime() {
        use kaijutsu_cas::ContentStore;

        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let principal = PrincipalId::new();
        let ctx = register_context(&dispatcher, Some("c"), None, principal);
        let caller = crate::kj::test_helpers::caller_with_context(ctx);

        // Seed a blob into the kernel CAS with a known mime.
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
}
