//! Transport subcommand: the composer's play/stop/pause/tempo control surface.
//!
//! The playhead *is* a transport position (拍子木 marks *now* and stages *what's
//! next*), so `kj transport` exposes it. Two switches per context: the **clock**
//! (`play`/`pause`/`stop`) and the **OODA-arm** (`ooda on|off`). The context tick
//! is event-counted, so `pause` freezes musical time and `play` resumes at +1 —
//! no rewind (the playhead is forward-only; revisiting the past is an export, not
//! a seek).
//!
//! The kernel can't reach the server's beat scheduler directly; it sends a
//! [`BeatCommand`](crate::hyoushigi::BeatCommand) over the ingress the server
//! installed at startup. Like `kj drive`, when no scheduler is wired this is an
//! explicit user command, so it reports the failure rather than silently
//! no-opping.

use clap::{Parser, Subcommand};
use kaijutsu_types::ContentType;

use super::refs;
use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};
use crate::hyoushigi::BeatCommand;

#[derive(Parser, Debug)]
#[command(
    name = "transport",
    about = "Transport control for a context's beat (the composer playhead)",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct TransportArgs {
    #[command(subcommand)]
    command: TransportCommand,
}

#[derive(Subcommand, Debug)]
enum TransportCommand {
    /// Start/resume the clock.
    Play {
        /// Target context: . (default) | <label> | <hex prefix>
        #[arg(long)]
        context: Option<String>,
    },
    /// Hold the clock (freeze the playhead).
    Pause {
        /// Target context: . (default) | <label> | <hex prefix>
        #[arg(long)]
        context: Option<String>,
    },
    /// Pause the clock and disarm OODA.
    Stop {
        /// Target context: . (default) | <label> | <hex prefix>
        #[arg(long)]
        context: Option<String>,
    },
    /// Set the beat period from a BPM value.
    Tempo {
        /// Beats per minute (positive integer)
        bpm: Option<u64>,
        /// Target context: . (default) | <label> | <hex prefix>
        #[arg(long)]
        context: Option<String>,
    },
    /// Arm/disarm the OODA loop.
    Ooda {
        /// `on` to arm, `off` to disarm
        state: Option<String>,
        /// Target context: . (default) | <label> | <hex prefix>
        #[arg(long)]
        context: Option<String>,
    },
}

impl TransportCommand {
    /// The `--context` ref this verb targets (shared across all verbs).
    fn context(&self) -> Option<&str> {
        match self {
            TransportCommand::Play { context }
            | TransportCommand::Pause { context }
            | TransportCommand::Stop { context }
            | TransportCommand::Tempo { context, .. }
            | TransportCommand::Ooda { context, .. } => context.as_deref(),
        }
    }

    /// The verb name for the result `action` field / data payload.
    fn action(&self) -> &'static str {
        match self {
            TransportCommand::Play { .. } => "play",
            TransportCommand::Pause { .. } => "pause",
            TransportCommand::Stop { .. } => "stop",
            TransportCommand::Tempo { .. } => "tempo",
            TransportCommand::Ooda { .. } => "ooda",
        }
    }
}

