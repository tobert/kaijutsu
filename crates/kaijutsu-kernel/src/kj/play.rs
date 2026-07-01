//! `kj play <path>` — resolve an audio sample and push a play directive over
//! the FlowBus (docs/pcm.md "The wire").
//!
//! The kernel never touches audio hardware. `kj play` reads the file, sniffs
//! its format from the extension, wraps the bytes in an [`AudioRef::Encoded`]
//! (inline — correct for this slice; a `Cas` ref is the primary path once the
//! speculation-lead prefetch lands, slice 5), and publishes
//! `BlockFlow::PlayAudio` — the same FlowBus every `BlockEvents` subscription
//! bridge already drains, so every attached client's `AudioRenderTarget`
//! receives it with no new transport.

use clap::Parser;
use kaijutsu_audio::{AudioFormatHint, AudioRef};
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
    /// Path to the audio file to play (OS path, not a VFS path — mirrors `kj cas put`).
    path: String,
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

        // Sniff the format from the extension BEFORE reading the file — an
        // unrecognized/missing extension is a loud error, never a silently
        // guessed default (a mis-decoded sample is a worse failure mode than
        // a rejected command).
        let format = match AudioFormatHint::from_path_extension(&parsed.path) {
            Some(f) => f,
            None => {
                return KjResult::Err(format!(
                    "kj play: {}: unrecognized or missing audio extension \
                     (expected one of .wav/.flac/.mp3/.ogg/.aac/.m4a)",
                    parsed.path
                ));
            }
        };

        let bytes = match std::fs::read(&parsed.path) {
            Ok(b) => b,
            Err(e) => return KjResult::Err(format!("kj play: {}: {}", parsed.path, e)),
        };

        let context_id = {
            let db = self.kernel_db().lock();
            match refs::resolve_context_arg(parsed.context.as_deref(), caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj play: {e}")),
            }
        };

        let byte_count = bytes.len();
        let audio = AudioRef::Encoded { bytes, format };
        let receivers = self.kernel().block_flows().publish(BlockFlow::PlayAudio {
            context_id,
            audio,
        });

        KjResult::ok_ephemeral(
            format!(
                "playing {} ({} bytes, {}) — {} listener(s)",
                parsed.path,
                byte_count,
                format.mime(),
                receivers,
            ),
            ContentType::Plain,
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::flows::BlockFlow;
    use crate::kj::test_helpers::{register_context, test_dispatcher};
    use kaijutsu_audio::AudioFormatHint;
    use kaijutsu_types::PrincipalId;
    use std::sync::Arc;

    /// TDD anchor: `kj play <tempwav>` must publish a `BlockFlow::PlayAudio`
    /// on the FlowBus, carrying the file's bytes verbatim and the sniffed
    /// format. Subscribe BEFORE dispatch — the FlowBus is a live broadcast,
    /// not a queue a late subscriber can catch up on.
    #[tokio::test]
    async fn play_publishes_block_flow_play_audio() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let principal = PrincipalId::new();
        let ctx = register_context(&dispatcher, Some("c"), None, principal);
        let caller = crate::kj::test_helpers::caller_with_context(ctx);

        let mut sub = dispatcher.kernel().block_flows().subscribe("block.play_audio");

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
            .expect("BlockFlow::PlayAudio should have been published");
        match msg.payload {
            BlockFlow::PlayAudio { context_id, audio } => {
                assert_eq!(context_id, ctx, "directive names the resolved context");
                match audio {
                    kaijutsu_audio::AudioRef::Encoded { bytes, format } => {
                        assert_eq!(bytes, sample_bytes, "bytes ride the directive verbatim");
                        assert_eq!(format, AudioFormatHint::Wav, "extension sniffed as wav");
                    }
                    other => panic!("expected Encoded, got {other:?}"),
                }
            }
            other => panic!("expected PlayAudio, got {other:?}"),
        }
    }

    /// A nonexistent file is a loud error — no directive published.
    #[tokio::test]
    async fn play_nonexistent_file_errors() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let principal = PrincipalId::new();
        let ctx = register_context(&dispatcher, Some("c"), None, principal);
        let caller = crate::kj::test_helpers::caller_with_context(ctx);

        let mut sub = dispatcher.kernel().block_flows().subscribe("block.play_audio");

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

        let mut sub = dispatcher.kernel().block_flows().subscribe("block.play_audio");

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
