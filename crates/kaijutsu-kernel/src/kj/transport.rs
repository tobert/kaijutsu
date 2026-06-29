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
    /// Arm a context's beat — both the manual restart-recovery (the scheduler's
    /// armed map starts empty on cold start, so a restart stops every beat) and
    /// the create-time entry the musician's `create/` rc calls. Arming IS the
    /// opt-in: a context becomes a beat participant by being armed, not by a
    /// hardcoded type name (so `funkMusician`/`lyricist…` are pure rc). Arms
    /// **stopped** + OODA-armed (no surprise token spend — `kj transport play`
    /// starts the clock). Policy + lane come from the persisted `beat_state`
    /// (real tempo/cadence on a re-arm), else the musician default + a lane
    /// derived from the label; an unsluggable label is refused (no shared lane).
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
    pub(crate) async fn dispatch_transport(&self, argv: &[String], caller: &KjCaller) -> KjResult {
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
                let (policy, track, rotate_every_phrases) = match self.beat_arm_payload(ctx) {
                    Ok(payload) => payload,
                    Err(e) => return KjResult::Err(e),
                };
                let verb = format!("armed (stopped) on lane '{}'", track.as_str());
                (
                    BeatCommand::Arm {
                        context_id: ctx,
                        policy,
                        track,
                        rotate_every_phrases,
                    },
                    verb,
                )
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
                // A BPM above the 1 ms beat floor truncates to a zero period under
                // integer division, and a zero period spins `fire_due` forever
                // (it re-pushes the same instant and never advances past `now`),
                // freezing the whole transport thread. Reject loudly rather than
                // silently clamp — a sub-millisecond beat is never what was meant.
                if bpm > 60_000 {
                    return KjResult::Err(format!(
                        "kj transport tempo: {bpm} BPM exceeds the 1 ms beat floor (max 60000); \
                         a sub-millisecond period would freeze the beat scheduler"
                    ));
                }
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

        // Send and AWAIT the scheduler's verdict so the report reflects what
        // actually happened — not a blind "playing" after a fire-and-forget send
        // that the scheduler silently dropped on an un-armed context.
        let Some(ack_rx) = self.kernel().send_beat_request(cmd) else {
            return KjResult::Err(
                "kj transport: no beat scheduler is active; the command was not applied"
                    .to_string(),
            );
        };
        match ack_rx.await {
            Ok(Ok(())) => KjResult::Ok {
                message: format!("transport: {} '{}'", verb, short_hex(ctx)),
                content_type: ContentType::Plain,
                ephemeral: false,
                data: Some(serde_json::json!({
                    "context_id": ctx.to_hex(),
                    "action": action,
                })),
            },
            // The scheduler refused (e.g. not armed) — report the truth, loudly.
            Ok(Err(reason)) => KjResult::Err(format!("kj transport: {reason}")),
            // The scheduler dropped the reply without answering (it shut down
            // between send and reply) — don't claim success we can't confirm.
            Err(_) => KjResult::Err(
                "kj transport: the beat scheduler dropped the request without a reply".to_string(),
            ),
        }
    }

    /// Assemble the `Arm` payload (policy + lane) for `kj transport arm`. Prefers
    /// the persisted `beat_state` — the real tempo/cadence a restart should
    /// restore — and falls back to the musician default + label-derived lane for
    /// a musician that was never persisted (created before beat-state persistence,
    /// or whose create-time write-through failed). Refuses loudly on a
    /// non-musician context: there is nothing meaningful to arm.
    fn beat_arm_payload(
        &self,
        ctx: ContextId,
    ) -> Result<(BeatPolicy, TrackId, Option<u64>), String> {
        let db = self.kernel_db().lock();
        // Prefer the persisted policy + lane (the real tempo/cadence a restart
        // should restore). A type-changed context still uses its persisted row —
        // arming is an explicit opt-in, so its last-known policy is what's wanted.
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
            return Ok((policy, track, state.rotate_every_phrases));
        }

        // No persisted state — derive the lane from the context label + musician
        // default. Arming is the opt-in: a context is a beat participant exactly
        // by *being armed* (typically from its `create/` rc — musician,
        // funkMusician, …), NOT by a hardcoded `context_type == "musician"` name.
        // The one hard requirement is a real lane: an empty/unsluggable label is
        // refused so two players can't collide on a silent shared lane (the
        // original footgun). `docs/chameleon.md`, "context_type is an rc bundle".
        let row = db
            .get_context(ctx)
            .map_err(|e| format!("kj transport arm: {e}"))?
            .ok_or_else(|| format!("kj transport arm: context {} not found", short_hex(ctx)))?;
        let label = row.label.unwrap_or_default();
        let track = TrackId::new(label.as_str())
            .ok()
            .or_else(|| TrackId::slugify(&label))
            .ok_or_else(|| {
                format!(
                    "kj transport arm: context {} label {label:?} yields no valid track \
                     lane (a beat participant needs a lane)",
                    short_hex(ctx)
                )
            })?;
        Ok((BeatPolicy::musician_default(), track, None))
    }
}

