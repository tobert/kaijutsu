//! Transport subcommand: the musician's play/stop/pause/tempo control surface.
//!
//! The beat lives on the **track** now (`docs/tracks.md`, Stage 1). A context
//! *attaches* to a track to be beaten by it. Transport is a single operation
//! on one clock domain — no per-context fan-out. Clock-domain verbs name the
//! **track** (`--track <name>`); kaish does the context→track lookup when
//! `--track` is omitted, so `kj` stays crisp.
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
use crate::hyoushigi::{Attachment, BeatCommand, BeatPolicy, Cadence};

#[derive(Parser, Debug)]
#[command(
    name = "transport",
    about = "Transport control for a track's beat clock (the musician playhead)",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct TransportArgs {
    #[command(subcommand)]
    command: TransportCommand,
}

#[derive(Subcommand, Debug)]
enum TransportCommand {
    /// Attach a context to a track — the context announces itself (entity #3 in
    /// docs/tracks.md). This is the opt-in: a context becomes a beat participant
    /// by attaching, not by a hardcoded type name. If the track doesn't exist yet
    /// it is created stopped (no surprise token spend); if it already exists the
    /// new attachment registers without changing the track's clock. Arms
    /// **stopped** + OODA-armed (`kj transport play` starts the clock). Policy
    /// and attachment come from the persisted rows when present (a restart re-attach
    /// restores the real tempo/cadence), else from the musician defaults + a lane
    /// derived from the label; an unsluggable label is refused (no shared lane).
    Attach {
        /// Target context: . (default) | <label> | <hex prefix>
        #[arg(long)]
        context: Option<String>,
        /// Track (clock domain) to attach to. Omit to resolve from the context's
        /// persisted attachment, or (for a fresh first-attach) to derive from the
        /// context label.
        #[arg(long)]
        track: Option<String>,
        /// Wake this context every N beats on the track clock (wakeup divisor).
        /// Overrides any persisted value. Default: musician default (128 beats).
        #[arg(long)]
        wakeup: Option<u64>,
        /// Self-fork rotate cadence in phrases. Overrides any persisted value.
        /// Omit to inherit the persisted cadence (or no auto-rotation by default).
        #[arg(long)]
        rotate: Option<u64>,
    },
    /// Detach a context from a track — unbind. Used by rotation's parent-side and
    /// by archive. The track persists with its remaining attachments.
    Detach {
        /// Target context: . (default) | <label> | <hex prefix>
        #[arg(long)]
        context: Option<String>,
        /// Track to detach from. Omit to resolve from the context's persisted
        /// attachment (exact-one required; error if ambiguous).
        #[arg(long)]
        track: Option<String>,
    },
    /// Start/resume the track's clock.
    Play {
        /// Target context (used for track lookup when --track is absent).
        #[arg(long)]
        context: Option<String>,
        /// Track to play. Omit to resolve from the context's attachment.
        #[arg(long)]
        track: Option<String>,
    },
    /// Hold the track's clock (freeze the playhead).
    Pause {
        /// Target context (used for track lookup when --track is absent).
        #[arg(long)]
        context: Option<String>,
        /// Track to pause. Omit to resolve from the context's attachment.
        #[arg(long)]
        track: Option<String>,
    },
    /// Stop the track's clock (MIDI idiom: stop = stop the clock only). Rotation
    /// is suspended/remembered, not cleared; per-attachment OODA arm is untouched.
    /// Use `kj transport ooda off` to disarm OODA separately.
    Stop {
        /// Target context (used for track lookup when --track is absent).
        #[arg(long)]
        context: Option<String>,
        /// Track to stop. Omit to resolve from the context's attachment.
        #[arg(long)]
        track: Option<String>,
    },
    /// Set the beat period from a BPM value.
    Tempo {
        /// Beats per minute (positive integer)
        bpm: Option<u64>,
        /// Target context (used for track lookup when --track is absent).
        #[arg(long)]
        context: Option<String>,
        /// Track whose tempo to set. Omit to resolve from the context's attachment.
        #[arg(long)]
        track: Option<String>,
    },
    /// Arm/disarm one attached context's OODA loop, without touching the clock.
    Ooda {
        /// `on` to arm, `off` to disarm
        state: Option<String>,
        /// Target context: . (default) | <label> | <hex prefix>
        #[arg(long)]
        context: Option<String>,
        /// Track this attachment belongs to. Omit to resolve from the context's
        /// persisted attachment.
        #[arg(long)]
        track: Option<String>,
    },
    /// Set (or clear) the self-fork rotate cadence — the page-turn. At every
    /// phrase horizon where `phrase % N == 0` the scheduler retires this context
    /// and fires the `rotate` rc lifecycle (fork a `spawn` child + attach it). The
    /// detach is synchronous in the scheduler (Rust), so it can't race the beat;
    /// the fork/attach action stays rc.
    Rotate {
        /// Phrases per rotation (positive). Omit and pass `off` to disable.
        #[arg(long)]
        every: Option<u64>,
        /// `off` to clear the rotate cadence.
        state: Option<String>,
        /// Target context: . (default) | <label> | <hex prefix>
        #[arg(long)]
        context: Option<String>,
        /// Track this attachment belongs to. Omit to resolve from the context's
        /// persisted attachment.
        #[arg(long)]
        track: Option<String>,
    },
}

