//! Transport subcommand: the musician's play/stop/pause/tempo control surface.
//!
//! The beat lives on the **track** now (`docs/tracks.md`, "The track entity"). A context
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

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use clap::{Parser, Subcommand};
use kaijutsu_types::{ContentType, ContextId, TrackId};

use super::effect::{Classify, Effect};
use super::format::{format_track_table, TrackListRow, TrackListState};
use super::refs;
use super::{KjCaller, KjDispatcher, KjResult};
use crate::hyoushigi::{Attachment, BeatCommand, BeatPolicy, Cadence, ClockKind};

/// The smallest rotate cadence in phrases. At 1 every phrase boundary is a
/// rotate horizon and the rotate replaces the tick, so the context never plays.
const MIN_ROTATE_PHRASES: u64 = 2;

fn rotate_cadence_refusal(verb: &str, every: u64) -> String {
    format!(
        "{verb}: a rotate cadence of {every} leaves no phrase to play: every phrase boundary \
         would rotate before the tick; use {MIN_ROTATE_PHRASES} or more (`kj transport rotate off` \
         clears the cadence)"
    )
}

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
    /// Attach a context to a track, so the track's beat wakes it. A new track is
    /// created stopped, with OODA armed (`kj transport play` starts the clock);
    /// an existing track keeps its clock and gains this attachment. Tempo and
    /// cadence come from the persisted rows when present, else from the musician
    /// defaults and a track named for the context's label; a label that names no
    /// valid track is refused. A track holds one live context per character: a
    /// second live context played by the same character is refused, so give each
    /// chair its own character (`kj character create drummer --model
    /// <backend>/<model> --rc musician`).
    Attach {
        /// Target context: . (default) | <label> | <hex prefix>
        #[arg(long)]
        context: Option<String>,
        /// Track (clock domain) to attach to. Omit to resolve from the context's
        /// persisted attachment, or (for a fresh first-attach) to derive from the
        /// context label.
        #[arg(long)]
        track: Option<String>,
        // The default comes from `Attachment::musician_default`
        // (hyoushigi/mod.rs); keep the number below in step with it.
        /// Wake this context every N beats on the track clock (wakeup
        /// divisor). Overrides any persisted value. Defaults to 32 beats.
        #[arg(long)]
        wakeup: Option<u64>,
        /// Rotate cadence in phrases, at least 2. Overrides any persisted value.
        /// Omit to inherit the persisted cadence (or no auto-rotation by default).
        #[arg(long)]
        rotate: Option<u64>,
    },
    /// Detach a context from a track. The track persists with its remaining
    /// attachments.
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
    /// Stop the track's clock. Rotation is suspended and remembered, not cleared;
    /// per-attachment OODA arm is untouched (`kj transport ooda off` disarms it).
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
    /// Switch the track's beat driver: `system` (local fixed-tempo timer) or
    /// `modeled` (phase-locked to an observed external MIDI clock). The current
    /// period carries over; a modeled clock free-runs at it until the first
    /// reference arrives.
    Clock {
        /// `system` or `modeled`
        kind: Option<String>,
        /// Target context (used for track lookup when --track is absent).
        #[arg(long)]
        context: Option<String>,
        /// Track whose clock to switch. Omit to resolve from the context's
        /// attachment.
        #[arg(long)]
        track: Option<String>,
    },
    /// Set or clear the rotate cadence (the page turn). At every phrase horizon
    /// where `phrase % N == 0`, the scheduler detaches this context and fires the
    /// `rotate` rc lifecycle, which replaces it with a successor (`kj context
    /// rotate`). A rotate horizon replaces that phrase's tick, so N must be at
    /// least 2.
    Rotate {
        /// Phrases per rotation, at least 2. Omit and pass `off` to disable.
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
    // Track deletion is a rename-aside tombstone (`docs/tracks.md`, "Transport").
    // Recovery is sqlite-only by decision: stop the kernel, then run
    // `UPDATE tracks SET track_id = '<name>', deleted_at = NULL WHERE track_id = '<name>~tombstone-<epoch-ms>'`.
    /// Delete a track (`--track <name>` is required). Stops the clock, detaches
    /// every attached context, and tombstones the track so `kj transport list`
    /// hides it. Leaves the score context untouched; attaching `--track <name>`
    /// again starts a new track with a new score.
    Delete {
        /// Track to delete (required; never resolved from the caller's attachment).
        #[arg(long)]
        track: String,
    },
    /// List every track with its live state (also what bare `kj transport` runs).
    /// Shows `dormant` for persisted tracks that have no live attachment in this
    /// session.
    List,
    /// Show pending work and the most recent 256 dispositions: each attempt's
    /// intended tick, estimate in ticks and milliseconds, the queue and compute
    /// time its producer measured, readiness, margin, basis, and outcome. The
    /// structured result carries the same statuses as JSON. This process-local
    /// history resets when the timeline is removed or the process restarts.
    Work {
        /// Track whose intended times, readiness, validity, and outcomes to read.
        #[arg(long)]
        track: String,
    },
    /// Repeat this context's last phrase at a future tick, with no model turn:
    /// `kj transport repeat --track bass --at 96`. At that tick's commitment the
    /// transport plays the latest phrase this context's performer committed on
    /// the track, or leaves silence when there is none. Needs the `drive`
    /// capability and an attachment to the track. `kj transport work` shows it
    /// as `requested: last good`; it is not open work (`KJ_OPEN_WORK`).
    Repeat {
        /// Track to repeat on; this context must be attached to it. Omit to use
        /// the context's attachment.
        #[arg(long)]
        track: Option<String>,
        /// Absolute track tick to repeat at, ahead of the track's playhead.
        #[arg(long)]
        at: i64,
    },
}

