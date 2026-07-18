//! The DJ thread — musical dispatch off the frame (`docs/midi.md` "The DJ
//! thread"). A dedicated `std::thread` ("kaijutsu-dj") running a
//! current-thread tokio runtime, one `select!` over {a Bevy control channel,
//! the actor's own event broadcast, its connection-status watch, the
//! click-horizon timer}. It holds its *own* `broadcast::Receiver` off the
//! `ActorHandle` — an independent cursor, so a stalled UI drain
//! (`poll_server_events`) costs the DJ nothing, closing the frame-jitter
//! bug this arc exists to fix.
//!
//! **Keep the thread thin.** Every `select!` arm below does exactly one
//! thing: translate channel traffic into a [`DjCore`] call, then translate
//! `DjCore`'s report into a sink dispatch and/or a [`DjEffect`] for
//! telemetry. The translation itself lives in small, pure, unit-testable
//! functions ([`handle_server_event`], [`handle_due_clicks`],
//! [`handle_status_change`]) — any decision logic beyond "what does this
//! channel message mean to the clock" belongs in `core.rs`, not here.
//!
//! **Slice #3** was the first LIVE wiring: every `audio/*`, `CLIP_MIME`, and
//! `PREPARE_MIME` `RenderCue` dispatches through
//! [`super::audio::dispatch_render_cue`] / [`super::audio::handle_prefetch_outcome`]
//! (ported from the deleted `audio.rs`) — the events arm calls the former for
//! every `RenderCue` alongside (not instead of) [`handle_server_event`]'s
//! clock reaction to the same cue, and a prefetch-outcome `select!` arm calls
//! the latter. [`DjSinks::audio`] is the real [`AudioSchedulerHandle`],
//! spawned in [`DjPlugin::build`] (moved from the deleted `AudioOutPlugin`).
//!
//! **Task #4 (the demolition) is this revision.** ABC→MIDI dispatch (events
//! arm → [`super::midi::dispatch_midi_cue`]), the ALSA sink + patch-bay
//! auto-connect (a loop-local [`super::midi::MidiSink`], generic parameter
//! `M: MidiDispatch` — see that module's doc for why loop-local rather than a
//! `DjSinks` field), and the click policy (already ported to [`DjCore`] in
//! Task #1) all now live on this thread end to end. `midi.rs` and
//! `metronome.rs` are DELETED — nothing outside this thread touches MIDI or
//! the metronome anymore. The old `ClickSink` seam (click-only) is replaced
//! by the broader [`super::midi::MidiDispatch`] (click + ABC-schedule +
//! flush + traffic + auto-connect), reflecting that the events arm now
//! dispatches through the same owned sink the click timer does — "one app,
//! one port."
//!
//! [`DjPlugin`] is registered in `main.rs`, replacing `AudioOutPlugin`,
//! `MidiOutPlugin`, and `MetronomePlugin`.
#![allow(dead_code)]

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bevy::prelude::*;
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{debug, warn};

use kaijutsu_audio::{Slew, RENDER_FLUSH_MIME};
use kaijutsu_client::{ActorHandle, ConnectionStatus, ServerEvent, SshConfig};

use crate::audio_sched::AudioSchedulerHandle;
use crate::connection::actor_plugin::{RpcActor, RpcConnectionState, RpcResultMessage};

use super::audio::{dispatch_render_cue, handle_prefetch_outcome};
use super::core::{ClockTransition, DjCore, MetronomeConfig};
use super::midi::{AUTOCONNECT_POLL, MidiDispatch, MidiSink, dispatch_midi_cue};
use super::prefetch::{CasPrefetch, PrefetchOutcome};

/// How far ahead the click timer pre-schedules — mirrors
/// `metronome.rs::SCHEDULE_HORIZON` verbatim (same value, same reasoning:
/// comfortably above the app's frame interval, comfortably under one beat).
/// Duplicated rather than imported: `metronome.rs`'s constant is private and
/// the metronome keeps its own copy of the click policy until Task #4 folds
/// click dispatch into this thread and deletes it (`core.rs`'s module doc).
const SCHEDULE_HORIZON: Duration = Duration::from_millis(250);

/// The click timer's sleep target while [`DjCore::next_wake`] returns `None`
/// (no phasor running). Long enough the timer arm effectively never fires on
/// its own in that state — a fresh `ActorReady`/event/status change always
/// wakes the `select!` immediately via its own arm regardless; this just
/// avoids spinning the timer arm for no reason.
const NO_PHASOR_SLEEP: Duration = Duration::from_secs(3600);

// ── DjPulse — the DJ→Bevy mirror channel ───────────────────────────────────

/// One pulse mirrored DJ→Bevy — decorative back-flow only (`docs/midi.md`:
/// "Back-flow from the DJ is one small crossbeam channel (patch-bay traffic
/// pulses)"). [`drain_dj_pulses`] drains the channel every frame and folds
/// each pulse into the Bevy-side [`RenderPortTraffic`] message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DjPulse {
    /// Something left the render port — an ABC cue (events arm, via
    /// [`dispatch_midi_cue`]) or a metronome click (the click-timer arm, via
    /// [`handle_due_clicks`]). Sent at most once per select!-iteration burst
    /// that touched the sink (`MidiSink::take_traffic`'s drain point in each
    /// arm below) — the same "one travelling packet, not a strobe" collapse
    /// `midi.rs`'s deleted per-Bevy-frame `emit_render_traffic` used, just at
    /// select!-iteration granularity instead of frame granularity (finer, but
    /// same rationale: a burst of clicks scheduled in one `due_clicks` call,
    /// or one ABC cue's worth of scheduling, is one pulse, not N).
    RenderTraffic,
}

// ── Sinks ────────────────────────────────────────────────────────────────

/// External effects the DJ thread can drive, beyond its own generic `M:
/// MidiDispatch` sink (a separate `run_loop` parameter — see `dj::midi`'s
/// module doc for why MIDI isn't a field here). `DjSinks::default()` is what
/// the subscription-swap/shutdown tests use; every call site that reads
/// `audio` already treats `None` as "compute the decision, skip the
/// dispatch," never as an error: a headless DJ (no audio device) is a fully
/// correct DJ, just a silent one.
#[derive(Default)]
pub(crate) struct DjSinks {
    /// The rodio scheduler thread's handle (`audio_sched.rs`) — the real
    /// thing in production ([`DjPlugin::build`] spawns it, moved from the
    /// deleted `AudioOutPlugin`). `None` in tests that don't care about
    /// scheduler dispatch.
    pub(crate) audio: Option<AudioSchedulerHandle>,
}

// ── ActorSource — the testable seam around `ActorHandle` ───────────────────

/// What [`DjCtl::ActorReady`] needs from its `handle` to mint the DJ
/// thread's own subscriptions — an independent cursor, never shared with the
/// UI's `poll_server_events`/`poll_connection_status` (`docs/midi.md`: "a
/// stalled UI drain costs it nothing"). Implemented for [`ActorHandle`] in
/// production; the test module implements it over a hand-built
/// `broadcast`/`watch` pair so [`run_loop`]'s subscription-swap and shutdown
/// paths run for real without a live SSH actor — constructing a real
/// `ActorHandle` needs a running `RpcActor` task whose `event_tx`/`status_tx`
/// are private, so nothing outside `kaijutsu-client` can inject a synthetic
/// [`ServerEvent`] into one.
pub(crate) trait ActorSource: Send + 'static {
    fn subscribe_events(&self) -> broadcast::Receiver<ServerEvent>;
    fn watch_status(&self) -> watch::Receiver<ConnectionStatus>;
    /// The status LEVEL at subscription time — the seed. `watch::subscribe`
    /// marks the current value as seen, so `changed()` never fires for it: a
    /// DJ resubscribing to an actor that already reached `Connected` (the
    /// fast-local-handshake race `poll_bootstrap_results` documents) would
    /// otherwise sit `connected = false` — dropping every CAS cue with a
    /// "no live connection" warn — until the next real transition. Same
    /// remedy as `poll_connection_status`'s `current_status()` seed.
    fn current_status(&self) -> ConnectionStatus;
}