fn short_hex(id: kaijutsu_types::ContextId) -> String {
    id.to_hex().chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::hyoushigi::{BeatAck, BeatCommand, BeatRequest};
    use crate::kj::test_helpers::*;
    use kaijutsu_types::PrincipalId;

    fn s(v: &str) -> String {
        v.to_string()
    }

    /// Stand in for the beat scheduler: ack every request with `ack` and forward
    /// the command on the returned channel for inspection. Dispatch now AWAITS
    /// the scheduler's ack, so a test that merely held the ingress receiver would
    /// hang on the await — this replies (and lets a test assert the NACK path by
    /// passing an `Err`). The command is forwarded before the ack is sent, so it
    /// is already present by the time `dispatch` returns.
    fn spawn_beat_stub(
        mut ingress: tokio::sync::mpsc::UnboundedReceiver<BeatRequest>,
        ack: BeatAck,
    ) -> tokio::sync::mpsc::UnboundedReceiver<BeatCommand> {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(BeatRequest { command, reply }) = ingress.recv().await {
                let _ = cmd_tx.send(command);
                if let Some(reply) = reply {
                    let _ = reply.send(ack.clone());
                }
            }
        });
        cmd_rx
    }

    #[tokio::test]
    async fn transport_play_sends_play_command_to_scheduler() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(d.kernel().set_beat_ingress(tx), "ingress installs once");
        let mut cmds = spawn_beat_stub(rx, Ok(()));

        let result = d.dispatch(&[s("transport"), s("play")], &c).await;
        assert!(result.is_ok(), "transport play failed: {}", result.message());
        match cmds.recv().await.expect("a BeatCommand should be sent") {
            BeatCommand::Play(id) => assert_eq!(id, ctx, "plays the current context"),
            other => panic!("expected Play, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transport_play_reports_scheduler_refusal() {
        // The blind-success fix: when the scheduler NACKs (e.g. the context isn't
        // armed) `kj transport` reports an ERROR with the reason, never a blind
        // "playing". Stub the refusal and assert dispatch surfaces it.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let _cmds = spawn_beat_stub(
            rx,
            Err("context is not armed — run `kj transport arm` first".to_string()),
        );

        let result = d.dispatch(&[s("transport"), s("play")], &c).await;
        assert!(
            matches!(result, crate::kj::KjResult::Err(_)),
            "a scheduler refusal must surface as an error, not blind success"
        );
        assert!(
            result.message().contains("not armed"),
            "the error carries the scheduler's reason: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn transport_tempo_rejects_absurd_bpm() {
        // A BPM that truncates the period to 0 ms would spin the beat scheduler;
        // reject it loudly instead of silently clamping. 60000 BPM == 1 ms is the
        // last valid value; 60001 truncates to 0.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(d.kernel().set_beat_ingress(tx), "ingress installs once");
        let mut cmds = spawn_beat_stub(rx, Ok(()));

        let bad = d
            .dispatch(&[s("transport"), s("tempo"), s("60001")], &c)
            .await;
        assert!(
            matches!(bad, crate::kj::KjResult::Err(_)),
            "60001 BPM must be rejected (would be a 0 ms period)"
        );
        assert!(
            cmds.try_recv().is_err(),
            "a rejected tempo must NOT reach the scheduler"
        );

        let ok = d
            .dispatch(&[s("transport"), s("tempo"), s("60000")], &c)
            .await;
        assert!(ok.is_ok(), "60000 BPM (1 ms) is the valid floor: {}", ok.message());
        match cmds.recv().await.expect("a valid tempo reaches the scheduler") {
            BeatCommand::SetTempo { period, .. } => {
                assert_eq!(period, Duration::from_millis(1), "60000 BPM == 1 ms period");
            }
            other => panic!("expected SetTempo, got {other:?}"),
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
        d.kernel_db()
            .lock()
            .upsert_beat_state(
                ctx,
                &PersistedBeatState {
                    period_ms: 250,
                    beats_per_phrase: 12,
                    ooda_every: 96,
                    track: "bass".to_string(),
                    rotate_every_phrases: Some(4),
                    playhead_tick: None,
                },
            )
            .unwrap();
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let mut cmds = spawn_beat_stub(rx, Ok(()));

        let result = d.dispatch(&[s("transport"), s("arm")], &c).await;
        assert!(result.is_ok(), "transport arm failed: {}", result.message());
        match cmds.recv().await.expect("an Arm should be sent") {
            BeatCommand::Arm { context_id, policy, track, rotate_every_phrases } => {
                assert_eq!(context_id, ctx);
                assert_eq!(policy.period, Duration::from_millis(250), "persisted tempo restored");
                assert_eq!(policy.beats_per_phrase, 12);
                assert_eq!(policy.ooda_every, 96);
                assert_eq!(track, TrackId::new("bass").unwrap(), "persisted lane restored");
                assert_eq!(
                    rotate_every_phrases,
                    Some(4),
                    "persisted rotate cadence restored on re-arm (the cold-restart page-turn fix)"
                );
            }
            other => panic!("expected Arm, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transport_arm_falls_back_to_default_when_unpersisted() {
        // No persisted beat_state → re-arm on the musician default + a lane
        // derived from the label. The context_type is irrelevant — arming is the
        // opt-in, so any context with a sluggable label gets a beat (this is what
        // lets funkMusician/lyricist be pure rc, no hardcoded type name).
        use kaijutsu_types::TrackId;

        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("Lead Synth"), None, principal);
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let mut cmds = spawn_beat_stub(rx, Ok(()));

        let result = d.dispatch(&[s("transport"), s("arm")], &c).await;
        assert!(result.is_ok(), "transport arm failed: {}", result.message());
        match cmds.recv().await.expect("an Arm should be sent") {
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
    async fn transport_arm_refuses_when_no_track_lane() {
        // The one hard gate that survives: a context whose label yields no track
        // lane can't be armed (it would collide on a silent shared lane). Refuse
        // loudly rather than send a lane-less Arm.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, None, None, principal); // no label → no lane
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let mut cmds = spawn_beat_stub(rx, Ok(()));

        let result = d.dispatch(&[s("transport"), s("arm")], &c).await;
        assert!(!result.is_ok(), "arming a context with no track lane should fail");
        assert!(
            result.message().contains("no valid track lane"),
            "error should explain why: {}",
            result.message()
        );
        assert!(cmds.try_recv().is_err(), "no Arm command sent on refusal");
    }

    #[tokio::test]
    async fn rotate_rc_forks_arms_and_plays_the_child_on_the_parent_track() {
        // End-to-end page-turn: the `rotate` lifecycle (musician/rotate/S10-rotate.kai)
        // forks a thin child, switches the rc shell to it, and arms + re-rotates +
        // plays it — on the PARENT's track (via the fork's beat_state copy), since a
        // spawn-fork has no label of its own. set_self_arc is required for kj-in-rc.
        use crate::kernel_db::PersistedBeatState;
        use kaijutsu_types::TrackId;
        use std::collections::HashMap;

        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        // The `spawn` factory preset the rotate rc forks with is seeded at server
        // startup; test_dispatcher doesn't, so seed it here.
        crate::seed_presets::ensure_factory_presets(
            &mut d.kernel_db().lock(),
            PrincipalId::new(),
        )
        .unwrap();
        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("bass"), None, principal);
        d.kernel_db().lock().update_context_type(parent, "musician").unwrap();
        // The scheduler would have persisted this on arm; seed it so the fork copies it.
        d.kernel_db()
            .lock()
            .upsert_beat_state(
                parent,
                &PersistedBeatState {
                    period_ms: 500,
                    beats_per_phrase: 16,
                    ooda_every: 128,
                    track: "bass".to_string(),
                    rotate_every_phrases: None,
                    playhead_tick: None,
                },
            )
            .unwrap();

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        // The rc runs `arm && rotate && play`, each AWAITing the scheduler ack;
        // the stub acks Ok so the `&&` chain proceeds, and forwards each command.
        let mut cmds = spawn_beat_stub(rx, Ok(()));

        // Fire the rotate lifecycle exactly as the beat scheduler's fire_rotate
        // does, seeding $ROTATE_EVERY (the scheduler sets it only when rotating).
        let caller = caller_with_context(parent);
        let mut vars = HashMap::new();
        vars.insert("ROTATE_EVERY".to_string(), "4".to_string());
        d.run_rc_lifecycle_with_vars("rotate", parent, None, None, None, &vars, &caller)
            .await
            .expect("rotate lifecycle runs");

        // The page-turn emits, in order, three commands — all for the forked CHILD
        // (the rc `--switch`ed onto it), never the retired parent.
        let arm = cmds.recv().await.expect("rotate arms the child");
        let (child, track) = match arm {
            BeatCommand::Arm { context_id, track, .. } => (context_id, track),
            other => panic!("expected Arm first, got {other:?}"),
        };
        assert_ne!(child, parent, "the child is a NEW context, not the parent");
        assert_eq!(track, TrackId::new("bass").unwrap(), "child keeps the parent's lane");

        match cmds.recv().await.expect("rotate re-establishes the cadence") {
            BeatCommand::SetRotate { context_id, every_phrases } => {
                assert_eq!(context_id, child, "cadence set on the child");
                assert_eq!(every_phrases, Some(4), "the song keeps turning at the same cadence");
            }
            other => panic!("expected SetRotate, got {other:?}"),
        }
        match cmds.recv().await.expect("rotate plays the child") {
            BeatCommand::Play(id) => assert_eq!(id, child, "the child's clock starts (seamless continuation)"),
            other => panic!("expected Play, got {other:?}"),
        }

        // The child really is a fork of the parent.
        let forked_from = d.kernel_db().lock().get_context(child).unwrap().unwrap().forked_from;
        assert_eq!(forked_from, Some(parent), "child is forked from the parent");
    }

    #[tokio::test]
    async fn transport_arm_uses_persisted_row_regardless_of_type() {
        // A type-changed context still carries (and uses) its persisted beat_state
        // — arming is an explicit opt-in, so the last-known policy/lane is what's
        // wanted, not a refusal. (Replaces the former type-gate refusal: the gate
        // is now "has a lane", not "is a musician".)
        use crate::kernel_db::PersistedBeatState;
        use kaijutsu_types::TrackId;

        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("bass"), None, principal); // "default" type
        d.kernel_db()
            .lock()
            .upsert_beat_state(
                ctx,
                &PersistedBeatState {
                    period_ms: 250,
                    beats_per_phrase: 12,
                    ooda_every: 96,
                    track: "bass".to_string(),
                    rotate_every_phrases: None,
                    playhead_tick: None,
                },
            )
            .unwrap();
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let mut cmds = spawn_beat_stub(rx, Ok(()));

        let result = d.dispatch(&[s("transport"), s("arm")], &c).await;
        assert!(result.is_ok(), "arm with a persisted row should succeed: {}", result.message());
        match cmds.recv().await.expect("an Arm should be sent") {
            BeatCommand::Arm { policy, track, .. } => {
                assert_eq!(policy.period, Duration::from_millis(250), "uses the persisted policy");
                assert_eq!(track, TrackId::new("bass").unwrap());
            }
            other => panic!("expected Arm, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transport_tempo_converts_bpm_to_period() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let mut cmds = spawn_beat_stub(rx, Ok(()));

        let result = d.dispatch(&[s("transport"), s("tempo"), s("120")], &c).await;
        assert!(result.is_ok(), "tempo failed: {}", result.message());
        match cmds.recv().await.expect("a SetTempo should be sent") {
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
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let mut cmds = spawn_beat_stub(rx, Ok(()));

        let result = d.dispatch(&[s("transport"), s("ooda"), s("off")], &c).await;
        assert!(result.is_ok(), "ooda off failed: {}", result.message());
        match cmds.recv().await.expect("a SetOoda should be sent") {
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
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let mut cmds = spawn_beat_stub(rx, Ok(()));

        let result = d
            .dispatch(&[s("transport"), s("rotate"), s("--every"), s("8")], &c)
            .await;
        assert!(result.is_ok(), "rotate --every failed: {}", result.message());
        match cmds.recv().await.expect("a SetRotate should be sent") {
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
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let mut cmds = spawn_beat_stub(rx, Ok(()));

        let result = d.dispatch(&[s("transport"), s("rotate"), s("off")], &c).await;
        assert!(result.is_ok(), "rotate off failed: {}", result.message());
        match cmds.recv().await.expect("a SetRotate should be sent") {
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