impl TransportCommand {
    /// The `--context` ref this verb targets (shared across all verbs).
    ///
    /// `Delete` has no `--context` — deletion is deliberately never resolved
    /// from the caller's attachment, so there is nothing here to feed the
    /// `resolve_context_arg(None, ..)` default-to-current-context fallback used
    /// by every other verb's track resolution.
    fn context(&self) -> Option<&str> {
        match self {
            TransportCommand::Attach { context, .. }
            | TransportCommand::Detach { context, .. }
            | TransportCommand::Play { context, .. }
            | TransportCommand::Pause { context, .. }
            | TransportCommand::Stop { context, .. }
            | TransportCommand::Tempo { context, .. }
            | TransportCommand::Ooda { context, .. }
            | TransportCommand::Clock { context, .. }
            | TransportCommand::Rotate { context, .. } => context.as_deref(),
            TransportCommand::Delete { .. }
            | TransportCommand::List
            | TransportCommand::Work { .. }
            | TransportCommand::Repeat { .. } => None,
        }
    }

    /// The `--track` override this verb carries (all verbs that name a track).
    /// `Delete`'s `track` is required (not `Option`), so it always yields `Some`.
    fn track_name(&self) -> Option<&str> {
        match self {
            TransportCommand::Attach { track, .. }
            | TransportCommand::Detach { track, .. }
            | TransportCommand::Play { track, .. }
            | TransportCommand::Pause { track, .. }
            | TransportCommand::Stop { track, .. }
            | TransportCommand::Tempo { track, .. }
            | TransportCommand::Ooda { track, .. }
            | TransportCommand::Clock { track, .. }
            | TransportCommand::Rotate { track, .. }
            | TransportCommand::Repeat { track, .. } => track.as_deref(),
            TransportCommand::Delete { track } | TransportCommand::Work { track } => Some(track.as_str()),
            TransportCommand::List => None,
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
            TransportCommand::Clock { .. } => "clock",
            TransportCommand::Rotate { .. } => "rotate",
            TransportCommand::Delete { .. } => "delete",
            TransportCommand::List => "list",
            TransportCommand::Work { .. } => "work",
            TransportCommand::Repeat { .. } => "repeat",
        }
    }
}