impl ActorSource for ActorHandle {
    fn subscribe_events(&self) -> broadcast::Receiver<ServerEvent> {
        ActorHandle::subscribe_events(self)
    }
    fn watch_status(&self) -> watch::Receiver<ConnectionStatus> {
        ActorHandle::watch_status(self)
    }
    fn current_status(&self) -> ConnectionStatus {
        ActorHandle::current_status(self)
    }
}

// ── DjCtl — Bevy → DJ-thread control channel ────────────────────────────────

/// Bevy → DJ-thread control messages (`docs/midi.md` "The DJ thread": "a Bevy
/// control channel (actor generation / metronome config / shutdown)").
/// Generic over `H` (defaulting to [`ActorHandle`]) purely so [`run_loop`]
/// is callable from a test with a hand-built double — production code only
/// ever names the bare `DjCtl` (`H = ActorHandle`).
pub enum DjCtl<H = ActorHandle> {
    /// A fresh (or respawned) actor is ready — drop any previous
    /// subscription and `subscribe_events()` + `watch_status()` on this one.
    /// `ssh_config` rides along unused until Task #3's CAS prefetch (kept now
    /// so this shape doesn't churn between tasks); `generation` likewise, for
    /// log/telemetry correlation.
    ActorReady { handle: H, ssh_config: SshConfig, generation: u64 },
    /// A freshly parsed per-client `metronome.toml` — applied verbatim to
    /// [`DjCore`]'s click config.
    MetronomeConfig(MetronomeConfig),
    /// Exit the `select!` loop cleanly. The ctl channel closing (every
    /// sender dropped) has the same effect — see [`run_loop`]'s ctl arm.
    Shutdown,
}

// ── DjEffect — what a translation function hands back for the loop to record

/// One thing the `select!` loop should record once `DjCore` reports it — kept
/// as data (not executed inline) so the translation functions below are
/// unit-testable without a tokio runtime or a live telemetry provider,
/// mirroring `DjCore`'s own "pure report, caller executes" stance
/// ([`core::BeatObservation`](super::core::BeatObservation)/
/// [`core::DueClicks`](super::core::DueClicks)).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum DjEffect {
    /// `kaijutsu.dj.clock_transition` (`to`/`reason` from [`ClockTransition`]).
    Transition(ClockTransition),
    /// `kaijutsu.dj.phasor_slew`, consumer `"dj"` — `time_well` is the other
    /// consumer (`view/time_well/live.rs`); `metronome` (the deleted
    /// `metronome.rs`'s own consumer label) no longer reports — the DJ is now
    /// the sole clicker, so `"dj"` is the intended replacement, not a gap.
    Slew(Slew),
    /// `kaijutsu.metronome.click` — one per click actually dispatched to a
    /// live sink (ported from the deleted `metronome.rs::click_on_beat`'s
    /// per-offset counter; a global counter, no consumer label to carry).
    Click,
}

/// Production `record_effect`: forward straight to `kaijutsu_telemetry`'s
/// global recorders (fire-and-forget, matching every other telemetry call
/// site in this codebase).
fn record_effect_via_telemetry(effect: DjEffect) {
    match effect {
        DjEffect::Transition(t) => {
            kaijutsu_telemetry::record_dj_clock_transition(t.to.as_str(), t.reason.as_str());
        }
        DjEffect::Slew(s) => {
            kaijutsu_telemetry::record_phasor_slew("dj", s.error_beats, s.deadbanded);
        }
        DjEffect::Click => {
            kaijutsu_telemetry::record_metronome_click();
        }
    }
}

// ── Translation functions — the testable core of the select! loop ──────────

/// Wallclock epoch-ns "now" for [`kaijutsu_audio::BeatRef::disposition`]'s
/// staleness math — mirrors `metronome.rs::ingest_beat_signals`'s reads.
fn now_epoch_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Translate one server event into `DjCore` calls + the resulting effects —
/// the `select!` loop's events arm, minus the channel/telemetry/sink
/// plumbing, so BeatSync→observe / `RENDER_FLUSH`→on_flush /
/// everything-else→ignored is unit-testable with hand-picked `Instant`s and
/// no tokio runtime. Every other `RenderCue` mime (Task #3/#4's cue
/// dispatch) falls through untouched — this task is clock + clicks only, the
/// same scope `DjCore` itself declares.
fn handle_server_event(
    core: &mut DjCore,
    event: &ServerEvent,
    now: Instant,
    now_epoch_ns: u64,
) -> Vec<DjEffect> {
    match event {
        ServerEvent::BeatSync { beat_ref, .. } => {
            let obs = core.observe_beat_sync(*beat_ref, now, now_epoch_ns);
            let mut effects = Vec::new();
            if let Some(slew) = obs.slew {
                effects.push(DjEffect::Slew(slew));
            }
            if let Some(t) = obs.transition {
                effects.push(DjEffect::Transition(t));
            }
            effects
        }
        ServerEvent::RenderCue { cue, .. } if cue.mime == RENDER_FLUSH_MIME => {
            core.on_flush(now).map(DjEffect::Transition).into_iter().collect()
        }
        _ => Vec::new(),
    }
}

/// Translate one connection-status level read into a `DjCore::on_disconnect`
/// call — ports `metronome.rs::halt_on_connection_loss`'s exact match (any
/// non-`Connected` status halts). Unlike the metronome (which reads the full
/// transition *stream* via `subscribe_status`), the DJ reads the *level* via
/// `watch_status` (`docs/midi.md`/context #4's steer): a rapid coalescing
/// blip can be missed, but a status that's still non-`Connected` by the time
/// this runs is still caught, and the level read is cheap and race-free
/// against a late-subscribing DJ thread.
fn handle_status_change(core: &mut DjCore, status: &ConnectionStatus, now: Instant) -> Option<DjEffect> {
    if matches!(status, ConnectionStatus::Connected { .. }) {
        return None;
    }
    core.on_disconnect(now).map(DjEffect::Transition)
}

/// Dispatch one `due_clicks` result to the MIDI sink (if present, and only
/// while clicks are enabled — mirrors the deleted `metronome.rs::click_on_beat`'s
/// own `config.enabled` gate, which sat at the dispatch site rather than
/// inside the click-policy math) and collect the resulting effects,
/// including one [`DjEffect::Click`] per offset actually dispatched (ported
/// telemetry counter — see that variant's doc). Extracted for the same
/// reason [`handle_server_event`] is: unit-testable without a tokio runtime
/// or a real sink — including the "no sink" path, a headless DJ (no ALSA
/// device) still reporting clock transitions correctly.
fn handle_due_clicks(
    metronome: &MetronomeConfig,
    sink: Option<&mut dyn MidiDispatch>,
    due: super::core::DueClicks,
) -> Vec<DjEffect> {
    let mut effects = Vec::new();
    if metronome.enabled
        && let Some(sink) = sink
    {
        for offset in &due.offsets {
            sink.click_at(metronome.note, metronome.channel, metronome.velocity, metronome.gate_ms, *offset);
            effects.push(DjEffect::Click);
        }
    }
    effects.extend(due.transition.map(DjEffect::Transition));
    effects
}

// ── run_loop — the thread's whole life ──────────────────────────────────────

