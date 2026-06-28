//! Transport subcommand: the musician's play/stop/pause/tempo control surface.
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

use std::time::Duration;

use clap::{Parser, Subcommand};
use kaijutsu_types::{ContentType, ContextId, TrackId};

use super::refs;
use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};
use crate::hyoushigi::{BeatCommand, BeatPolicy};

#[derive(Parser, Debug)]
#[command(
    name = "transport",
    about = "Transport control for a context's beat (the musician playhead)",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct TransportArgs {
    #[command(subcommand)]
    command: TransportCommand,
}

#[derive(Subcommand, Debug)]
enum TransportCommand {
    /// Re-arm a musician's beat after a kernel restart. The scheduler's armed
    /// map starts empty on cold start, so a restart silently stops every
    /// musician's beat (auto-arm fires only on context *create*); this is the
    /// manual recovery. Arms **stopped** + OODA-armed (no surprise token spend —
    /// `kj transport play` starts the clock). The policy + lane come from the
    /// persisted `beat_state` (the real tempo/cadence), falling back to the
    /// musician default + label-derived lane for a never-persisted musician.
    Arm {
        /// Target context: . (default) | <label> | <hex prefix>
        #[arg(long)]
        context: Option<String>,
    },
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
    /// Set (or clear) the self-fork rotate cadence — the page-turn. At every
    /// phrase horizon where `phrase % N == 0` the scheduler retires this context
    /// and fires the `rotate` rc lifecycle (fork a `spawn` child + arm it). The
    /// detach is synchronous in the scheduler (Rust), so it can't race the beat;
    /// the fork/arm action stays rc.
    Rotate {
        /// Phrases per rotation (positive). Omit and pass `off` to disable.
        #[arg(long)]
        every: Option<u64>,
        /// `off` to clear the rotate cadence.
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
            TransportCommand::Arm { context }
            | TransportCommand::Play { context }
            | TransportCommand::Pause { context }
            | TransportCommand::Stop { context }
            | TransportCommand::Tempo { context, .. }
            | TransportCommand::Ooda { context, .. }
            | TransportCommand::Rotate { context, .. } => context.as_deref(),
        }
    }

    /// The verb name for the result `action` field / data payload.
    fn action(&self) -> &'static str {
        match self {
            TransportCommand::Arm { .. } => "arm",
            TransportCommand::Play { .. } => "play",
            TransportCommand::Pause { .. } => "pause",
            TransportCommand::Stop { .. } => "stop",
            TransportCommand::Tempo { .. } => "tempo",
            TransportCommand::Ooda { .. } => "ooda",
            TransportCommand::Rotate { .. } => "rotate",
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
            TransportCommand::Arm { .. } => {
                let (policy, track) = match self.beat_arm_payload(ctx) {
                    Ok(payload) => payload,
                    Err(e) => return KjResult::Err(e),
                };
                let verb = format!("armed (stopped) on lane '{}'", track.as_str());
                (BeatCommand::Arm { context_id: ctx, policy, track }, verb)
            }
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
            TransportCommand::Rotate { every, state, .. } => {
                let every_phrases = match (every, state.as_deref()) {
                    // `off` clears the cadence.
                    (_, Some("off")) => None,
                    (Some(n), _) if *n > 0 => Some(*n),
                    (Some(_), _) => {
                        return KjResult::Err(
                            "kj transport rotate: --every needs a positive phrase count".to_string(),
                        );
                    }
                    (None, _) => {
                        return KjResult::Err(
                            "kj transport rotate: pass `--every N` to set the cadence, or `off` to \
                             clear it"
                                .to_string(),
                        );
                    }
                };
                let verb = match every_phrases {
                    Some(n) => format!("rotate every {n} phrase(s)"),
                    None => "rotate off".to_string(),
                };
                (BeatCommand::SetRotate { context_id: ctx, every_phrases }, verb)
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

    /// Assemble the `Arm` payload (policy + lane) for `kj transport arm`. Prefers
    /// the persisted `beat_state` — the real tempo/cadence a restart should
    /// restore — and falls back to the musician default + label-derived lane for
    /// a musician that was never persisted (created before beat-state persistence,
    /// or whose create-time write-through failed). Refuses loudly on a
    /// non-musician context: there is nothing meaningful to arm.
    fn beat_arm_payload(&self, ctx: ContextId) -> Result<(BeatPolicy, TrackId), String> {
        let db = self.kernel_db().lock();
        // Verify the context is a musician FIRST, regardless of whether a
        // beat_state row exists. A context whose type was changed away from
        // musician still carries its stale beat_state; arming it would start a
        // beat on a non-musician. The type gate is the authority, not the row.
        let row = db
            .get_context(ctx)
            .map_err(|e| format!("kj transport arm: {e}"))?
            .ok_or_else(|| format!("kj transport arm: context {} not found", short_hex(ctx)))?;
        if row.context_type != "musician" {
            return Err(format!(
                "kj transport arm: context {} is a '{}', not a musician — nothing to arm",
                short_hex(ctx),
                row.context_type
            ));
        }

        // Prefer the persisted policy + lane (the real tempo/cadence a restart
        // should restore).
        if let Some(state) = db
            .get_beat_state(ctx)
            .map_err(|e| format!("kj transport arm: reading beat state: {e}"))?
        {
            // The stored track was a valid TrackId when written; a value that no
            // longer parses is corruption, so fail loud rather than re-arm onto a
            // bad lane. (get_beat_state already rejects an empty track / zero period.)
            let track = TrackId::new(state.track.as_str()).map_err(|e| {
                format!("kj transport arm: stored track {:?} is invalid: {e}", state.track)
            })?;
            let policy = BeatPolicy {
                period: Duration::from_millis(state.period_ms),
                beats_per_phrase: state.beats_per_phrase,
                ooda_every: state.ooda_every,
            };
            return Ok((policy, track));
        }

        // No persisted state — derive the lane from the label + musician default.
        let label = row.label.unwrap_or_default();
        // Mirror the create path's lane derivation: strict first, then lossy-but-
        // loud slugify; an empty slug is a hard error (never a silent shared lane).
        let track = TrackId::new(label.as_str())
            .ok()
            .or_else(|| TrackId::slugify(&label))
            .ok_or_else(|| {
                format!(
                    "kj transport arm: musician label {label:?} yields no valid track id \
                     (slug is empty)"
                )
            })?;
        Ok((BeatPolicy::musician_default(), track))
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
    async fn transport_arm_uses_persisted_beat_state() {
        // The point of beat-state persistence: a restart re-arm restores the real
        // tempo/cadence, not BeatPolicy::musician_default().
        use crate::kernel_db::PersistedBeatState;
        use kaijutsu_types::TrackId;

        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("bass"), None, principal);
        d.kernel_db().lock().update_context_type(ctx, "musician").unwrap();
        d.kernel_db()
            .lock()
            .upsert_beat_state(
                ctx,
                &PersistedBeatState {
                    period_ms: 250,
                    beats_per_phrase: 12,
                    ooda_every: 96,
                    track: "bass".to_string(),
                },
            )
            .unwrap();
        let c = caller_with_context(ctx);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);

        let result = d.dispatch(&[s("transport"), s("arm")], &c).await;
        assert!(result.is_ok(), "transport arm failed: {}", result.message());
        match rx.try_recv().expect("an Arm should be sent") {
            BeatCommand::Arm { context_id, policy, track } => {
                assert_eq!(context_id, ctx);
                assert_eq!(policy.period, Duration::from_millis(250), "persisted tempo restored");
                assert_eq!(policy.beats_per_phrase, 12);
                assert_eq!(policy.ooda_every, 96);
                assert_eq!(track, TrackId::new("bass").unwrap(), "persisted lane restored");
            }
            other => panic!("expected Arm, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transport_arm_falls_back_to_default_for_unpersisted_musician() {
        // A musician with no persisted beat_state (created before persistence, or
        // a failed create-time write) re-arms on the musician default + a lane
        // derived from its label — never silently un-armed.
        use kaijutsu_types::TrackId;

        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("Lead Synth"), None, principal);
        // register_context stamps "default"; make it a musician so the fallback applies.
        d.kernel_db().lock().update_context_type(ctx, "musician").unwrap();
        let c = caller_with_context(ctx);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);

        let result = d.dispatch(&[s("transport"), s("arm")], &c).await;
        assert!(result.is_ok(), "transport arm failed: {}", result.message());
        match rx.try_recv().expect("an Arm should be sent") {
            BeatCommand::Arm { policy, track, .. } => {
                assert_eq!(policy.period, Duration::from_millis(500), "musician default tempo");
                assert_eq!(policy.beats_per_phrase, 16);
                assert_eq!(
                    track,
                    TrackId::slugify("Lead Synth").unwrap(),
                    "lane slugified from the label"
                );
            }
            other => panic!("expected Arm, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transport_arm_refuses_non_musician_without_state() {
        // Arming a non-musician with no persisted beat_state has nothing to arm —
        // refuse loudly rather than send a meaningless Arm.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("coder-ctx"), None, principal); // "default" type
        let c = caller_with_context(ctx);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);

        let result = d.dispatch(&[s("transport"), s("arm")], &c).await;
        assert!(!result.is_ok(), "arming a non-musician should fail");
        assert!(
            result.message().contains("not a musician"),
            "error should explain why: {}",
            result.message()
        );
        assert!(rx.try_recv().is_err(), "no Arm command sent on refusal");
    }

    #[tokio::test]
    async fn transport_arm_refuses_former_musician_with_stale_state() {
        // A context that WAS a musician (so it carries a stale beat_state row) but
        // whose type was changed away must still be refused — the type gate is the
        // authority, not the persisted row. Guards the short-circuit DeepSeek flagged.
        use crate::kernel_db::PersistedBeatState;

        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("bass"), None, principal);
        d.kernel_db().lock().update_context_type(ctx, "musician").unwrap();
        d.kernel_db()
            .lock()
            .upsert_beat_state(
                ctx,
                &PersistedBeatState {
                    period_ms: 250,
                    beats_per_phrase: 12,
                    ooda_every: 96,
                    track: "bass".to_string(),
                },
            )
            .unwrap();
        // Type changed away from musician — the row is now stale.
        d.kernel_db().lock().update_context_type(ctx, "default").unwrap();
        let c = caller_with_context(ctx);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);

        let result = d.dispatch(&[s("transport"), s("arm")], &c).await;
        assert!(!result.is_ok(), "arming a former musician should still fail");
        assert!(
            result.message().contains("not a musician"),
            "the type gate refuses despite a persisted row: {}",
            result.message()
        );
        assert!(rx.try_recv().is_err(), "no Arm command sent on refusal");
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
    async fn transport_rotate_every_sets_cadence() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);

        let result = d
            .dispatch(&[s("transport"), s("rotate"), s("--every"), s("8")], &c)
            .await;
        assert!(result.is_ok(), "rotate --every failed: {}", result.message());
        match rx.try_recv().expect("a SetRotate should be sent") {
            BeatCommand::SetRotate { context_id, every_phrases } => {
                assert_eq!(context_id, ctx);
                assert_eq!(every_phrases, Some(8));
            }
            other => panic!("expected SetRotate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transport_rotate_off_clears_cadence() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);

        let result = d.dispatch(&[s("transport"), s("rotate"), s("off")], &c).await;
        assert!(result.is_ok(), "rotate off failed: {}", result.message());
        match rx.try_recv().expect("a SetRotate should be sent") {
            BeatCommand::SetRotate { every_phrases, .. } => {
                assert_eq!(every_phrases, None, "off clears the cadence");
            }
            other => panic!("expected SetRotate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transport_rotate_needs_every_or_off() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);

        let result = d.dispatch(&[s("transport"), s("rotate")], &c).await;
        assert!(!result.is_ok(), "bare rotate must error");
        assert!(
            result.message().contains("--every") && result.message().contains("off"),
            "should teach the two forms: {}",
            result.message()
        );
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