impl KjDispatcher {
    pub(crate) async fn dispatch_transport(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        // Bare `kj transport` answers "what tracks exist?" — the first thing
        // anyone tries (per feedback.md). The roster is a read, so it needs no
        // `transport` capability; the footer points at the full verb list.
        if argv.is_empty() {
            return self.transport_list(true).await;
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

        // `list` is a read-only observability surface — never gate it behind the
        // `transport` (write) capability. Short-circuit BEFORE the cap check so a
        // context that can't drive the beat can still see what's on it.
        if matches!(command, TransportCommand::List) {
            return self.transport_list(false).await;
        }
        if let TransportCommand::Work { track } = &command {
            let track = match TrackId::new(track) {
                Ok(track) => track,
                Err(e) => return KjResult::Err(format!("kj transport work: invalid track: {e}")),
            };
            let Some(timeline) = self.kernel().track_timeline(&track) else {
                return KjResult::Err(format!("kj transport work: track '{}' is not armed", track.as_str()));
            };
            let statuses = timeline.lock().statuses();
            let text = super::format::format_work_table(&statuses);
            let data = serde_json::to_value(statuses).expect("work status is serializable");
            return KjResult::ok_with_data(text, data);
        }
        // A repeat puts the caller's own performer's material on the score, as
        // `kj drive --score-at` does, so it takes `drive`, not `transport`.
        if let TransportCommand::Repeat { at, .. } = &command {
            return self.transport_repeat(command.track_name(), *at, caller);
        }

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

                TransportCommand::Clock { kind, .. } => {
                    let kind = match kind.as_deref() {
                        Some("system") => ClockKind::System,
                        Some("modeled") => ClockKind::Modeled,
                        _ => {
                            return KjResult::Err(
                                "kj transport clock: expected `system` or `modeled`".to_string(),
                            );
                        }
                    };
                    let track = match self.resolve_track_for_ctx(track_name, ctx) {
                        Ok(t) => t,
                        Err(e) => return KjResult::Err(e),
                    };
                    let tid = track.as_str().to_string();
                    let verb = format!(
                        "clock → {}",
                        if kind == ClockKind::Modeled { "modeled" } else { "system" }
                    );
                    (BeatCommand::SetClock { track, kind }, verb, Some(tid))
                }

                TransportCommand::Rotate { every, state, .. } => {
                    let every_cadence = match (every, state.as_deref()) {
                        // `off` clears the cadence.
                        (_, Some("off")) => None,
                        (Some(n), _) if *n >= MIN_ROTATE_PHRASES => Some(Cadence::new(*n)),
                        (Some(n), _) => {
                            return KjResult::Err(rotate_cadence_refusal("kj transport rotate", *n));
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

                TransportCommand::Delete { track } => {
                    // Deliberately bypasses `resolve_track_for_ctx` entirely —
                    // the whole point of this verb is that it NEVER infers a
                    // track from the caller's own attachment.
                    let track_id = match TrackId::new(track.as_str()) {
                        Ok(t) => t,
                        Err(e) => {
                            return KjResult::Err(format!(
                                "kj transport delete: invalid track {track:?}: {e}"
                            ));
                        }
                    };
                    let verb = format!("deleted track '{}'", track_id.as_str());
                    let tid = track_id.as_str().to_string();
                    (BeatCommand::Delete { track: track_id }, verb, Some(tid))
                }

                // Dispatched (and returned) above, before this match ever runs.
                // Kept only so the match stays exhaustive.
                TransportCommand::List | TransportCommand::Work { .. } | TransportCommand::Repeat { .. } => {
                    unreachable!("transport list, work and repeat are handled before the command match")
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
            // `detail` is `Some(..)` only for a command that carries extra
            // success info the caller couldn't otherwise learn — today,
            // `Delete`'s tombstone name (the scheduler computed it inside the
            // DB transaction that renamed the row; nowhere else knows it).
            Ok(Ok(detail)) => {
                let message = match &detail {
                    Some(tombstone) => format!("transport: {verb} (tombstone: '{tombstone}')"),
                    None => format!("transport: {} '{}'", verb, ctx.short()),
                };
                KjResult::Ok {
                    message,
                    content_type: ContentType::Plain,
                    ephemeral: false,
                    data: Some(serde_json::json!({
                        "context_id": ctx.to_hex(),
                        "track_id": track_id_for_data,
                        "action": action,
                        "tombstone": detail,
                    })),
                }
            }
            // The scheduler refused (e.g. not attached) — report the truth, loudly.
            Ok(Err(reason)) => KjResult::Err(format!("kj transport: {reason}")),
            // The scheduler dropped the reply without answering (it shut down
            // between send and reply) — don't claim success we can't confirm.
            Err(_) => KjResult::Err(
                "kj transport: the beat scheduler dropped the request without a reply".to_string(),
            ),
        }
    }

    /// Render `kj transport list`: the merged persisted+live roster of tracks.
    ///
    /// Two sources, unioned by track id:
    /// - **Persisted** (`db.list_tracks()`) — the durable, tombstone-filtered set.
    ///   This is what survives a restart, and it's the *only* source that sees a
    ///   `dormant` track (a real track in the DB that nothing has re-attached this
    ///   session, so the scheduler holds no live state for it).
    /// - **Live** (`request_track_snapshot()`) — the in-memory scheduler truth:
    ///   real playhead, live attachment count, whether the clock is actually
    ///   rolling. The persisted row's playhead lags (written on transitions), so
    ///   anything live must come from here.
    ///
    /// Live values win where a track appears in both; a persisted-only track is
    /// `dormant`; a live-only track (a db-less/embedded store never persists) is
    /// still shown. `.data` is the array of track-id strings so
    /// `for t in $(kj transport list)` round-trips into `--track $t`.
    ///
    /// `show_footer` appends the "run `kj transport --help` for all verbs"
    /// pointer — set only for the bare `kj transport` entry, kept off the
    /// explicit `kj transport list` so scripted output stays a clean table.
    async fn transport_list(&self, show_footer: bool) -> KjResult {
        // Persisted set first (canonical). A corrupt row fails loud rather than
        // rendering nonsense — same posture as `list_tracks` itself. Pull every
        // attachment row in the SAME lock scope (one query, not N+1 — one per
        // dormant track) so a dormant track's ATTACHED count reads at the same
        // instant as `list_tracks()` instead of risking disagreement with its
        // own row.
        let (persisted, attach_counts) = {
            let db = self.kernel_db().lock();
            let persisted = match db.list_tracks() {
                Ok(t) => t,
                Err(e) => return KjResult::Err(format!("kj transport list: reading tracks: {e}")),
            };
            let attachments = match db.list_all_attachments() {
                Ok(a) => a,
                Err(e) => {
                    return KjResult::Err(format!(
                        "kj transport list: reading attachments: {e}"
                    ));
                }
            };
            let mut attach_counts: HashMap<String, usize> = HashMap::new();
            for a in attachments {
                *attach_counts.entry(a.track_id).or_insert(0) += 1;
            }
            (persisted, attach_counts)
        };

        // Live scheduler snapshot (empty when no scheduler is wired — embedded/
        // test, or a cold kernel before anything re-attaches; that `None` case
        // legitimately means everything is dormant). A *wired* scheduler that
        // drops the reply is different — don't let `unwrap_or_default` fold it
        // into "nothing is live," which would render every live track
        // `dormant` and lie about it, contradicting the dormant-is-honest
        // design. Same posture as the command path's `ack_rx.await` match
        // above.
        let live = match self.kernel().request_track_snapshot() {
            Some(rx) => match rx.await {
                Ok(v) => v,
                Err(_) => {
                    return KjResult::Err(
                        "kj transport list: the beat scheduler dropped the snapshot request \
                         without a reply"
                            .to_string(),
                    );
                }
            },
            None => Vec::new(),
        };
        let live_by_id: HashMap<&str, &crate::hyoushigi::TrackSnapshot> =
            live.iter().map(|s| (s.id.as_str(), s)).collect();
        let persisted_by_id: HashMap<&str, &crate::kernel_db::PersistedTrack> =
            persisted.iter().map(|p| (p.track_id.as_str(), p)).collect();

        // Union of ids, sorted (BTreeSet) for deterministic output.
        let ids: BTreeSet<&str> = persisted
            .iter()
            .map(|p| p.track_id.as_str())
            .chain(live.iter().map(|s| s.id.as_str()))
            .collect();

        let mut rows = Vec::with_capacity(ids.len());
        let mut handles = Vec::with_capacity(ids.len());
        for id in ids {
            let live = live_by_id.get(id).copied();
            let pers = persisted_by_id.get(id).copied();

            let state = match live {
                Some(l) if l.playing => TrackListState::Playing,
                Some(_) => TrackListState::Stopped,
                None => TrackListState::Dormant,
            };
            // Effective period: live wins, else persisted. BPM is the integer
            // inverse (matching `tempo`'s `period = 60000/bpm`); 0 ms → 0 (never
            // divide by zero — a corrupt row would already have been rejected).
            let period_ms = live
                .map(|l| l.period.as_millis() as u64)
                .or_else(|| pers.map(|p| p.period_ms))
                .unwrap_or(0);
            let bpm = 60_000u64.checked_div(period_ms).unwrap_or(0);
            let beats_per_phrase = live
                .map(|l| l.beats_per_phrase)
                .or_else(|| pers.map(|p| p.beats_per_phrase))
                .unwrap_or(0);
            let clock_kind = live
                .map(|l| l.clock_kind.clone())
                .or_else(|| pers.map(|p| p.clock_kind.clone()))
                .unwrap_or_else(|| "system".to_string());
            // `None` distinguishes "never played" from "played to tick 0" —
            // `format_track_table` renders it `—`, same convention as SCORE.
            let playhead = live
                .map(|l| l.playhead)
                .or_else(|| pers.and_then(|p| p.playhead_tick));
            // Live attachment count is the scheduler's; a dormant track counts
            // its persisted attachment rows (they're what a re-attach restores)
            // — an O(1) lookup into the up-front map, not a per-row query.
            let attached = match live {
                Some(l) => l.attached.len(),
                None => attach_counts.get(id).copied().unwrap_or(0),
            };
            let score = live
                .map(|l| l.score_context)
                .or_else(|| pers.and_then(|p| p.score_context_id));
            let score_short = score.map(|c| c.short()).unwrap_or_else(|| "—".to_string());

            rows.push(TrackListRow {
                track_id: id.to_string(),
                state,
                clock_kind,
                bpm,
                beats_per_phrase,
                attached,
                playhead,
                score_short,
            });
            handles.push(serde_json::Value::String(id.to_string()));
        }

        let mut text = format_track_table(&rows);
        if show_footer {
            text.push_str("\n\n(run `kj transport --help` for all verbs)");
        }
        KjResult::ok_with_data(text, serde_json::Value::Array(handles))
    }

    /// `kj transport repeat`: place a requested fallback for the calling
    /// context's performer on a track that context is attached to
    /// ([`Timeline::request_fallback`](kaijutsu_hyoushigi::Timeline::request_fallback)).
    /// Runs no model and no resolver work.
    fn transport_repeat(&self, track_override: Option<&str>, at: i64, caller: &KjCaller) -> KjResult {
        if let Err(denied) = self.require_cap(caller, crate::mcp::Capability::Drive, "transport repeat") {
            return denied;
        }
        let Some(ctx) = caller.context_id else {
            return KjResult::Err(
                "kj transport repeat: no active context; a repeat plays the calling context's own phrase".to_string(),
            );
        };
        let track = match self.resolve_track_for_ctx(track_override, ctx) {
            Ok(t) => t,
            Err(e) => return KjResult::Err(e),
        };
        let performer = {
            let db = self.kernel_db().lock();
            let performer = match db.get_context(ctx) {
                Ok(row) => row.and_then(|row| row.performer),
                Err(e) => return KjResult::Err(format!("kj transport repeat: reading context {}: {e}", ctx.short())),
            };
            let Some(performer) = performer else {
                return KjResult::Err(format!(
                    "kj transport repeat: context {} has no performer, so it has no phrase to repeat",
                    ctx.short()
                ));
            };
            match db.get_attachment(track.as_str(), ctx) {
                Ok(Some(_)) => {}
                Ok(None) => return KjResult::Err(format!(
                    "kj transport repeat: context {} is not attached to track '{}'; a repeat plays on \
                     the caller's own track (`kj transport attach --track {}` first)",
                    ctx.short(), track.as_str(), track.as_str()
                )),
                Err(e) => return KjResult::Err(format!("kj transport repeat: reading attachment: {e}")),
            }
            performer
        };
        let Some(timeline) = self.kernel().track_timeline(&track) else {
            return KjResult::Err(format!(
                "kj transport repeat: track '{}' is not armed; nothing will play there",
                track.as_str()
            ));
        };
        let start = kaijutsu_types::Tick::new(at);
        let admitted = timeline.lock().request_fallback(
            kaijutsu_types::Span::instant(start), track.clone(), performer, kaijutsu_hyoushigi::Fallback::UseLastGood,
        );
        match admitted {
            Ok(work) => KjResult::ok_with_data(
                format!("transport: repeat at tick {at} on track '{}'", track.as_str()),
                serde_json::json!({
                    "context_id": ctx.to_hex(),
                    "track_id": track.as_str(),
                    "action": "repeat",
                    "start": at,
                    "work_id": work,
                }),
            ),
            Err(kaijutsu_hyoushigi::ScheduleError::InThePast { now, .. }) => KjResult::Err(format!(
                "kj transport repeat: tick {at} is not ahead of the playhead {}; nothing was placed. \
                 Pass a later --at",
                now.get()
            )),
            Err(e) => KjResult::Err(format!("kj transport repeat: nothing was placed: {e}")),
        }
    }

    /// Resolve the target [`TrackId`] for a clock-domain or per-attachment verb.
    ///
    /// If `track_override` is `Some`, parse it directly. Otherwise look up the
    /// context's persisted attachment set: exactly one row → use its `track_id`;
    /// zero rows → loud error (attach first); more than one → loud error
    /// (ambiguous). The stored track string that no longer
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
            if r < MIN_ROTATE_PHRASES {
                return Err(rotate_cadence_refusal("kj transport attach", r));
            }
            attachment.rotate = Some(Cadence::new(r));
        }

        Ok((track, attachment, policy))
    }
}

// Verb class: kj/effect.rs
impl Classify for TransportArgs {
    fn effect(&self) -> Effect {
        self.command.effect()
    }
}

impl Classify for TransportCommand {
    fn effect(&self) -> Effect {
        match self {
            // `list` is the roster read (`readonly.rs`'s own doc excludes it
            // from the module's table only because `kj transport` was kept
            // out wholesale, not because it mutates — `kj/effect.rs`
            // makes it Read on its own merits).
            TransportCommand::List | TransportCommand::Work { .. } => Effect::Read,
            TransportCommand::Attach { .. }
            | TransportCommand::Detach { .. }
            | TransportCommand::Play { .. }
            | TransportCommand::Pause { .. }
            | TransportCommand::Stop { .. }
            | TransportCommand::Tempo { .. }
            | TransportCommand::Ooda { .. }
            | TransportCommand::Clock { .. }
            | TransportCommand::Rotate { .. }
            | TransportCommand::Repeat { .. }
            | TransportCommand::Delete { .. } => Effect::Write,
        }
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
                    // Fire-and-forget; the stub has no clocks to slave.
                    BeatRequest::ClockEstimate { .. } => {}
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
        let mut cmds = spawn_beat_stub(rx, Ok(None));

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
        let mut cmds = spawn_beat_stub(rx, Ok(None));

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
        let mut cmds = spawn_beat_stub(rx, Ok(None));

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
        let mut cmds = spawn_beat_stub(rx, Ok(None));

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
        let mut cmds = spawn_beat_stub(rx, Ok(None));

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
        // derived from the label. Attaching is the opt-in, so any context with a sluggable label gets a beat.
        use kaijutsu_types::TrackId;

        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("Lead Synth"), None, principal);
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let mut cmds = spawn_beat_stub(rx, Ok(None));

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
        let mut cmds = spawn_beat_stub(rx, Ok(None));

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
        let mut cmds = spawn_beat_stub(rx, Ok(None));

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
        let mut cmds = spawn_beat_stub(rx, Ok(None));

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
    async fn transport_attach_uses_the_persisted_row() {
        // A context keeps (and uses) its persisted attachment row — attaching is an explicit opt-in, so the last-known policy/lane is
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
        let mut cmds = spawn_beat_stub(rx, Ok(None));

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
        let mut cmds = spawn_beat_stub(rx, Ok(None));

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
        let mut cmds = spawn_beat_stub(rx, Ok(None));

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
        let mut cmds = spawn_beat_stub(rx, Ok(None));

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
        let mut cmds = spawn_beat_stub(rx, Ok(None));

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
        let mut cmds = spawn_beat_stub(rx, Ok(None));

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
        let mut cmds = spawn_beat_stub(rx, Ok(None));

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

    /// A cadence of 1 makes every phrase boundary a rotate horizon, which
    /// replaces the tick: the context would never play.
    #[tokio::test]
    async fn a_rotate_cadence_of_one_is_refused_with_its_consequence() {
        let d = test_dispatcher().await;
        let ctx = register_context(&d, Some("c"), None, PrincipalId::new());
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let mut cmds = spawn_beat_stub(rx, Ok(None));

        for argv in [
            vec![s("transport"), s("rotate"), s("--every"), s("1"), s("--track"), s("bass")],
            vec![s("transport"), s("rotate"), s("--every"), s("0"), s("--track"), s("bass")],
            vec![s("transport"), s("attach"), s("--rotate"), s("1"), s("--track"), s("bass")],
        ] {
            let result = d.dispatch(&argv, &c).await;
            assert!(!result.is_ok(), "{argv:?} must be refused");
            assert!(
                result.message().contains("no phrase to play") && result.message().contains("use 2 or more"),
                "the refusal states the consequence and the fix: {}",
                result.message()
            );
        }
        assert!(cmds.try_recv().is_err(), "no command reaches the scheduler");

        let ok = d
            .dispatch(&[s("transport"), s("rotate"), s("--every"), s("2"), s("--track"), s("bass")], &c)
            .await;
        assert!(ok.is_ok(), "2 is the smallest cadence: {}", ok.message());
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

    // ── list ──────────────────────────────────────────────────────────────────

    /// Beat stub that answers `Snapshot` with a caller-supplied set of live
    /// [`TrackSnapshot`]s (the running scheduler's truth). Commands are still
    /// forwarded + acked, same as [`spawn_beat_stub`].
    fn spawn_beat_stub_snap(
        mut ingress: tokio::sync::mpsc::UnboundedReceiver<BeatRequest>,
        ack: BeatAck,
        snapshots: Vec<crate::hyoushigi::TrackSnapshot>,
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
                    BeatRequest::Snapshot { reply } => {
                        let _ = reply.send(snapshots.clone());
                    }
                    BeatRequest::CommitCapture { reply, .. } => {
                        let _ = reply.send(Err("beat stub: no tracks".into()));
                    }
                    BeatRequest::ClockEstimate { .. } => {}
                }
            }
        });
        cmd_rx
    }