/// The DJ thread's whole life once the tokio runtime is up: one `select!`
/// over {ctl, events, status, click timer, auto-connect timer, prefetch
/// outcomes}. Generic over `H` (the [`ActorSource`] carried by `ActorReady`),
/// `F` (where effects go), and `M` (the [`MidiDispatch`] sink) so this same
/// function drives both the real thread ([`thread_main`], with a real
/// [`ActorHandle`], telemetry, and a real ALSA-backed [`MidiSink`]) and the
/// thread-level tests (hand-built doubles for all three).
///
/// `midi` is owned loop-local (see `dj::midi`'s module doc for why it isn't a
/// `DjSinks` field: a real ALSA `Seq` is `!Send`, and `MidiSink` holds
/// nothing runtime-shaped that would force it into `thread_main`'s sync frame
/// the way `CasPrefetch` must be) — both the events arm's ABC dispatch and
/// the click-timer arm's clicks go through this SAME sink, "one app, one
/// port."
///
/// `prefetch`/`prefetch_rx` are the two halves of one
/// [`super::prefetch::CasPrefetch`] — split apart (rather than folded into
/// `sinks`) because only ONE task may ever drain a
/// `tokio::mpsc::UnboundedReceiver`, and this loop's prefetch-outcome arm is
/// that task; `prefetch` itself only ever needs to *send* (see that module's
/// doc). `prefetch` is a BORROW, not owned: it holds its own separate
/// multi-thread `tokio::runtime::Runtime`, and dropping a `Runtime` blocks
/// the calling thread — tokio disallows that from within an async context
/// (`Cannot drop a runtime in a context where blocking is not allowed`,
/// found live running this loop's own tests). So `CasPrefetch::new()` is
/// called by the SYNC caller ([`thread_main`]) before `rt.block_on` and
/// dropped after it returns; this function only ever sees a reference. Both
/// halves are held for the whole thread lifetime — unlike
/// `events_rx`/`status_rx`, neither is ever swapped or cleared, so the
/// prefetch-outcome `select!` arm needs no `Option`-guard/async-block idiom
/// (see that arm's own comment for how this differs from the events arm's
/// footgun).
async fn run_loop<H, F, M>(
    mut ctl_rx: mpsc::UnboundedReceiver<DjCtl<H>>,
    mut midi: M,
    sinks: DjSinks,
    prefetch: &CasPrefetch,
    mut prefetch_rx: mpsc::UnboundedReceiver<PrefetchOutcome>,
    pulse_tx: crossbeam_channel::Sender<DjPulse>,
    mut record_effect: F,
) where
    H: ActorSource,
    F: FnMut(DjEffect),
    M: MidiDispatch,
{
    let mut core = DjCore::default();
    let mut events_rx: Option<broadcast::Receiver<ServerEvent>> = None;
    let mut status_rx: Option<watch::Receiver<ConnectionStatus>> = None;
    // The render-port auto-connect one-shot (`dj::midi`'s module doc): its
    // own `tokio::time::interval` arm below, cadence `AUTOCONNECT_POLL`. The
    // `done` latch lives here (loop-local), not inside `midi` — `MidiSink`
    // only ever reports "settled or not" per attempt (`tick_autoconnect`);
    // the caller owns whether to keep asking.
    let mut autoconnect_timer = tokio::time::interval(AUTOCONNECT_POLL);
    // `Delay` (skip missed ticks, don't burst-catch-up): a DJ thread busy
    // long enough to miss several 2s retries has no reason to then fire the
    // auto-connect attempt several times back-to-back — one prompt retry
    // once it's free again is exactly as good as N.
    autoconnect_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut autoconnect_done = false;
    // First real consumer as of this task: gates/feeds every CAS prefetch
    // dispatch (`dispatch_render_cue`'s `Option<&SshConfig>` parameter)
    // exactly as the deleted `audio.rs`'s `conn.ssh_config.clone()` did.
    let mut ssh_config: Option<SshConfig> = None;
    // Log/telemetry correlation only (`DjCtl::ActorReady`'s doc) — a genuine
    // cross-generation guard on a prefetch outcome that started under a
    // since-replaced actor is a real follow-up (`docs/issues.md`), not yet
    // needed: `CasPrefetch` reconnects lazily per-dispatch `SshConfig`
    // rather than holding a connection keyed to one actor generation.
    let mut _generation: u64 = 0;
    // The other half of the old `conn.connected` check
    // (`play_render_cues`/`drain_prefetch_results`'s `Some(conn) if
    // conn.connected`) — read from the status-watch arm's *level*, exactly
    // as `handle_status_change` already does for `on_disconnect`.
    let mut connected = false;

    loop {
        let now = Instant::now();
        // The timer aims at whichever comes first: the next beat ENTERING the
        // click horizon (`next_wake`'s lead — waking at the beat itself would
        // schedule clicks with zero ALSA lead), or the instant BeatGrid goes
        // stale/free-run-capped with no further reference
        // (`next_stale_deadline` — without this bound, a slow tempo would
        // leave the mode machine on a dead grid until the next click
        // happened to wake the loop; 2026-07-18 deliberation, finding 5).
        // Recomputed every iteration (state changes retime it naturally —
        // e.g. a fresh Fold shortens it, a flush/disconnect lengthens it back
        // to `NO_PHASOR_SLEEP`).
        let wake = core
            .next_wake(now, SCHEDULE_HORIZON)
            .unwrap_or_else(|| now + NO_PHASOR_SLEEP);
        let wake = match core.next_stale_deadline() {
            Some(deadline) => wake.min(deadline),
            None => wake,
        };
        let sleep_target = tokio::time::Instant::from(wake);

        tokio::select! {
            ctl = ctl_rx.recv() => {
                match ctl {
                    Some(DjCtl::ActorReady { handle, ssh_config: cfg, generation: new_generation }) => {
                        debug!("kaijutsu-dj: new actor (generation {new_generation}) — resubscribing");
                        // Assigning drops whatever receiver was here before —
                        // "drop any previous subscription" from the spec.
                        events_rx = Some(handle.subscribe_events());
                        status_rx = Some(handle.watch_status());
                        ssh_config = Some(cfg);
                        _generation = new_generation;
                        // Seed from the LEVEL (`ActorSource::current_status`'s
                        // doc): the watch marks its current value seen, so an
                        // actor already Connected at resubscribe time would
                        // never announce itself — the fast-local-handshake
                        // race. The same read feeds the clock machine for
                        // consistency (idempotent when already Wallclock).
                        let status = handle.current_status();
                        connected = matches!(status, ConnectionStatus::Connected { .. });
                        if let Some(effect) =
                            handle_status_change(&mut core, &status, Instant::now())
                        {
                            record_effect(effect);
                        }
                    }
                    Some(DjCtl::MetronomeConfig(cfg)) => {
                        core.metronome = cfg;
                    }
                    Some(DjCtl::Shutdown) | None => {
                        debug!("kaijutsu-dj: shutting down ({})", if ctl_rx.is_closed() { "ctl channel closed" } else { "Shutdown" });
                        break;
                    }
                }
            }

            // `tokio::select!` evaluates every branch's expression to
            // construct its future EVEN WHEN the `if` guard disables that
            // branch — only *polling* is skipped (see `tokio::select!`'s own
            // doc, step 2: "the resulting future is not polled"). An async
            // block defers its body (the `.unwrap()`) to first-poll time, so
            // a disabled branch — guard false, body never runs — never
            // touches the `None`. Writing `events_rx.as_mut().unwrap()`
            // directly as the branch expression (no wrapping block) would
            // panic on construction every iteration `events_rx` is `None`,
            // guard or no guard.
            event = async { events_rx.as_mut().unwrap().recv().await }, if events_rx.is_some() => {
                match event {
                    Ok(ev) => {
                        // Read once per receipt (mirrors the deleted
                        // `audio.rs::play_render_cues`'s "Instant::now() ...
                        // read ONCE per batch" discipline) so the clock
                        // reaction and the cue dispatch below age against
                        // the SAME instant.
                        let event_now = Instant::now();
                        let event_epoch_ns = now_epoch_ns();
                        for effect in handle_server_event(&mut core, &ev, event_now, event_epoch_ns) {
                            record_effect(effect);
                        }
                        // Cue dispatch runs ALONGSIDE (not instead of)
                        // `handle_server_event`'s clock reaction to the same
                        // event — mirrors how the deleted `audio.rs` and
                        // `midi.rs` already independently read
                        // `RENDER_FLUSH_MIME` off one shared stream, now both
                        // folded onto this one events-arm iteration.
                        if let ServerEvent::RenderCue { cue, .. } = &ev {
                            dispatch_render_cue(
                                cue,
                                event_now,
                                event_epoch_ns,
                                connected,
                                ssh_config.as_ref(),
                                sinks.audio.as_ref(),
                                prefetch,
                            );
                            dispatch_midi_cue(cue, event_epoch_ns, &mut midi);
                            if midi.take_traffic() {
                                let _ = pulse_tx.send(DjPulse::RenderTraffic);
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // Never silently swallowed (house fail-loud posture)
                        // — keep receiving; the DJ's own independent
                        // subscription cursor is exactly what's supposed to
                        // make this rare (docs/midi.md's whole point).
                        warn!("kaijutsu-dj: event broadcast lagged by {n} messages — kept receiving");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        events_rx = None;
                    }
                }
            }

            changed = async { status_rx.as_mut().unwrap().changed().await }, if status_rx.is_some() => {
                match changed {
                    Ok(()) => {
                        let status = status_rx.as_ref().unwrap().borrow().clone();
                        connected = matches!(status, ConnectionStatus::Connected { .. });
                        if let Some(effect) = handle_status_change(&mut core, &status, Instant::now()) {
                            record_effect(effect);
                        }
                    }
                    Err(_) => {
                        // The watch sender was dropped (actor gone). Clear it
                        // so the `if status_rx.is_some()` guard stops this
                        // branch from busy-polling an already-closed watch —
                        // a fresh `ActorReady` restores it.
                        status_rx = None;
                    }
                }
            }

            _ = tokio::time::sleep_until(sleep_target) => {
                let due = core.due_clicks(Instant::now(), SCHEDULE_HORIZON);
                for effect in handle_due_clicks(&core.metronome, Some(&mut midi), due) {
                    record_effect(effect);
                }
                if midi.take_traffic() {
                    let _ = pulse_tx.send(DjPulse::RenderTraffic);
                }
            }

            // Patient one-shot retry of the render-port auto-connect
            // (`dj::midi`'s module doc). `tokio::time::interval` fires
            // immediately on its first `tick()` (its own documented
            // guarantee), giving the cold-start attempt the same promptness
            // the deleted Bevy `Timer`'s hand-primed `elapsed = duration`
            // trick used to buy. The `if !autoconnect_done` guard permanently
            // disables this arm once the one-shot settles — never re-armed,
            // per `MidiSink::tick_autoconnect`'s "never reverse a later cut"
            // doc.
            _ = autoconnect_timer.tick(), if !autoconnect_done => {
                autoconnect_done = midi.tick_autoconnect();
            }

            // No `Option`-guard/async-block wrapper needed here (contrast
            // the events/status arms above): `prefetch_rx` is owned outright
            // for the thread's whole life, never swapped or cleared, and its
            // paired `tx` half lives in `prefetch` (also owned for the whole
            // loop) — so `.recv()` can never spuriously return `None` while
            // this loop runs; a real `None` would mean `prefetch` itself was
            // dropped, which only happens when this function has already
            // returned.
            outcome = prefetch_rx.recv() => {
                if let Some(outcome) = outcome {
                    handle_prefetch_outcome(
                        outcome,
                        Instant::now(),
                        connected,
                        ssh_config.as_ref(),
                        sinks.audio.as_ref(),
                        prefetch,
                    );
                }
            }
        }
    }
}

/// Build the current-thread tokio runtime and run [`run_loop`] with real
/// telemetry recording — the production entry point [`DjPlugin::build`]
/// spawns onto the `"kaijutsu-dj"` thread. Mirrors
/// `connection/bootstrap.rs::bootstrap_thread`'s
/// `Builder::new_current_thread().enable_all()` shape (no `LocalSet` needed
/// here — nothing in this module is `!Send`, unlike the capnp actor).
///
/// `scheduler` arrives pre-spawned from [`DjPlugin::build`] (the rodio
/// thread has no dependency on this one — `audio_sched::spawn` just needs to
/// run once, somewhere, before the DJ starts dispatching cues).
///
/// `prefetch` is built and OWNED here, in this sync function, never inside
/// [`run_loop`] itself — found live (this task's thread-level tests panicked
/// on it before this was pinned down): [`super::prefetch::CasPrefetch`] owns
/// its own separate multi-thread `tokio::runtime::Runtime`, and dropping a
/// `Runtime` blocks the calling thread waiting for its workers to stop.
/// Tokio disallows that *specific* blocking op from inside an async context
/// (`Cannot drop a runtime in a context where blocking is not allowed`) —
/// so if `prefetch` were owned by (and dropped inside) `run_loop`'s async
/// stack frame, its `Drop` would fire while still polled by `rt.block_on`
/// below and panic. Building AND dropping it out here, wrapped around
/// `block_on` rather than inside it, keeps both firmly in sync-land; only a
/// `&CasPrefetch` borrow crosses into the async world.
fn thread_main(
    ctl_rx: mpsc::UnboundedReceiver<DjCtl>,
    pulse_tx: crossbeam_channel::Sender<DjPulse>,
    scheduler: AudioSchedulerHandle,
) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build kaijutsu-dj tokio runtime");
    let (prefetch, prefetch_rx) = CasPrefetch::new();
    let sinks = DjSinks { audio: Some(scheduler) };
    // `MidiSink::default()` here — not inside `run_loop` — for the same
    // reason it's a plain local at all: this function already runs ON the DJ
    // thread (spawned by `DjPlugin::build` below), so building it here vs. as
    // `run_loop`'s first local statement is purely stylistic; kept here to
    // keep `thread_main`'s "everything this thread owns, assembled once" shape
    // symmetric with `prefetch`.
    rt.block_on(run_loop(
        ctl_rx,
        MidiSink::default(),
        sinks,
        &prefetch,
        prefetch_rx,
        pulse_tx,
        record_effect_via_telemetry,
    ));
}

// ── Bevy-side: DjHandle resource + DjPlugin ─────────────────────────────────

/// The Bevy-side handle to the DJ thread: a ctl sender and the pulse mirror
/// receiver. `crossbeam_channel::Receiver` is already `Sync` (unlike tokio's
/// `mpsc::UnboundedReceiver`, which is why [`RpcResultChannel`]
/// (`connection/actor_plugin.rs`) needs a `Mutex` wrapper and this doesn't).
#[derive(Resource)]
pub struct DjHandle {
    ctl_tx: mpsc::UnboundedSender<DjCtl>,
    pulse_rx: crossbeam_channel::Receiver<DjPulse>,
}

impl Drop for DjHandle {
    fn drop(&mut self) {
        // Best-effort clean-shutdown signal. `audio_sched.rs`'s scheduler
        // thread has no equivalent — it relies solely on its channel closing
        // when every `Sender` drops — but `DjCtl` already has an explicit
        // `Shutdown` the select! loop understands, so sending it here costs
        // nothing and makes the intent explicit rather than incidental.
        // Never blocks: no thread join, matching audio_sched's fire-and-
        // forget posture — worst case (send fails because the thread is
        // already gone) the drop of `ctl_tx` right after this still closes
        // the channel, which the loop treats identically.
        let _ = self.ctl_tx.send(DjCtl::Shutdown);
    }
}

/// A render-port send just happened — the patch bay lights the RENDER chord.
/// The app can only observe its OWN traffic (`docs/scenes/patchbay.md`, the
/// live layer): every send out the render seq port — an ABC cue or a 拍子木
/// click — is one edge-observable event. Moved here from the deleted
/// `midi.rs` (Task #4): the DJ thread is the sole producer now, mirrored
/// Bevy-side by [`drain_dj_pulses`] folding each [`DjPulse::RenderTraffic`]
/// into one of these. Consumed only by the patch bay
/// (`view/patch_bay/mod.rs`).
#[derive(Message)]
pub struct RenderPortTraffic;

/// Spawns the DJ thread and wires the Bevy-side forwarding systems
/// (`docs/midi.md` "The DJ thread"). Registered in `main.rs`, replacing the
/// deleted `AudioOutPlugin`/`MidiOutPlugin`/`MetronomePlugin` — the DJ
/// dispatches every `audio/*`/`CLIP_MIME`/`PREPARE_MIME`/`text/vnd.abc` cue
/// and every metronome click for real, so there is a live sink to make its
/// output audible.
pub struct DjPlugin;

impl Plugin for DjPlugin {
    fn build(&self, app: &mut App) {
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<DjCtl>();
        let (pulse_tx, pulse_rx) = crossbeam_channel::unbounded::<DjPulse>();
        // Spawning the rodio scheduler thread moves here from the deleted
        // `AudioOutPlugin::build` (`docs/pcm.md` R5) — its Bevy `Resource`
        // insertion disappears with it: `audio.rs`'s now-deleted systems
        // were its only consumer, so the handle rides straight into the DJ
        // thread's own sinks instead.
        let scheduler = crate::audio_sched::spawn();

        std::thread::Builder::new()
            .name("kaijutsu-dj".into())
            .spawn(move || thread_main(ctl_rx, pulse_tx, scheduler))
            .expect("spawn kaijutsu-dj thread");

        app.insert_resource(DjHandle { ctl_tx, pulse_rx });
        app.add_message::<RenderPortTraffic>();
        app.add_systems(Update, (forward_actor_to_dj, forward_metronome_config_to_dj, drain_dj_pulses));
    }
}

/// Forward a fresh `RpcActor` to the DJ thread — the same `actor.is_changed()`
/// idiom `poll_server_events` uses to detect a respawn/reconnect (a new
/// generation after the bootstrap thread replaces the resource). The DJ
/// thread holds its OWN broadcast subscription (`docs/midi.md`: "independent
/// cursor — a stalled UI drain costs it nothing"), so it must re-subscribe on
/// every change exactly like the UI poll systems do, never share theirs.
fn forward_actor_to_dj(actor: Option<Res<RpcActor>>, conn: Res<RpcConnectionState>, dj: Res<DjHandle>) {
    let Some(actor) = actor else { return };
    if !actor.is_changed() {
        return;
    }
    let _ = dj.ctl_tx.send(DjCtl::ActorReady {
        handle: actor.handle.clone(),
        ssh_config: conn.ssh_config.clone(),
        generation: actor.generation,
    });
}

/// Parse a fetched `metronome.toml` and forward it to the DJ thread — the
/// same source event `metronome.rs::apply_metronome_config` consumes, mirrored
/// here so the DJ's own click config tracks the per-client config
/// independently of the (still-live, until Task #4) metronome resource. A
/// parse failure warns and keeps the DJ's current config — ports
/// `apply_metronome_config`'s existing fail-loud-but-don't-revert posture
/// verbatim, never a silent fallback.
fn forward_metronome_config_to_dj(mut results: MessageReader<RpcResultMessage>, dj: Res<DjHandle>) {
    for result in results.read() {
        if let RpcResultMessage::MetronomeConfigReceived(toml) = result {
            match toml::from_str::<MetronomeConfig>(toml) {
                Ok(cfg) => {
                    let _ = dj.ctl_tx.send(DjCtl::MetronomeConfig(cfg));
                }
                Err(e) => {
                    log::error!("metronome.toml is unparseable: {e}; DJ thread keeps its current config");
                }
            }
        }
    }
}

/// Drain the DJ→Bevy mirror channel: fold each [`DjPulse`] into the Bevy-side
/// message its consumer reads (`docs/midi.md`: "room glow, block sync"
/// precedent from `poll_server_events`). The only pulse today is
/// [`DjPulse::RenderTraffic`] → [`RenderPortTraffic`], consumed by the patch
/// bay (`view/patch_bay/mod.rs::pulse_render_chords`).
fn drain_dj_pulses(dj: Res<DjHandle>, mut traffic: MessageWriter<RenderPortTraffic>) {
    while let Ok(pulse) = dj.pulse_rx.try_recv() {
        match pulse {
            DjPulse::RenderTraffic => {
                traffic.write(RenderPortTraffic);
            }
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_sched::{self, SchedulerCmd};
    use kaijutsu_audio::{ABC_MIME, BeatRef, CuePayload, RenderCue};
    use kaijutsu_types::ContextId;

    use super::super::core::{ClockMode, DueClicks, TransitionReason};

    fn beat_sync(beat: f64, tempo_bps: f64) -> ServerEvent {
        ServerEvent::BeatSync { context_id: ContextId::new(), beat_ref: BeatRef::new(beat, tempo_bps) }
    }

    fn flush_cue() -> ServerEvent {
        ServerEvent::RenderCue {
            context_id: ContextId::new(),
            cue: RenderCue {
                mime: RENDER_FLUSH_MIME.into(),
                payload: CuePayload::Inline(vec![]),
                lead: Duration::ZERO,
                epoch_ns: 0,
            },
        }
    }

    fn non_flush_cue() -> ServerEvent {
        ServerEvent::RenderCue {
            context_id: ContextId::new(),
            cue: RenderCue {
                mime: "text/vnd.abc".into(),
                payload: CuePayload::Inline(vec![]),
                lead: Duration::ZERO,
                epoch_ns: 0,
            },
        }
    }

    // ── handle_server_event ────────────────────────────────────────────

    #[test]
    fn a_beat_sync_event_folds_and_reports_the_anchoring_transition() {
        let mut core = DjCore::default();
        let now = Instant::now();
        let effects = handle_server_event(&mut core, &beat_sync(0.0, 2.0), now, 0);
        assert_eq!(core.mode(), ClockMode::BeatGrid);
        assert_eq!(
            effects,
            vec![DjEffect::Transition(ClockTransition {
                to: ClockMode::BeatGrid,
                reason: TransitionReason::Fold
            })],
            "the anchoring fold has no slew to report (first-ever fold), only the transition"
        );
    }

    #[test]
    fn a_second_beat_sync_while_already_dialed_in_reports_a_slew_not_a_repeat_transition() {
        let mut core = DjCore::default();
        let t0 = Instant::now();
        handle_server_event(&mut core, &beat_sync(0.0, 2.0), t0, 0);

        let t1 = t0 + Duration::from_secs(1);
        let effects = handle_server_event(&mut core, &beat_sync(2.0, 2.0), t1, 0);
        assert_eq!(effects.len(), 1, "no repeat transition on an ordinary fold: {effects:?}");
        assert!(matches!(effects[0], DjEffect::Slew(_)), "an ongoing fold reports a Slew: {effects:?}");
    }

    #[test]
    fn a_render_flush_cue_calls_on_flush_and_reports_the_transition() {
        let mut core = DjCore::default();
        let t0 = Instant::now();
        handle_server_event(&mut core, &beat_sync(0.0, 2.0), t0, 0);
        assert_eq!(core.mode(), ClockMode::BeatGrid);

        let effects = handle_server_event(&mut core, &flush_cue(), t0, 0);
        assert_eq!(core.mode(), ClockMode::Wallclock);
        assert_eq!(
            effects,
            vec![DjEffect::Transition(ClockTransition {
                to: ClockMode::Wallclock,
                reason: TransitionReason::Flush
            })]
        );
    }

    #[test]
    fn a_non_flush_render_cue_is_ignored_this_task() {
        let mut core = DjCore::default();
        let effects = handle_server_event(&mut core, &non_flush_cue(), Instant::now(), 0);
        assert!(effects.is_empty(), "cue dispatch is Task #3/#4 — ignored here, not acted on");
        assert_eq!(core.mode(), ClockMode::Wallclock, "an ignored event must not touch the clock");
    }

    #[test]
    fn an_unrelated_server_event_is_ignored() {
        let mut core = DjCore::default();
        let event = ServerEvent::Reconnected;
        let effects = handle_server_event(&mut core, &event, Instant::now(), 0);
        assert!(effects.is_empty());
    }

    // ── handle_status_change ────────────────────────────────────────────

    #[test]
    fn a_non_connected_status_triggers_on_disconnect() {
        let mut core = DjCore::default();
        let t0 = Instant::now();
        handle_server_event(&mut core, &beat_sync(0.0, 2.0), t0, 0);
        assert_eq!(core.mode(), ClockMode::BeatGrid);

        let effect = handle_status_change(
            &mut core,
            &ConnectionStatus::Closing { cause: "kernel restart".into() },
            t0,
        );
        assert_eq!(
            effect,
            Some(DjEffect::Transition(ClockTransition {
                to: ClockMode::Wallclock,
                reason: TransitionReason::Disconnect
            }))
        );
        assert_eq!(core.mode(), ClockMode::Wallclock);
    }

    #[test]
    fn a_connected_status_is_a_no_op() {
        let mut core = DjCore::default();
        let t0 = Instant::now();
        handle_server_event(&mut core, &beat_sync(0.0, 2.0), t0, 0);

        let effect = handle_status_change(
            &mut core,
            &ConnectionStatus::Connected { kernel_id: kaijutsu_types::KernelId::new(), context_id: None, since_ms: 0 },
            t0,
        );
        assert_eq!(effect, None);
        assert_eq!(core.mode(), ClockMode::BeatGrid, "a healthy status must not disturb a running phasor");
    }

    // ── handle_due_clicks ────────────────────────────────────────────────

    /// One command a [`RecordingMidiSink`] recorded — the thread-level
    /// double's whole vocabulary: clicks (`handle_due_clicks`'s tests below),
    /// scheduled ABC phrases (`thread_dispatches_an_abc_cue_to_the_midi_sink`),
    /// and flushes.
    #[derive(Debug, Clone, PartialEq)]
    enum MidiCmd {
        ClickAt { note: u8, channel: u8, velocity: u8, gate_ms: u64, offset: Duration },
        ScheduleAbc { events: Vec<(Duration, Vec<u8>)>, lead: Duration },
        Flush,
    }

    /// A channel-backed [`MidiDispatch`] double — the MIDI-sink analog of
    /// `audio_sched::test_handle()`: a real implementor of the production
    /// trait, backed by a plain channel instead of ALSA, so `handle_due_clicks`
    /// and the real `run_loop` can both be driven end to end with no device.
    struct RecordingMidiSink {
        tx: crossbeam_channel::Sender<MidiCmd>,
    }

    impl RecordingMidiSink {
        fn new() -> (Self, crossbeam_channel::Receiver<MidiCmd>) {
            let (tx, rx) = crossbeam_channel::unbounded();
            (Self { tx }, rx)
        }
    }

    impl MidiDispatch for RecordingMidiSink {
        fn click_at(&mut self, note: u8, channel: u8, velocity: u8, gate_ms: u64, offset: Duration) {
            let _ = self.tx.send(MidiCmd::ClickAt { note, channel, velocity, gate_ms, offset });
        }
        fn schedule_abc(&mut self, events: Vec<(Duration, Vec<u8>)>, lead: Duration) {
            let _ = self.tx.send(MidiCmd::ScheduleAbc { events, lead });
        }
        fn flush(&mut self) {
            let _ = self.tx.send(MidiCmd::Flush);
        }
        fn take_traffic(&mut self) -> bool {
            false // not exercised by these tests; the pulse-coalescing is untested plumbing here
        }
        fn tick_autoconnect(&mut self) -> bool {
            true // settle immediately — no ALSA graph for a recording double to introspect
        }
    }

    #[test]
    fn due_clicks_dispatch_to_the_sink_when_enabled() {
        let (mut sink, rx) = RecordingMidiSink::new();
        let cfg = MetronomeConfig { enabled: true, note: 84, channel: 15, velocity: 110, gate_ms: 60 };
        let due = DueClicks { offsets: vec![Duration::from_millis(200)], transition: None };

        let effects = handle_due_clicks(&cfg, Some(&mut sink), due);
        assert_eq!(effects, vec![DjEffect::Click], "one Click effect per dispatched offset");
        assert_eq!(
            rx.try_recv(),
            Ok(MidiCmd::ClickAt { note: 84, channel: 15, velocity: 110, gate_ms: 60, offset: Duration::from_millis(200) })
        );
        assert!(rx.try_recv().is_err(), "exactly one dispatch");
    }

    #[test]
    fn due_clicks_skip_the_sink_when_disabled() {
        let (mut sink, rx) = RecordingMidiSink::new();
        let cfg = MetronomeConfig { enabled: false, note: 84, channel: 15, velocity: 110, gate_ms: 60 };
        let due = DueClicks { offsets: vec![Duration::from_millis(200)], transition: None };

        let effects = handle_due_clicks(&cfg, Some(&mut sink), due);
        assert!(effects.is_empty(), "disabled config dispatches nothing, no Click effects either");
        assert!(rx.try_recv().is_err(), "disabled config must not dispatch, even with a sink present");
    }

    #[test]
    fn due_clicks_with_no_sink_still_report_transitions_headless_correctness() {
        let cfg = MetronomeConfig::default();
        let due = DueClicks {
            offsets: vec![Duration::ZERO],
            transition: Some(ClockTransition { to: ClockMode::Wallclock, reason: TransitionReason::Stale }),
        };
        let effects = handle_due_clicks(&cfg, None, due);
        assert_eq!(
            effects,
            vec![DjEffect::Transition(ClockTransition { to: ClockMode::Wallclock, reason: TransitionReason::Stale })],
            "a headless DJ (no click sink) still reports clock transitions correctly"
        );
    }

    // ── The real thread: subscription swap + shutdown ──────────────────

    /// A hand-built [`ActorSource`] double: a `broadcast`/`watch` pair the
    /// test drives directly, standing in for a live `ActorHandle`.
    struct TestSource {
        events: broadcast::Sender<ServerEvent>,
        status: watch::Sender<ConnectionStatus>,
    }

    impl ActorSource for TestSource {
        fn subscribe_events(&self) -> broadcast::Receiver<ServerEvent> {
            self.events.subscribe()
        }
        fn watch_status(&self) -> watch::Receiver<ConnectionStatus> {
            self.status.subscribe()
        }
        fn current_status(&self) -> ConnectionStatus {
            // The level, exactly as `ActorHandle::current_status` reads its
            // own watch — so the seed path behaves identically under test.
            self.status.borrow().clone()
        }
    }

    /// Repeatedly (re)send `mk_event()` on `events` and poll `effect_rx`
    /// until an effect surfaces. Necessary (not just belt-and-suspenders):
    /// a `broadcast` subscriber created on the DJ thread only sees messages
    /// sent AFTER it subscribes, and the ctl arm's `ActorReady` processing
    /// (which does the subscribing) races this test thread's sends by
    /// construction — two independent OS threads, one channel hop apart.
    /// Bounded by `timeout`; panics (a loud test failure) rather than
    /// hanging past it.
    fn send_until_observed(
        events: &broadcast::Sender<ServerEvent>,
        effect_rx: &std::sync::mpsc::Receiver<DjEffect>,
        mk_event: impl Fn() -> ServerEvent,
        timeout: Duration,
    ) -> DjEffect {
        let deadline = Instant::now() + timeout;
        loop {
            let _ = events.send(mk_event());
            match effect_rx.recv_timeout(Duration::from_millis(20)) {
                Ok(effect) => return effect,
                Err(_) => {
                    if Instant::now() >= deadline {
                        panic!("no effect observed on the current subscription within {timeout:?}");
                    }
                }
            }
        }
    }

    /// Wait for the next `Transition` effect, skipping `Slew`/`Click` noise
    /// (the send-until-observed harness can fold the same reference more than
    /// once, and each extra fold emits a Slew; a live phasor can also click
    /// in the background of this test). Bounded by `timeout`; panics loud
    /// rather than hanging.
    fn expect_transition(
        effect_rx: &std::sync::mpsc::Receiver<DjEffect>,
        timeout: Duration,
    ) -> ClockTransition {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match effect_rx.recv_timeout(remaining) {
                Ok(DjEffect::Transition(t)) => return t,
                Ok(DjEffect::Slew(_)) | Ok(DjEffect::Click) => continue, // harness-induced, not the observable
                Err(_) => panic!("no clock transition observed within {timeout:?}"),
            }
        }
    }

    #[test]
    fn thread_swaps_subscriptions_for_real_and_shuts_down_cleanly() {
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<DjCtl<TestSource>>();
        let (pulse_tx, _pulse_rx) = crossbeam_channel::unbounded::<DjPulse>();
        let (effect_tx, effect_rx) = std::sync::mpsc::channel::<DjEffect>();

        let join = std::thread::Builder::new()
            .name("dj-test".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                let (prefetch, prefetch_rx) = CasPrefetch::new();
                rt.block_on(run_loop(ctl_rx, MidiSink::default(), DjSinks::default(), &prefetch, prefetch_rx, pulse_tx, move |effect| {
                    let _ = effect_tx.send(effect);
                }));
            })
            .expect("spawn test dj thread");

        // Subscription A: send ActorReady, then confirm it's really live by
        // driving a fresh, unstamped BeatSync to the deterministic first-ever
        // Fold transition.
        let (events_a, _keep_a) = broadcast::channel(16);
        let (status_a, _keep_sa) = watch::channel(ConnectionStatus::Idle);
        ctl_tx
            .send(DjCtl::ActorReady {
                handle: TestSource { events: events_a.clone(), status: status_a.clone() },
                ssh_config: SshConfig::default(),
                generation: 1,
            })
            .expect("send ActorReady A");

        let anchored = send_until_observed(&events_a, &effect_rx, || beat_sync(0.0, 2.0), Duration::from_secs(2));
        assert_eq!(
            anchored,
            DjEffect::Transition(ClockTransition { to: ClockMode::BeatGrid, reason: TransitionReason::Fold }),
            "sub A must be live and processed for real through the actual thread"
        );

        // Swap to subscription B — whose status LEVEL is Idle at ActorReady
        // time. The seed (`ActorSource::current_status`, the watch-marks-
        // current-value-seen remedy) must read that level and halt the clock
        // WITHOUT any watch change event ever being sent on B: the
        // Wallclock/Disconnect transition below is produced by the ActorReady
        // arm alone. This is also behavior parity with the old Bevy path,
        // where poll_connection_status's current_status() seed rode into
        // halt_on_connection_loss on every actor swap.
        let (events_b, _keep_b) = broadcast::channel(16);
        let (status_b, _keep_sb) = watch::channel(ConnectionStatus::Idle);
        ctl_tx
            .send(DjCtl::ActorReady {
                handle: TestSource { events: events_b.clone(), status: status_b.clone() },
                ssh_config: SshConfig::default(),
                generation: 2,
            })
            .expect("send ActorReady B");

        let seeded = expect_transition(&effect_rx, Duration::from_secs(2));
        assert_eq!(
            seeded,
            ClockTransition { to: ClockMode::Wallclock, reason: TransitionReason::Disconnect },
            "ActorReady must seed from the status LEVEL — no watch event was ever sent on B"
        );

        // Confirm B's event subscription is live: a fresh anchor on B.
        let reanchored = send_until_observed(&events_b, &effect_rx, || beat_sync(0.0, 2.0), Duration::from_secs(2));
        assert_eq!(
            reanchored,
            DjEffect::Transition(ClockTransition { to: ClockMode::BeatGrid, reason: TransitionReason::Fold }),
            "sub B must be live and processed for real through the actual thread"
        );

        // Drain any trailing effects (send_until_observed may have sent the
        // anchoring BeatSync more than once; each extra one folds and emits a
        // Slew) so the dropped-A quiet assertion below starts from silence.
        while effect_rx.recv_timeout(Duration::from_millis(100)).is_ok() {}

        // Now that the swap is CONFIRMED complete, a further send on the
        // dropped subscription A must produce nothing — the old receiver is
        // gone, not merely deprioritized. (A fold on the live subscription
        // would emit at least a Slew, so silence-of-Slew-or-Transition
        // discriminates.) `DjEffect::Click` is filtered rather than treated
        // as silence-breaking: B's own phasor is dialed in and clicks
        // enabled by default, so the click timer legitimately fires in the
        // background on its own cadence, independent of anything sent on A
        // or B — that's not the signal this assertion is checking for.
        let _ = events_a.send(beat_sync(0.0, 2.0));
        let quiet_deadline = Instant::now() + Duration::from_millis(200);
        loop {
            let remaining = quiet_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break; // true silence (aside from background clicks) — the expected outcome
            }
            match effect_rx.recv_timeout(remaining) {
                Ok(DjEffect::Click) => continue, // background click-timer noise, not from A
                Ok(other) => panic!(
                    "the DROPPED subscription A must no longer be polled after the swap to B, got {other:?}"
                ),
                Err(_) => break,
            }
        }

        // Shutdown must join cleanly, not hang.
        ctl_tx.send(DjCtl::Shutdown).expect("send Shutdown");
        join.join().expect("dj test thread panicked");
    }

    #[test]
    fn thread_exits_cleanly_when_the_ctl_channel_simply_closes() {
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<DjCtl<TestSource>>();
        let (pulse_tx, _pulse_rx) = crossbeam_channel::unbounded::<DjPulse>();

        let join = std::thread::Builder::new()
            .name("dj-test-close".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                let (prefetch, prefetch_rx) = CasPrefetch::new();
                rt.block_on(run_loop(ctl_rx, MidiSink::default(), DjSinks::default(), &prefetch, prefetch_rx, pulse_tx, |_effect: DjEffect| {}));
            })
            .expect("spawn test dj thread");

        drop(ctl_tx); // every sender gone — the ctl arm's `None` path
        join.join().expect("dj test thread panicked");
    }

    /// The named thread-level test (`docs/midi.md` DJ-thread arc Task #3):
    /// feed a `RenderCue` through the REAL `run_loop` (not the translation
    /// function directly, unlike `dj::audio`'s own suite) and assert the
    /// resulting `SchedulerCmd` arrives on a `audio_sched::test_handle`
    /// receiver — proves the events arm's cue-dispatch wiring end to end,
    /// not just `dispatch_render_cue`'s own logic in isolation.
    #[test]
    fn thread_dispatches_a_render_cue_to_the_scheduler() {
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<DjCtl<TestSource>>();
        let (pulse_tx, _pulse_rx) = crossbeam_channel::unbounded::<DjPulse>();
        let (scheduler, scheduler_rx) = audio_sched::test_handle();

        let join = std::thread::Builder::new()
            .name("dj-test-audio".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                let (prefetch, prefetch_rx) = CasPrefetch::new();
                let sinks = DjSinks { audio: Some(scheduler) };
                rt.block_on(run_loop(ctl_rx, MidiSink::default(), sinks, &prefetch, prefetch_rx, pulse_tx, |_effect: DjEffect| {}));
            })
            .expect("spawn test dj thread");

        let (events, _keep_events) = broadcast::channel(16);
        let (status, _keep_status) = watch::channel(ConnectionStatus::Idle);
        ctl_tx
            .send(DjCtl::ActorReady {
                handle: TestSource { events: events.clone(), status },
                ssh_config: SshConfig::default(),
                generation: 1,
            })
            .expect("send ActorReady");

        // A zero-lead, unstamped inline audio cue is the play-now fast path
        // (mirrors `dj::audio::tests::inline_audio_cue_zero_lead_sends_one_play_now`)
        // — deterministic and connection-free, so this test only needs to
        // prove the WIRING (events arm -> dispatch_render_cue -> scheduler),
        // not re-cover dispatch decisions the sibling suite already owns.
        let cue = RenderCue::now_inline("audio/wav", vec![1, 2, 3, 4]);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let _ = events.send(ServerEvent::RenderCue { context_id: ContextId::new(), cue: cue.clone() });
            match scheduler_rx.recv_timeout(Duration::from_millis(20)) {
                Ok(SchedulerCmd::PlayNow { bytes }) => {
                    assert_eq!(bytes, vec![1, 2, 3, 4]);
                    break;
                }
                Ok(other) => panic!("expected PlayNow, got {other:?}"),
                Err(_) => {
                    if Instant::now() >= deadline {
                        panic!("no SchedulerCmd observed within 2s");
                    }
                }
            }
        }

        ctl_tx.send(DjCtl::Shutdown).expect("send Shutdown");
        join.join().expect("dj test thread panicked");
    }

    /// The Task #4 sibling of `thread_dispatches_a_render_cue_to_the_scheduler`:
    /// feed an ABC `RenderCue` through the REAL `run_loop` and assert the
    /// resulting `MidiCmd::ScheduleAbc` arrives on a [`RecordingMidiSink`]
    /// receiver — proves the events arm's ABC-dispatch wiring
    /// (`super::midi::dispatch_midi_cue`) end to end, not just that
    /// function's own logic in isolation (`dj::midi`'s own test module
    /// already covers that).
    #[test]
    fn thread_dispatches_an_abc_cue_to_the_midi_sink() {
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<DjCtl<TestSource>>();
        let (pulse_tx, _pulse_rx) = crossbeam_channel::unbounded::<DjPulse>();
        let (midi, midi_rx) = RecordingMidiSink::new();

        let join = std::thread::Builder::new()
            .name("dj-test-midi".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                let (prefetch, prefetch_rx) = CasPrefetch::new();
                rt.block_on(run_loop(ctl_rx, midi, DjSinks::default(), &prefetch, prefetch_rx, pulse_tx, |_effect: DjEffect| {}));
            })
            .expect("spawn test dj thread");

        let (events, _keep_events) = broadcast::channel(16);
        let (status, _keep_status) = watch::channel(ConnectionStatus::Idle);
        ctl_tx
            .send(DjCtl::ActorReady {
                handle: TestSource { events: events.clone(), status },
                ssh_config: SshConfig::default(),
                generation: 1,
            })
            .expect("send ActorReady");

        // A brisk four-note phrase, unstamped + zero-lead — deterministic and
        // connection-free, so this test only needs to prove the WIRING
        // (events arm -> dispatch_midi_cue -> the owned sink), not re-cover
        // ABC-render/backdate decisions `dj::midi`'s own suite already owns.
        const CDEF: &str = "X:1\nT:t\nM:4/4\nL:1/4\nQ:1/4=120\nK:C\nCDEF|\n";
        let cue = RenderCue::now_inline(ABC_MIME, CDEF.as_bytes().to_vec());
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let _ = events.send(ServerEvent::RenderCue { context_id: ContextId::new(), cue: cue.clone() });
            match midi_rx.recv_timeout(Duration::from_millis(20)) {
                Ok(MidiCmd::ScheduleAbc { events, lead }) => {
                    let note_ons = events
                        .iter()
                        .filter(|(_, d)| d.len() == 3 && d[0] & 0xF0 == 0x90 && d[2] > 0)
                        .count();
                    assert_eq!(note_ons, 4, "CDEF renders to four NoteOns: {events:?}");
                    assert_eq!(lead, Duration::ZERO, "unstamped, zero-lead cue: lead untouched");
                    break;
                }
                Ok(other) => panic!("expected ScheduleAbc, got {other:?}"),
                Err(_) => {
                    if Instant::now() >= deadline {
                        panic!("no MidiCmd observed within 2s");
                    }
                }
            }
        }

        ctl_tx.send(DjCtl::Shutdown).expect("send Shutdown");
        join.join().expect("dj test thread panicked");
    }

    // ── Bevy-side: forward_metronome_config_to_dj ───────────────────────

    #[test]
    fn forward_metronome_config_parses_and_forwards_to_the_ctl_channel() {
        let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel::<DjCtl>();
        let (_pulse_tx, pulse_rx) = crossbeam_channel::unbounded::<DjPulse>();

        let mut app = App::new();
        app.insert_resource(DjHandle { ctl_tx, pulse_rx })
            .add_message::<RpcResultMessage>()
            .add_systems(Update, forward_metronome_config_to_dj);
        app.world_mut().write_message(RpcResultMessage::MetronomeConfigReceived(
            "enabled = false\nnote = 72\nchannel = 9\nvelocity = 40\ngate_ms = 30\n".to_string(),
        ));
        app.update();

        // Read the ctl channel BEFORE `app` (and its `DjHandle`) drops —
        // `DjHandle::drop` sends its own `DjCtl::Shutdown`, which would
        // otherwise queue up behind the message forwarded above.
        //
        // `DjCtl<ActorHandle>` isn't `Debug` (`ActorHandle` isn't), so match
        // explicitly rather than formatting the whole value on failure.
        match ctl_rx.try_recv() {
            Ok(DjCtl::MetronomeConfig(cfg)) => {
                assert_eq!(cfg, MetronomeConfig { enabled: false, note: 72, channel: 9, velocity: 40, gate_ms: 30 });
            }
            Ok(DjCtl::ActorReady { .. }) => panic!("expected MetronomeConfig, got ActorReady"),
            Ok(DjCtl::Shutdown) => panic!("expected MetronomeConfig, got Shutdown"),
            Err(e) => panic!("expected a forwarded MetronomeConfig, got error: {e:?}"),
        }
    }

    #[test]
    fn forward_metronome_config_keeps_quiet_on_a_parse_failure() {
        let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel::<DjCtl>();
        let (_pulse_tx, pulse_rx) = crossbeam_channel::unbounded::<DjPulse>();

        let mut app = App::new();
        app.insert_resource(DjHandle { ctl_tx, pulse_rx })
            .add_message::<RpcResultMessage>()
            .add_systems(Update, forward_metronome_config_to_dj);
        app.world_mut()
            .write_message(RpcResultMessage::MetronomeConfigReceived("this is not valid toml =".to_string()));
        app.update();

        // Read before `app` drops — see the sibling test's comment on why
        // `DjHandle::drop`'s own `Shutdown` must not be read here instead.
        assert!(
            ctl_rx.try_recv().is_err(),
            "a parse failure must forward nothing — the DJ thread keeps its current config"
        );
    }
}