impl KjDispatcher {
    pub(crate) fn dispatch_transport(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<TransportArgs>();
        }
        let parsed = match TransportArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj transport: {e}"));
            }
        };
        let command = parsed.command;

        // Driving the beat (play/pause/stop/tempo/ooda) is gated on `transport`.
        if let Err(denied) =
            self.require_cap(caller, crate::mcp::Capability::Transport, "transport")
        {
            return denied;
        }

        // Target context: `--context <ref>`, else the caller's current context.
        let ctx = {
            let db = self.kernel_db().lock();
            match refs::resolve_context_arg(command.context(), caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj transport: {e}")),
            }
        };

        let action = command.action();

        let (cmd, verb): (BeatCommand, String) = match &command {
            TransportCommand::Play { .. } => (BeatCommand::Play(ctx), "playing".into()),
            TransportCommand::Pause { .. } => (BeatCommand::Pause(ctx), "paused".into()),
            TransportCommand::Stop { .. } => (BeatCommand::Stop(ctx), "stopped".into()),
            TransportCommand::Tempo { bpm, .. } => {
                let Some(bpm) = bpm.filter(|b| *b > 0) else {
                    return KjResult::Err(
                        "kj transport tempo: need a positive BPM, e.g. `kj transport tempo 120`"
                            .to_string(),
                    );
                };
                let period = std::time::Duration::from_millis(60_000 / bpm);
                (
                    BeatCommand::SetTempo { context_id: ctx, period },
                    format!("tempo {bpm} BPM"),
                )
            }
            TransportCommand::Ooda { state, .. } => {
                let armed = match state.as_deref() {
                    Some("on") => true,
                    Some("off") => false,
                    _ => {
                        return KjResult::Err(
                            "kj transport ooda: expected `on` or `off`".to_string(),
                        );
                    }
                };
                (
                    BeatCommand::SetOoda { context_id: ctx, armed },
                    format!("OODA {}", if armed { "armed" } else { "disarmed" }),
                )
            }
        };

        if !self.kernel().send_beat_command(cmd) {
            return KjResult::Err(
                "kj transport: no beat scheduler is active; the command was not applied"
                    .to_string(),
            );
        }

        KjResult::Ok {
            message: format!("transport: {} '{}'", verb, short_hex(ctx)),
            content_type: ContentType::Plain,
            ephemeral: false,
            data: Some(serde_json::json!({
                "context_id": ctx.to_hex(),
                "action": action,
            })),
        }
    }
}

fn short_hex(id: kaijutsu_types::ContextId) -> String {
    id.to_hex().chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::hyoushigi::BeatCommand;
    use crate::kj::test_helpers::*;
    use kaijutsu_types::PrincipalId;

    fn s(v: &str) -> String {
        v.to_string()
    }

    #[tokio::test]
    async fn transport_play_sends_play_command_to_scheduler() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(d.kernel().set_beat_ingress(tx), "ingress installs once");

        let result = d.dispatch(&[s("transport"), s("play")], &c).await;
        assert!(result.is_ok(), "transport play failed: {}", result.message());
        match rx.try_recv().expect("a BeatCommand should be sent") {
            BeatCommand::Play(id) => assert_eq!(id, ctx, "plays the current context"),
            other => panic!("expected Play, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transport_tempo_converts_bpm_to_period() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);

        let result = d.dispatch(&[s("transport"), s("tempo"), s("120")], &c).await;
        assert!(result.is_ok(), "tempo failed: {}", result.message());
        match rx.try_recv().expect("a SetTempo should be sent") {
            BeatCommand::SetTempo { context_id, period } => {
                assert_eq!(context_id, ctx);
                assert_eq!(period, Duration::from_millis(500), "120 BPM → 500 ms/beat");
            }
            other => panic!("expected SetTempo, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transport_ooda_off_disarms() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);

        let result = d.dispatch(&[s("transport"), s("ooda"), s("off")], &c).await;
        assert!(result.is_ok(), "ooda off failed: {}", result.message());
        match rx.try_recv().expect("a SetOoda should be sent") {
            BeatCommand::SetOoda { context_id, armed } => {
                assert_eq!(context_id, ctx);
                assert!(!armed, "ooda off → disarmed");
            }
            other => panic!("expected SetOoda, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transport_without_scheduler_errors_explicitly() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);
        // No beat ingress installed → no scheduler wired.

        let result = d.dispatch(&[s("transport"), s("play")], &c).await;
        assert!(!result.is_ok(), "must error when no scheduler is wired");
        assert!(
            result.message().contains("no beat scheduler"),
            "error should name the missing scheduler: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn transport_tempo_rejects_nonpositive_bpm() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);
        d.kernel()
            .set_beat_ingress(tokio::sync::mpsc::unbounded_channel().0);

        let result = d.dispatch(&[s("transport"), s("tempo"), s("0")], &c).await;
        assert!(!result.is_ok(), "0 BPM must be rejected");
    }
}