impl TransportCommand {
    /// The `--context` ref this verb targets (shared across all verbs).
    fn context(&self) -> Option<&str> {
        match self {
            TransportCommand::Attach { context, .. }
            | TransportCommand::Detach { context, .. }
            | TransportCommand::Play { context, .. }
            | TransportCommand::Pause { context, .. }
            | TransportCommand::Stop { context, .. }
            | TransportCommand::Tempo { context, .. }
            | TransportCommand::Ooda { context, .. }
            | TransportCommand::Rotate { context, .. } => context.as_deref(),
        }
    }

    /// The `--track` override this verb carries (all verbs that name a track).
    fn track_name(&self) -> Option<&str> {
        match self {
            TransportCommand::Attach { track, .. }
            | TransportCommand::Detach { track, .. }
            | TransportCommand::Play { track, .. }
            | TransportCommand::Pause { track, .. }
            | TransportCommand::Stop { track, .. }
            | TransportCommand::Tempo { track, .. }
            | TransportCommand::Ooda { track, .. }
            | TransportCommand::Rotate { track, .. } => track.as_deref(),
        }
    }

    /// The verb name for the result `action` field / data payload.
    fn action(&self) -> &'static str {
        match self {
            TransportCommand::Attach { .. } => "attach",
            TransportCommand::Detach { .. } => "detach",
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
        // All verbs use the context — for `attach`/`detach` it's the binding target;
        // for clock ops it resolves the track when `--track` is absent.
        let ctx = {
            let db = self.kernel_db().lock();
            match refs::resolve_context_arg(command.context(), caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj transport: {e}")),
            }
        };

        let action = command.action();
        let track_name = command.track_name();

        let (cmd, verb, track_id_for_data): (BeatCommand, String, Option<String>) =
            match &command {
                TransportCommand::Attach { wakeup, rotate, .. } => {
                    let (track, attachment, policy) =
                        match self.beat_attach_payload(ctx, track_name, *wakeup, *rotate) {
                            Ok(p) => p,
                            Err(e) => return KjResult::Err(e),
                        };
                    let verb = format!("attached to track '{}'", track.as_str());
                    let tid = track.as_str().to_string();
                    (BeatCommand::Attach { track, context_id: ctx, attachment, policy }, verb, Some(tid))
                }

                TransportCommand::Detach { .. } => {
                    let track = match self.resolve_track_for_ctx(track_name, ctx) {
                        Ok(t) => t,
                        Err(e) => return KjResult::Err(e),
                    };
                    let verb = format!("detached from track '{}'", track.as_str());
                    let tid = track.as_str().to_string();
                    (BeatCommand::Detach { track, context_id: ctx }, verb, Some(tid))
                }

                TransportCommand::Play { .. } => {
                    let track = match self.resolve_track_for_ctx(track_name, ctx) {
                        Ok(t) => t,
                        Err(e) => return KjResult::Err(e),
                    };
                    let verb = format!("playing (track '{}')", track.as_str());
                    let tid = track.as_str().to_string();
                    (BeatCommand::Play(track), verb, Some(tid))
                }

                TransportCommand::Pause { .. } => {
                    let track = match self.resolve_track_for_ctx(track_name, ctx) {
                        Ok(t) => t,
                        Err(e) => return KjResult::Err(e),
                    };
                    let verb = format!("paused (track '{}')", track.as_str());
                    let tid = track.as_str().to_string();
                    (BeatCommand::Pause(track), verb, Some(tid))
                }

                TransportCommand::Stop { .. } => {
                    let track = match self.resolve_track_for_ctx(track_name, ctx) {
                        Ok(t) => t,
                        Err(e) => return KjResult::Err(e),
                    };
                    let verb = format!("stopped (track '{}')", track.as_str());
                    let tid = track.as_str().to_string();
                    (BeatCommand::Stop(track), verb, Some(tid))
                }

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
                    let period = Duration::from_millis(60_000 / bpm);
                    let track = match self.resolve_track_for_ctx(track_name, ctx) {
                        Ok(t) => t,
                        Err(e) => return KjResult::Err(e),
                    };
                    let tid = track.as_str().to_string();
                    (BeatCommand::SetTempo { track, period }, format!("tempo {bpm} BPM"), Some(tid))
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
                    let track = match self.resolve_track_for_ctx(track_name, ctx) {
                        Ok(t) => t,
                        Err(e) => return KjResult::Err(e),
                    };
                    let tid = track.as_str().to_string();
                    (
                        BeatCommand::SetOoda { track, context_id: ctx, armed },
                        format!("OODA {}", if armed { "armed" } else { "disarmed" }),
                        Some(tid),
                    )
                }

                TransportCommand::Rotate { every, state, .. } => {
                    let every_cadence = match (every, state.as_deref()) {
                        // `off` clears the cadence.
                        (_, Some("off")) => None,
                        (Some(n), _) if *n > 0 => Some(Cadence::new(*n)),
                        (Some(_), _) => {
                            return KjResult::Err(
                                "kj transport rotate: --every needs a positive phrase count"
                                    .to_string(),
                            );
                        }
                        (None, _) => {
                            return KjResult::Err(
                                "kj transport rotate: pass `--every N` to set the cadence, or `off` \
                                 to clear it"
                                    .to_string(),
                            );
                        }
                    };
                    let track = match self.resolve_track_for_ctx(track_name, ctx) {
                        Ok(t) => t,
                        Err(e) => return KjResult::Err(e),
                    };
                    let verb = match &every_cadence {
                        Some(c) => format!("rotate every {} phrase(s)", c.every),
                        None => "rotate off".to_string(),
                    };
                    let tid = track.as_str().to_string();
                    (
                        BeatCommand::SetRotate { track, context_id: ctx, every: every_cadence },
                        verb,
                        Some(tid),
                    )
                }
            };