    #[tokio::test]
    async fn transport_list_empty_when_no_tracks() {
        // No persisted tracks, no scheduler wired → a definitive "(no tracks)",
        // not a scavenger hunt. This is the answer DeepSeek couldn't get.
        let d = test_dispatcher().await;
        let ctx = register_context(&d, Some("c"), None, PrincipalId::new());
        let c = caller_with_context(ctx);
        let result = d.dispatch(&[s("transport"), s("list")], &c).await;
        assert!(result.is_ok(), "list failed: {}", result.message());
        assert!(
            result.message().contains("(no tracks)"),
            "empty roster is explicit: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn transport_list_shows_dormant_persisted_track() {
        // The cold-kernel gap: a track in the DB but not re-attached this session
        // has NO live snapshot. It must still appear — marked `dormant` — with its
        // persisted tempo/phrase and attachment count. (Live snapshot is empty.)
        let d = test_dispatcher().await;
        let ctx = register_context(&d, Some("bass"), None, PrincipalId::new());
        seed_track_and_attachment(&d, ctx, "bass", None); // 500 ms → 120 BPM, phrase 16, 1 attach
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let _cmds = spawn_beat_stub(rx, Ok(None)); // Snapshot → empty

        let result = d.dispatch(&[s("transport"), s("list")], &c).await;
        assert!(result.is_ok(), "list failed: {}", result.message());
        let m = result.message();
        assert!(m.contains("bass"), "persisted track appears: {m}");
        assert!(m.contains("dormant"), "not-live persisted track is `dormant`: {m}");
        assert!(m.contains("120"), "persisted BPM (500 ms → 120): {m}");
        // Columns: TRACK STATE CLOCK BPM PHRASE ATTACHED PLAYHEAD SCORE
        let row = m.lines().find(|l| l.contains("bass")).expect("a bass row");
        let cols: Vec<&str> = row.split_whitespace().collect();
        assert_eq!(cols[5], "1", "dormant track counts its persisted attachment: {row}");
    }

    #[tokio::test]
    async fn transport_list_shows_live_playing_track() {
        // A track live in the scheduler renders `playing` with the scheduler's
        // real playhead — even when it has no persisted row (embedded/db-less).
        use crate::hyoushigi::TrackSnapshot;
        use kaijutsu_types::{ContextId, TrackId};
        let d = test_dispatcher().await;
        let ctx = register_context(&d, Some("lead"), None, PrincipalId::new());
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let snap = TrackSnapshot {
            id: TrackId::new("lead").unwrap(),
            score_context: ContextId::new(),
            playing: true,
            playhead: 384,
            period: Duration::from_millis(500),
            beats_per_phrase: 32,
            beat_count: 12,
            last_epoch_ns: 0,
            clock_kind: "system".to_string(),
            attached: vec![ctx],
        };
        let _cmds = spawn_beat_stub_snap(rx, Ok(None), vec![snap]);

        let result = d.dispatch(&[s("transport"), s("list")], &c).await;
        assert!(result.is_ok(), "list failed: {}", result.message());
        let m = result.message();
        assert!(m.contains("lead"), "live-only track appears: {m}");
        assert!(m.contains("playing"), "live + playing → `playing`: {m}");
        assert!(m.contains("384"), "shows the live playhead: {m}");
    }

    #[tokio::test]
    async fn transport_list_data_is_track_id_array() {
        // `.data` is the array of track-id strings, so `for t in $(kj transport
        // list); do kj transport play --track $t; done` round-trips.
        let d = test_dispatcher().await;
        let ctx = register_context(&d, Some("bass"), None, PrincipalId::new());
        seed_track_and_attachment(&d, ctx, "bass", None);
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let _cmds = spawn_beat_stub(rx, Ok(None));

        let result = d.dispatch(&[s("transport"), s("list")], &c).await;
        match result {
            crate::kj::KjResult::Ok { data: Some(serde_json::Value::Array(ids)), .. } => {
                assert_eq!(
                    ids,
                    vec![serde_json::Value::String("bass".to_string())],
                    "data carries the track ids for iteration"
                );
            }
            other => panic!("expected Ok with array data, got {other:?}"),
        }
    }

    /// Beat stub whose scheduler IS wired (commands ack normally) but that
    /// silently DROPS the `Snapshot` reply sender instead of answering —
    /// standing in for a scheduler that shut down between the request and
    /// the reply. Distinct from `None` (no ingress at all), which
    /// legitimately means "everything dormant."
    fn spawn_beat_stub_snapshot_drops_reply(
        mut ingress: tokio::sync::mpsc::UnboundedReceiver<BeatRequest>,
    ) -> tokio::sync::mpsc::UnboundedReceiver<BeatCommand> {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(req) = ingress.recv().await {
                match req {
                    BeatRequest::Command { command, reply } => {
                        let _ = cmd_tx.send(command);
                        if let Some(reply) = reply {
                            let _ = reply.send(Ok(None));
                        }
                    }
                    // The bug under test: never send — the receiver sees the
                    // sender drop, i.e. `rx.await` resolves to `Err`.
                    BeatRequest::Snapshot { reply } => drop(reply),
                    BeatRequest::CommitCapture { reply, .. } => {
                        let _ = reply.send(Err("beat stub: no tracks".into()));
                    }
                    BeatRequest::ClockEstimate { .. } => {}
                }
            }
        });
        cmd_rx
    }

    #[tokio::test]
    async fn transport_list_errors_when_scheduler_drops_snapshot_reply() {
        // The scheduler IS wired (unlike `transport_list_empty_when_no_tracks`'s
        // `None` case, which legitimately means "everything dormant") but
        // drops the `Snapshot` reply. `kj transport list` must report that
        // loudly rather than folding it into an empty live set and rendering
        // every track `dormant` — a lie the dormant-is-honest design forbids.
        let d = test_dispatcher().await;
        let ctx = register_context(&d, Some("bass"), None, PrincipalId::new());
        seed_track_and_attachment(&d, ctx, "bass", None);
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let _cmds = spawn_beat_stub_snapshot_drops_reply(rx);

        let result = d.dispatch(&[s("transport"), s("list")], &c).await;
        assert!(
            matches!(result, crate::kj::KjResult::Err(_)),
            "a dropped Snapshot reply must surface as an error, not a lying dormant roster: {:?}",
            result
        );
        assert!(
            result.message().to_lowercase().contains("scheduler"),
            "the error names the scheduler: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn bare_transport_lists_with_help_footer_ungated() {
        // Bare `kj transport` lists (DeepSeek's first instinct) AND appends the
        // help footer. The caller is non-privileged with no `transport` binding —
        // proving the roster is a read that needs no capability.
        let d = test_dispatcher().await;
        let ctx = register_context(&d, Some("bass"), None, PrincipalId::new());
        seed_track_and_attachment(&d, ctx, "bass", None);
        let c = caller_with_context(ctx); // non-privileged, no cap
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let _cmds = spawn_beat_stub(rx, Ok(None));

        let result = d.dispatch(&[s("transport")], &c).await;
        assert!(
            result.is_ok(),
            "bare transport must list (not error/deny): {}",
            result.message()
        );
        let m = result.message();
        assert!(m.contains("bass"), "bare transport lists tracks: {m}");
        assert!(m.contains("--help"), "bare transport shows the help footer: {m}");
    }

    #[tokio::test]
    async fn explicit_transport_list_has_no_footer() {
        // The explicit verb is a clean table — no footer — so scripted callers
        // get pure data. (Discovery footer is only for the bare entry.)
        let d = test_dispatcher().await;
        let ctx = register_context(&d, Some("bass"), None, PrincipalId::new());
        seed_track_and_attachment(&d, ctx, "bass", None);
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let _cmds = spawn_beat_stub(rx, Ok(None));

        let result = d.dispatch(&[s("transport"), s("list")], &c).await;
        assert!(result.is_ok(), "list failed: {}", result.message());
        assert!(
            !result.message().contains("--help"),
            "explicit list has no footer: {}",
            result.message()
        );
    }

    // ── repeat ───────────────────────────────────────────────────────────────

    /// A context played by a fresh performer, attached to an armed track
    /// `band` whose playhead is at 10, with a loadout of `house` plus `caps`.
    fn repeat_fixture(
        d: &super::super::KjDispatcher,
        label: &str,
        caps: &[crate::mcp::Capability],
    ) -> (kaijutsu_types::ContextId, PrincipalId, crate::hyoushigi::SharedTimeline) {
        let performer = PrincipalId::new();
        let ctx = register_context(d, Some(label), None, performer);
        d.kernel_db().lock().update_performer(ctx, Some(performer)).unwrap();
        seed_track_and_attachment(d, ctx, "band", None);
        let mut binding = crate::mcp::ContextToolBinding::new();
        binding.grant(crate::mcp::Capability::House);
        for cap in caps {
            binding.grant(cap.clone());
        }
        d.kernel_db().lock().upsert_context_binding(ctx, &binding).unwrap();
        let timeline = d.kernel().arm_track_timeline(
            kaijutsu_types::TrackId::new("band").unwrap(),
            kaijutsu_hyoushigi::TickClock::default(),
            kaijutsu_types::Tick::new(10),
        );
        (ctx, performer, timeline)
    }

    /// With `drive`, a player places a repeat of its own last phrase on the
    /// track it is attached to: an item for its performer at that tick, with
    /// no settlement yet and no open work. `--track` defaults to the
    /// context's attachment.
    #[tokio::test]
    async fn transport_repeat_places_a_requested_repeat_for_the_callers_performer() {
        let d = test_dispatcher().await;
        let (ctx, performer, timeline) = repeat_fixture(&d, "bass", &[crate::mcp::Capability::Drive]);
        let c = caller_with_context(ctx);
        for args in [
            vec![s("transport"), s("repeat"), s("--track"), s("band"), s("--at"), s("20")],
            vec![s("transport"), s("repeat"), s("--at"), s("24")],
        ] {
            let result = d.dispatch(&args, &c).await;
            assert!(result.is_ok(), "{args:?}: {}", result.message());
        }
        let statuses = timeline.lock().statuses();
        let starts: Vec<i64> = statuses.iter().map(|s| s.start.get()).collect();
        assert_eq!(starts, [20, 24]);
        for status in &statuses {
            assert_eq!(status.played_by, performer);
            assert_eq!(status.disposition, None);
        }
        assert_eq!(timeline.lock().open_work(performer), 0, "a repeat is not open work");
    }

    /// Each refusal says why: no `drive`, a track the context is not
    /// attached to, a tick not ahead of the playhead, an unarmed track, and a
    /// context with no performer. A refusal places nothing.
    #[tokio::test]
    async fn transport_repeat_refusals_say_why() {
        let d = test_dispatcher().await;
        let (ctx, _performer, timeline) = repeat_fixture(&d, "bass", &[crate::mcp::Capability::Drive]);
        let c = caller_with_context(ctx);
        d.kernel().arm_track_timeline(
            kaijutsu_types::TrackId::new("other").unwrap(),
            kaijutsu_hyoushigi::TickClock::default(),
            kaijutsu_types::Tick::ZERO,
        );
        for (args, expected) in [
            (vec!["--track", "other", "--at", "20"], "not attached to track 'other'"),
            (vec!["--track", "band", "--at", "10"], "not ahead of the playhead 10"),
            (vec!["--track", "band", "--at", "3"], "not ahead of the playhead 10"),
        ] {
            let mut argv = vec![s("transport"), s("repeat")];
            argv.extend(args.iter().map(|a| s(a)));
            let result = d.dispatch(&argv, &c).await;
            assert!(!result.is_ok(), "{argv:?} must be refused");
            assert!(result.message().contains(expected), "{argv:?}: {}", result.message());
        }

        seed_track_and_attachment(&d, ctx, "unarmed", None);
        let result = d.dispatch(&[s("transport"), s("repeat"), s("--track"), s("unarmed"), s("--at"), s("20")], &c).await;
        assert!(result.message().contains("track 'unarmed' is not armed"), "{}", result.message());

        let (no_drive, _, _) = repeat_fixture(&d, "drums", &[crate::mcp::Capability::Transport]);
        let result = d.dispatch(&[s("transport"), s("repeat"), s("--track"), s("band"), s("--at"), s("20")],
            &caller_with_context(no_drive)).await;
        assert!(result.message().contains("denied") && result.message().contains("'drive'"), "{}", result.message());

        let probe = register_context(&d, Some("probe"), None, PrincipalId::new());
        seed_track_and_attachment(&d, probe, "band", None);
        let result = d.dispatch(&[s("transport"), s("repeat"), s("--track"), s("band"), s("--at"), s("20")],
            &caller_with_context(probe)).await;
        assert!(result.message().contains("has no performer"), "{}", result.message());

        assert!(timeline.lock().statuses().is_empty(), "a refusal places nothing");
    }

    #[tokio::test]
    async fn transport_repeat_help() {
        let d = test_dispatcher().await;
        let ctx = register_context(&d, Some("here"), None, PrincipalId::new());
        let result = d.dispatch(&[s("transport"), s("repeat"), s("--help")], &caller_with_context(ctx)).await;
        assert!(result.is_ok(), "{}", result.message());
        eprintln!("{}", result.message());
        for flag in ["--track", "--at"] {
            assert!(result.message().contains(flag), "{}", result.message());
        }
    }

    // ── delete ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn transport_delete_requires_track_flag() {
        // The one hard gate this verb has: no implicit resolution from the
        // caller's attachment, ever — bare `kj transport delete` (even with a
        // real attachment seeded) must refuse before it ever reaches the
        // scheduler, not silently target whatever track the caller happens to
        // be on.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("bass"), None, principal);
        seed_track_and_attachment(&d, ctx, "bass", None);
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let mut cmds = spawn_beat_stub(rx, Ok(None));

        let result = d.dispatch(&[s("transport"), s("delete")], &c).await;
        assert!(!result.is_ok(), "delete with no --track must error");
        assert!(
            cmds.try_recv().is_err(),
            "no Delete command reaches the scheduler without an explicit --track"
        );
    }

    #[tokio::test]
    async fn transport_delete_sends_delete_command() {
        use kaijutsu_types::TrackId;

        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        // The caller isn't even attached to "bass" — proves delete never
        // consults the caller's own attachment, only the explicit --track.
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let mut cmds = spawn_beat_stub(rx, Ok(Some("bass~tombstone-1234".to_string())));

        let result = d
            .dispatch(&[s("transport"), s("delete"), s("--track"), s("bass")], &c)
            .await;
        assert!(result.is_ok(), "delete failed: {}", result.message());
        match cmds.recv().await.expect("a Delete should be sent") {
            BeatCommand::Delete { track } => {
                assert_eq!(track, TrackId::new("bass").unwrap());
            }
            other => panic!("expected Delete, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transport_delete_reports_the_tombstone_name_on_success() {
        // The scheduler is the only place that knows the tombstone name (it
        // computed it inside the DB transaction that renamed the row); `kj
        // transport` must relay it, not recompute or guess at it.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let _cmds = spawn_beat_stub(rx, Ok(Some("bass~tombstone-1752607200000".to_string())));

        let result = d
            .dispatch(&[s("transport"), s("delete"), s("--track"), s("bass")], &c)
            .await;
        assert!(result.is_ok(), "delete failed: {}", result.message());
        assert!(
            result.message().contains("bass~tombstone-1752607200000"),
            "the report carries the tombstone name: {}",
            result.message()
        );
        let crate::kj::KjResult::Ok { data: Some(data), .. } = result else {
            panic!("delete must report structured data");
        };
        assert_eq!(
            data.get("tombstone").and_then(|v| v.as_str()),
            Some("bass~tombstone-1752607200000"),
            "the tombstone name rides the data payload too: {data:?}"
        );
        assert_eq!(data.get("action").and_then(|v| v.as_str()), Some("delete"));
    }

    #[tokio::test]
    async fn transport_delete_surfaces_scheduler_refusal() {
        // An unknown track refuses at the scheduler (BeatScheduler::delete) —
        // `kj transport delete` must report that refusal, never a blind success.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let _cmds = spawn_beat_stub(
            rx,
            Err("track 'bass' has no clock — nothing to delete".to_string()),
        );

        let result = d
            .dispatch(&[s("transport"), s("delete"), s("--track"), s("bass")], &c)
            .await;
        assert!(
            matches!(result, crate::kj::KjResult::Err(_)),
            "a scheduler refusal must surface as an error, not blind success"
        );
        assert!(
            result.message().contains("nothing to delete"),
            "the error carries the scheduler's reason: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn transport_delete_rejects_invalid_track_name() {
        // `~` (the tombstone separator) is outside the track-id charset — this
        // is the enforcement point `kernel_db::tombstone_track`'s doc cites:
        // `TrackId::new` refuses it here, so a tombstoned name can never be
        // targeted by `kj transport delete` (or `attach`) in the first place.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        d.kernel().set_beat_ingress(tx);
        let mut cmds = spawn_beat_stub(rx, Ok(None));

        let result = d
            .dispatch(
                &[s("transport"), s("delete"), s("--track"), s("bass~tombstone-1")],
                &c,
            )
            .await;
        assert!(!result.is_ok(), "an invalid track name must be refused");
        assert!(cmds.try_recv().is_err(), "no Delete command sent on refusal");
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