        // Send and AWAIT the scheduler's verdict so the report reflects what
        // actually happened — not a blind "playing" after a fire-and-forget send
        // that the scheduler silently dropped on an un-attached context.
        let Some(ack_rx) = self.kernel().send_beat_request(cmd) else {
            return KjResult::Err(
                "kj transport: no beat scheduler is active; the command was not applied"
                    .to_string(),
            );
        };
        match ack_rx.await {
            Ok(Ok(())) => KjResult::Ok {
                message: format!("transport: {} '{}'", verb, ctx.short()),
                content_type: ContentType::Plain,
                ephemeral: false,
                data: Some(serde_json::json!({
                    "context_id": ctx.to_hex(),
                    "track_id": track_id_for_data,
                    "action": action,
                })),
            },
            // The scheduler refused (e.g. not attached) — report the truth, loudly.
            Ok(Err(reason)) => KjResult::Err(format!("kj transport: {reason}")),
            // The scheduler dropped the reply without answering (it shut down
            // between send and reply) — don't claim success we can't confirm.
            Err(_) => KjResult::Err(
                "kj transport: the beat scheduler dropped the request without a reply".to_string(),
            ),
        }
    }

    /// Resolve the target [`TrackId`] for a clock-domain or per-attachment verb.
    ///
    /// If `track_override` is `Some`, parse it directly. Otherwise look up the
    /// context's persisted attachment set: exactly one row → use its `track_id`;
    /// zero rows → loud error (attach first); more than one → loud error
    /// (ambiguous; Stage 1 music has one). The stored track string that no longer
    /// parses is corruption → fail loud.
    fn resolve_track_for_ctx(
        &self,
        track_override: Option<&str>,
        ctx: ContextId,
    ) -> Result<TrackId, String> {
        if let Some(name) = track_override {
            return TrackId::new(name).map_err(|e| {
                format!("kj transport: invalid track name {name:?}: {e}")
            });
        }
        let db = self.kernel_db().lock();
        let attachments = db
            .list_attachments_for_context(ctx)
            .map_err(|e| format!("kj transport: reading attachments for context: {e}"))?;
        match attachments.as_slice() {
            [] => Err(format!(
                "kj transport: context {} is not attached to a track — \
                 run `kj transport attach` first",
                ctx.short()
            )),
            [one] => TrackId::new(&one.track_id).map_err(|e| {
                format!(
                    "kj transport: stored track {:?} is invalid (corruption): {e}",
                    one.track_id
                )
            }),
            many => Err(format!(
                "kj transport: context {} is attached to {} tracks — \
                 pass --track <name> to disambiguate",
                ctx.short(),
                many.len()
            )),
        }
    }

    /// Assemble the [`BeatCommand::Attach`] payload for `kj transport attach`.
    ///
    /// Returns `(TrackId, Attachment, BeatPolicy)`. Logic:
    ///
    /// - **Track**: `track_override` → persisted attachment's `track_id` (if
    ///   exactly one attachment) → label-derived lane for a fresh first-attach.
    ///   An unsluggable label is refused (no shared lane). A stored track that no
    ///   longer parses is corruption → fail loud.
    /// - **Policy**: `get_track(track_id)` restores the real tempo/cadence on a
    ///   restart re-attach; else `BeatPolicy::musician_default()`.
    /// - **Attachment**: built from the persisted attachment row when present
    ///   (`wakeup_every`/`rotate_every_phrases`/`ooda_armed`), else
    ///   `Attachment::musician_default()`; CLI flags `wakeup_override`/`rotate_override`
    ///   win over both. Pulse always starts at 0 on an attach.
    fn beat_attach_payload(
        &self,
        ctx: ContextId,
        track_override: Option<&str>,
        wakeup_override: Option<u64>,
        rotate_override: Option<u64>,
    ) -> Result<(TrackId, Attachment, BeatPolicy), String> {
        let db = self.kernel_db().lock();

        // 1. Resolve the track.
        let track = if let Some(name) = track_override {
            TrackId::new(name).map_err(|e| {
                format!("kj transport attach: invalid track {name:?}: {e}")
            })?
        } else {
            let attachments = db
                .list_attachments_for_context(ctx)
                .map_err(|e| format!("kj transport attach: reading attachments: {e}"))?;
            match attachments.as_slice() {
                [] => {
                    // Fresh first-attach: derive the lane from the context label.
                    // A beat participant needs a real lane; empty/unsluggable labels
                    // are refused so two players can't collide on a silent shared lane.
                    // This is also the path the musician create rc takes (label ≈ role).
                    let row = db
                        .get_context(ctx)
                        .map_err(|e| format!("kj transport attach: {e}"))?
                        .ok_or_else(|| {
                            format!("kj transport attach: context {} not found", ctx.short())
                        })?;
                    let label = row.label.unwrap_or_default();
                    TrackId::new(label.as_str())
                        .ok()
                        .or_else(|| TrackId::slugify(&label))
                        .ok_or_else(|| {
                            format!(
                                "kj transport attach: context {} label {label:?} yields no valid \
                                 track lane (a beat participant needs a lane)",
                                ctx.short()
                            )
                        })?
                }
                [one] => {
                    // Re-attach (restart recovery or rotation page-turn): use the
                    // stored lane. A value that no longer parses is corruption → loud.
                    TrackId::new(&one.track_id).map_err(|e| {
                        format!(
                            "kj transport attach: stored track {:?} is invalid (corruption): {e}",
                            one.track_id
                        )
                    })?
                }
                many => {
                    return Err(format!(
                        "kj transport attach: context {} is attached to {} tracks — \
                         pass --track <name>",
                        ctx.short(),
                        many.len()
                    ));
                }
            }
        };

        // 2. Resolve the policy (real tempo/cadence on a restart; else musician default).
        let policy = db
            .get_track(track.as_str())
            .map_err(|e| format!("kj transport attach: reading track: {e}"))?
            .map(|t| BeatPolicy {
                period: Duration::from_millis(t.period_ms),
                beats_per_phrase: t.beats_per_phrase,
            })
            .unwrap_or_else(BeatPolicy::musician_default);

        // 3. Resolve the attachment (persisted row → musician default, then CLI wins).
        let persisted = db
            .get_attachment(track.as_str(), ctx)
            .map_err(|e| format!("kj transport attach: reading attachment: {e}"))?;

        let mut attachment = persisted
            .map(|p| Attachment {
                wakeup: Cadence::new(p.wakeup_every),
                rotate: p.rotate_every_phrases.map(Cadence::new),
                ooda_armed: p.ooda_armed,
                pulse: 0,
            })
            .unwrap_or_else(Attachment::musician_default);

        // CLI overrides win over whatever was persisted.
        if let Some(w) = wakeup_override {
            attachment.wakeup = Cadence::new(w);
        }
        if let Some(r) = rotate_override {
            attachment.rotate = Some(Cadence::new(r));
        }

        Ok((track, attachment, policy))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::hyoushigi::{BeatAck, BeatCommand, BeatRequest, Cadence};
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
            while let Some(req) = ingress.recv().await {
                match req {
                    BeatRequest::Command { command, reply } => {
                        let _ = cmd_tx.send(command);
                        if let Some(reply) = reply {
                            let _ = reply.send(ack.clone());
                        }
                    }
                    // The stub schedules nothing, so it owns no tracks.
                    BeatRequest::Snapshot { reply } => {
                        let _ = reply.send(Vec::new());
                    }
                    // …and holds no attachments, so a capture commit refuses.
                    BeatRequest::CommitCapture { reply, .. } => {
                        let _ = reply.send(Err("beat stub: no tracks".into()));
                    }
                }
            }
        });
        cmd_rx
    }

    // ── helper: seed a track + attachment for a context ──────────────────────
    fn seed_track_and_attachment(
        d: &super::super::KjDispatcher,
        ctx: kaijutsu_types::ContextId,
        track_name: &str,
        rotate_every_phrases: Option<u64>,
    ) {
        use crate::kernel_db::{PersistedAttachment, PersistedTrack};
        d.kernel_db()
            .lock()
            .upsert_track(&PersistedTrack {
                track_id: track_name.to_string(),
                period_ms: 500,
                beats_per_phrase: 16,
                playhead_tick: None,
                playing: false,
                score_context_id: None,
                clock_kind: "system".to_string(),
            })
            .unwrap();
        d.kernel_db()
            .lock()
            .upsert_attachment(&PersistedAttachment {
                track_id: track_name.to_string(),
                context_id: ctx,
                wakeup_every: 128,
                rotate_every_phrases,
                ooda_armed: true,
            })
            .unwrap();
    }

    // ── play ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn transport_play_sends_play_command_to_scheduler() {
        // Using --track bypasses the context→track lookup, testing the direct path.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(d.kernel().set_beat_ingress(tx), "ingress installs once");
        let mut cmds = spawn_beat_stub(rx, Ok(()));

        let result = d
            .dispatch(&[s("transport"), s("play"), s("--track"), s("c")], &c)
            .await;
        assert!(result.is_ok(), "transport play failed: {}", result.message());
        match cmds.recv().await.expect("a BeatCommand should be sent") {
            BeatCommand::Play(id) => {
                assert_eq!(id, kaijutsu_types::TrackId::new("c").unwrap(), "plays track c")
            }
            other => panic!("expected Play, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transport_play_without_track_resolves_from_attachment() {
        // When --track is omitted, the track is looked up from the context's
        // persisted attachment. The Play command carries the TrackId, not the
        // ContextId.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("bass"), None, principal);
        seed_track_and_attachment(&d, ctx, "bass", None);
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let mut cmds = spawn_beat_stub(rx, Ok(()));

        let result = d.dispatch(&[s("transport"), s("play")], &c).await;
        assert!(result.is_ok(), "transport play failed: {}", result.message());
        match cmds.recv().await.expect("a BeatCommand should be sent") {
            BeatCommand::Play(track) => {
                assert_eq!(
                    track,
                    kaijutsu_types::TrackId::new("bass").unwrap(),
                    "play targets the track from the persisted attachment"
                )
            }
            other => panic!("expected Play(TrackId), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transport_play_without_track_errors_when_no_attachment() {
        // A context with no attachment yields a loud error — not a silent no-op.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        // No attachment seeded.
        let c = caller_with_context(ctx);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);

        let result = d.dispatch(&[s("transport"), s("play")], &c).await;
        assert!(!result.is_ok(), "play with no attachment must error");
        assert!(
            result.message().contains("not attached to a track"),
            "error explains the missing attachment: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn transport_play_reports_scheduler_refusal() {
        // The blind-success fix: when the scheduler NACKs (e.g. the track isn't
        // live) `kj transport` reports an ERROR with the reason, never blind
        // "playing". Stub the refusal and assert dispatch surfaces it.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let _cmds = spawn_beat_stub(
            rx,
            Err("track is not live — run `kj transport attach` first".to_string()),
        );

        let result = d
            .dispatch(&[s("transport"), s("play"), s("--track"), s("bass")], &c)
            .await;
        assert!(
            matches!(result, crate::kj::KjResult::Err(_)),
            "a scheduler refusal must surface as an error, not blind success"
        );
        assert!(
            result.message().contains("not live"),
            "the error carries the scheduler's reason: {}",
            result.message()
        );
    }

    // ── tempo ─────────────────────────────────────────────────────────────────

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
            .dispatch(
                &[s("transport"), s("tempo"), s("60001"), s("--track"), s("c")],
                &c,
            )
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
            .dispatch(
                &[s("transport"), s("tempo"), s("60000"), s("--track"), s("c")],
                &c,
            )
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
    async fn transport_tempo_converts_bpm_to_period() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let mut cmds = spawn_beat_stub(rx, Ok(()));

        let result = d
            .dispatch(
                &[s("transport"), s("tempo"), s("120"), s("--track"), s("bass")],
                &c,
            )
            .await;
        assert!(result.is_ok(), "tempo failed: {}", result.message());
        match cmds.recv().await.expect("a SetTempo should be sent") {
            BeatCommand::SetTempo { track, period } => {
                assert_eq!(track, kaijutsu_types::TrackId::new("bass").unwrap());
                assert_eq!(period, Duration::from_millis(500), "120 BPM → 500 ms/beat");
            }
            other => panic!("expected SetTempo, got {other:?}"),
        }
    }

    // ── attach ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn transport_attach_uses_persisted_attachment() {
        // The point of attachment persistence: a restart re-attach restores the
        // real tempo/cadence and wakeup cadence, not the musician defaults.
        use crate::kernel_db::{PersistedAttachment, PersistedTrack};
        use kaijutsu_types::TrackId;

        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("bass"), None, principal);
        d.kernel_db()
            .lock()
            .upsert_track(&PersistedTrack {
                track_id: "bass".to_string(),
                period_ms: 250,
                beats_per_phrase: 12,
                playhead_tick: None,
                playing: false,
                score_context_id: None,
                clock_kind: "system".to_string(),
            })
            .unwrap();
        d.kernel_db()
            .lock()
            .upsert_attachment(&PersistedAttachment {
                track_id: "bass".to_string(),
                context_id: ctx,
                wakeup_every: 96,
                rotate_every_phrases: Some(4),
                ooda_armed: true,
            })
            .unwrap();

        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let mut cmds = spawn_beat_stub(rx, Ok(()));

        let result = d.dispatch(&[s("transport"), s("attach")], &c).await;
        assert!(result.is_ok(), "transport attach failed: {}", result.message());
        match cmds.recv().await.expect("an Attach should be sent") {
            BeatCommand::Attach { context_id, policy, track, attachment } => {
                assert_eq!(context_id, ctx);
                assert_eq!(policy.period, Duration::from_millis(250), "persisted tempo restored");
                assert_eq!(policy.beats_per_phrase, 12);
                assert_eq!(track, TrackId::new("bass").unwrap(), "persisted lane restored");
                assert_eq!(
                    attachment.wakeup,
                    Cadence::new(96),
                    "persisted wakeup cadence restored"
                );
                assert_eq!(
                    attachment.rotate,
                    Some(Cadence::new(4)),
                    "persisted rotate cadence restored on re-attach"
                );
                assert!(attachment.ooda_armed, "persisted ooda_armed restored");
            }
            other => panic!("expected Attach, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transport_attach_falls_back_to_default_when_unpersisted() {
        // No persisted attachment → attach on the musician default + a lane
        // derived from the label. The context_type is irrelevant — attaching is
        // the opt-in, so any context with a sluggable label gets a beat.
        use kaijutsu_types::TrackId;

        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("Lead Synth"), None, principal);
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let mut cmds = spawn_beat_stub(rx, Ok(()));

        let result = d.dispatch(&[s("transport"), s("attach")], &c).await;
        assert!(result.is_ok(), "transport attach failed: {}", result.message());
        match cmds.recv().await.expect("an Attach should be sent") {
            BeatCommand::Attach { policy, track, attachment, .. } => {
                assert_eq!(policy.period, Duration::from_millis(500), "musician default tempo");
                assert_eq!(policy.beats_per_phrase, 32, "musician default phrase = 8 bars");
                assert_eq!(
                    track,
                    TrackId::slugify("Lead Synth").unwrap(),
                    "lane slugified from the label"
                );
                assert_eq!(
                    attachment.wakeup,
                    Cadence::new(32),
                    "musician default wakeup = one phrase (compose back-to-back)"
                );
                assert!(attachment.ooda_armed, "musician default: OODA armed");
            }
            other => panic!("expected Attach, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transport_attach_refuses_when_no_track_lane() {
        // The one hard gate: a context whose label yields no track lane can't be
        // attached. Refuse loudly rather than send an Attach with an empty lane.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, None, None, principal); // no label → no lane
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let mut cmds = spawn_beat_stub(rx, Ok(()));

        let result = d.dispatch(&[s("transport"), s("attach")], &c).await;
        assert!(!result.is_ok(), "attaching a context with no track lane should fail");
        assert!(
            result.message().contains("no valid track lane"),
            "error should explain why: {}",
            result.message()
        );
        assert!(cmds.try_recv().is_err(), "no Attach command sent on refusal");
    }

    #[tokio::test]
    async fn transport_attach_with_track_flag_overrides_derived_lane() {
        // --track <name> bypasses both the persisted-attachment lookup and the
        // label-derivation; the Attach command carries exactly the named track.
        use kaijutsu_types::TrackId;

        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        // Context has no attachment yet; with --track the label isn't consulted.
        let ctx = register_context(&d, None, None, principal);
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let mut cmds = spawn_beat_stub(rx, Ok(()));

        let result = d
            .dispatch(&[s("transport"), s("attach"), s("--track"), s("bass")], &c)
            .await;
        assert!(result.is_ok(), "attach --track bass failed: {}", result.message());
        match cmds.recv().await.expect("an Attach should be sent") {
            BeatCommand::Attach { track, .. } => {
                assert_eq!(track, TrackId::new("bass").unwrap(), "--track flag wins");
            }
            other => panic!("expected Attach, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transport_attach_wakeup_and_rotate_overrides() {
        // --wakeup and --rotate override the persisted (or default) cadences.
        use kaijutsu_types::TrackId;

        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("bass"), None, principal);
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let mut cmds = spawn_beat_stub(rx, Ok(()));

        let result = d
            .dispatch(
                &[
                    s("transport"),
                    s("attach"),
                    s("--wakeup"),
                    s("64"),
                    s("--rotate"),
                    s("8"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "attach with overrides failed: {}", result.message());
        match cmds.recv().await.expect("an Attach should be sent") {
            BeatCommand::Attach { track, attachment, .. } => {
                assert_eq!(track, TrackId::new("bass").unwrap());
                assert_eq!(attachment.wakeup, Cadence::new(64), "--wakeup 64 override");
                assert_eq!(
                    attachment.rotate,
                    Some(Cadence::new(8)),
                    "--rotate 8 override"
                );
            }
            other => panic!("expected Attach, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transport_attach_uses_persisted_row_regardless_of_type() {
        // A type-changed context still carries (and uses) its persisted attachment
        // row — attaching is an explicit opt-in, so the last-known policy/lane is
        // what's wanted, not a refusal.
        use crate::kernel_db::{PersistedAttachment, PersistedTrack};
        use kaijutsu_types::TrackId;

        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("bass"), None, principal); // "default" type
        d.kernel_db()
            .lock()
            .upsert_track(&PersistedTrack {
                track_id: "bass".to_string(),
                period_ms: 250,
                beats_per_phrase: 12,
                playhead_tick: None,
                playing: false,
                score_context_id: None,
                clock_kind: "system".to_string(),
            })
            .unwrap();
        d.kernel_db()
            .lock()
            .upsert_attachment(&PersistedAttachment {
                track_id: "bass".to_string(),
                context_id: ctx,
                wakeup_every: 96,
                rotate_every_phrases: None,
                ooda_armed: true,
            })
            .unwrap();

        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let mut cmds = spawn_beat_stub(rx, Ok(()));

        let result = d.dispatch(&[s("transport"), s("attach")], &c).await;
        assert!(
            result.is_ok(),
            "attach with a persisted row should succeed: {}",
            result.message()
        );
        match cmds.recv().await.expect("an Attach should be sent") {
            BeatCommand::Attach { policy, track, .. } => {
                assert_eq!(policy.period, Duration::from_millis(250), "uses the persisted policy");
                assert_eq!(track, TrackId::new("bass").unwrap());
            }
            other => panic!("expected Attach, got {other:?}"),
        }
    }

    // ── detach ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn transport_detach_builds_detach_command() {
        use kaijutsu_types::TrackId;

        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("bass"), None, principal);
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let mut cmds = spawn_beat_stub(rx, Ok(()));

        // --track bypasses the attachment lookup (nothing is seeded here).
        let result = d
            .dispatch(&[s("transport"), s("detach"), s("--track"), s("bass")], &c)
            .await;
        assert!(result.is_ok(), "detach failed: {}", result.message());
        match cmds.recv().await.expect("a Detach should be sent") {
            BeatCommand::Detach { track, context_id } => {
                assert_eq!(track, TrackId::new("bass").unwrap());
                assert_eq!(context_id, ctx);
            }
            other => panic!("expected Detach, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transport_detach_resolves_track_from_attachment() {
        use kaijutsu_types::TrackId;

        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("bass"), None, principal);
        seed_track_and_attachment(&d, ctx, "bass", None);
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let mut cmds = spawn_beat_stub(rx, Ok(()));

        let result = d.dispatch(&[s("transport"), s("detach")], &c).await;
        assert!(result.is_ok(), "detach (no --track) failed: {}", result.message());
        match cmds.recv().await.expect("a Detach should be sent") {
            BeatCommand::Detach { track, context_id } => {
                assert_eq!(track, TrackId::new("bass").unwrap());
                assert_eq!(context_id, ctx);
            }
            other => panic!("expected Detach, got {other:?}"),
        }
    }

    // ── ooda / rotate ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn transport_ooda_off_disarms() {
        use kaijutsu_types::TrackId;

        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let mut cmds = spawn_beat_stub(rx, Ok(()));

        let result = d
            .dispatch(
                &[s("transport"), s("ooda"), s("off"), s("--track"), s("bass")],
                &c,
            )
            .await;
        assert!(result.is_ok(), "ooda off failed: {}", result.message());
        match cmds.recv().await.expect("a SetOoda should be sent") {
            BeatCommand::SetOoda { track, context_id, armed } => {
                assert_eq!(track, TrackId::new("bass").unwrap());
                assert_eq!(context_id, ctx);
                assert!(!armed, "ooda off → disarmed");
            }
            other => panic!("expected SetOoda, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transport_ooda_resolves_track_from_attachment() {
        use kaijutsu_types::TrackId;

        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("bass"), None, principal);
        seed_track_and_attachment(&d, ctx, "bass", None);
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let mut cmds = spawn_beat_stub(rx, Ok(()));

        let result = d.dispatch(&[s("transport"), s("ooda"), s("on")], &c).await;
        assert!(result.is_ok(), "ooda on (no --track) failed: {}", result.message());
        match cmds.recv().await.expect("a SetOoda should be sent") {
            BeatCommand::SetOoda { track, context_id, armed } => {
                assert_eq!(track, TrackId::new("bass").unwrap());
                assert_eq!(context_id, ctx);
                assert!(armed);
            }
            other => panic!("expected SetOoda, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transport_rotate_every_sets_cadence() {
        use kaijutsu_types::TrackId;

        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let mut cmds = spawn_beat_stub(rx, Ok(()));

        let result = d
            .dispatch(
                &[
                    s("transport"),
                    s("rotate"),
                    s("--every"),
                    s("8"),
                    s("--track"),
                    s("bass"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "rotate --every failed: {}", result.message());
        match cmds.recv().await.expect("a SetRotate should be sent") {
            BeatCommand::SetRotate { track, context_id, every } => {
                assert_eq!(track, TrackId::new("bass").unwrap());
                assert_eq!(context_id, ctx);
                assert_eq!(every, Some(Cadence::new(8)));
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

        let result = d
            .dispatch(
                &[s("transport"), s("rotate"), s("off"), s("--track"), s("bass")],
                &c,
            )
            .await;
        assert!(result.is_ok(), "rotate off failed: {}", result.message());
        match cmds.recv().await.expect("a SetRotate should be sent") {
            BeatCommand::SetRotate { every, .. } => {
                assert_eq!(every, None, "off clears the cadence");
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

    // ── rotate rc end-to-end ──────────────────────────────────────────────────

    /// Run `body` to completion on a thread sized for rc lifecycles, joining it
    /// and re-raising any panic. Mirrors the server's beat-scheduler/SSH threads:
    /// a deep rc nest (kaish re-entered many levels) overflows the default 2 MiB
    /// test stack, so the work runs on [`crate::KAISH_RC_THREAD_STACK`] instead.
    fn run_on_rc_stack(body: impl FnOnce() + Send + 'static) {
        std::thread::Builder::new()
            .stack_size(crate::KAISH_RC_THREAD_STACK)
            .spawn(body)
            .expect("spawn rc-stack thread")
            .join()
            .expect("rc-stack thread panicked");
    }

    #[test]
    fn rotate_rc_forks_attaches_and_plays_the_child_on_the_parent_track() {
        // The page-turn nests rc deeply (rotate → `kj fork` → the child's
        // fork+attach rc → `kj transport attach`/`play`, each `kj` re-entering
        // kaish), which overflows the default 2 MiB test stack — exactly as it
        // would on the production beat-scheduler thread. Drive the body on the
        // same generous stack that thread now uses.
        run_on_rc_stack(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build current-thread runtime")
                .block_on(rotate_rc_body());
        });
    }

    /// Body of [`rotate_rc_forks_attaches_and_plays_the_child_on_the_parent_track`],
    /// extracted verbatim so the wrapper can run it on a deep stack.
    async fn rotate_rc_body() {
        // End-to-end page-turn: the `rotate` lifecycle (musician/rotate/S10-rotate.kai)
        // forks a spawn child, switches the rc shell to it, and attaches + plays it
        // on the PARENT's track. The child inherits the attachment (track + wakeup +
        // rotate cadence) via the fork-copy of the attachment row in
        // `insert_forked_context`. The child's `kj transport attach` re-announces
        // the inherited binding, so the song keeps turning at the same cadence
        // without any explicit SetRotate — the cadence travels with the fork.
        use crate::kernel_db::{PersistedAttachment, PersistedTrack};
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

        // Seed a track + attachment (with rotate cadence) so the fork-copy carries
        // the real values. This simulates what the scheduler would have persisted
        // after the original arm + rotate --every 4.
        d.kernel_db()
            .lock()
            .upsert_track(&PersistedTrack {
                track_id: "bass".to_string(),
                period_ms: 500,
                beats_per_phrase: 16,
                playhead_tick: None,
                playing: false,
                score_context_id: None,
                clock_kind: "system".to_string(),
            })
            .unwrap();
        d.kernel_db()
            .lock()
            .upsert_attachment(&PersistedAttachment {
                track_id: "bass".to_string(),
                context_id: parent,
                wakeup_every: 128,
                rotate_every_phrases: Some(4),
                ooda_armed: true,
            })
            .unwrap();

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        // The rc runs `kj transport attach && kj transport play`, each AWAITing the
        // scheduler ack; the stub acks Ok so the `&&` chain proceeds.
        let mut cmds = spawn_beat_stub(rx, Ok(()));

        // Fire the rotate lifecycle exactly as the beat scheduler's fire_rotate does.
        let caller = caller_with_context(parent);
        let vars = HashMap::new(); // rotate cadence is inherited via attachment, not via env
        d.run_rc_lifecycle_with_vars("rotate", parent, None, None, None, &vars, &caller)
            .await
            .expect("rotate lifecycle runs");

        // The page-turn emits two commands — all for the forked CHILD (the rc
        // `--switch`ed onto it), never the retired parent.
        let attach_cmd = cmds.recv().await.expect("rotate attaches the child");
        let (child, track) = match attach_cmd {
            BeatCommand::Attach { context_id, track, attachment, .. } => {
                assert_eq!(
                    attachment.rotate,
                    Some(Cadence::new(4)),
                    "child inherits the parent's rotate cadence via attachment fork-copy"
                );
                assert!(attachment.ooda_armed, "child inherits ooda_armed");
                (context_id, track)
            }
            other => panic!("expected Attach first, got {other:?}"),
        };
        assert_ne!(child, parent, "the child is a NEW context, not the parent");
        assert_eq!(track, TrackId::new("bass").unwrap(), "child keeps the parent's lane");

        match cmds.recv().await.expect("rotate plays the child's track") {
            BeatCommand::Play(id) => {
                assert_eq!(id, TrackId::new("bass").unwrap(), "the track's clock starts")
            }
            other => panic!("expected Play, got {other:?}"),
        }

        // The child really is a fork of the parent.
        let forked_from =
            d.kernel_db().lock().get_context(child).unwrap().unwrap().forked_from;
        assert_eq!(forked_from, Some(parent), "child is forked from the parent");
    }

    // ── misc error paths ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn transport_without_scheduler_errors_explicitly() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);
        // No beat ingress installed → no scheduler wired.

        // --track bypasses track resolution so we reach the "no scheduler" check.
        let result = d
            .dispatch(&[s("transport"), s("play"), s("--track"), s("bass")], &c)
            .await;
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

        let result = d
            .dispatch(
                &[s("transport"), s("tempo"), s("0"), s("--track"), s("bass")],
                &c,
            )
            .await;
        assert!(!result.is_ok(), "0 BPM must be rejected");
    }
}
