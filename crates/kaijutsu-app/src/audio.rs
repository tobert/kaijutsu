//! Render sink — the app side of `docs/pcm.md` / `docs/midi.md`'s mime-keyed
//! render cue seam.
//!
//! `kaijutsu-audio::RenderSink` is the *conceptual* seam ("one sink, emit a
//! cue"); this module is the app's implementation of that idea. Playback
//! itself never happens here: every `audio/*` and `application/vnd.kaijutsu.
//! clip+json` cue is translated into a [`crate::audio_sched::SchedulerCmd`]
//! and handed to the dedicated rodio scheduler thread (`audio_sched.rs`,
//! `docs/pcm.md` R5 — "the app owns rodio outright"). This module's job is
//! purely *deciding*: parse the cue, resolve its bytes (inline or CAS), work
//! out the backdated deadline, and dispatch. `RenderSink::emit` takes `&self`
//! deliberately (`docs/pcm.md` "play(&self) vs emit(&mut self)") — a Bevy
//! system already has that shape (world access without `&mut self` on some
//! sink struct), so the system reading `ServerEventMessage` plays the sink
//! role directly rather than wrapping it in a type that would just forward
//! straight through with no seam benefit.
//!
//! **CAS resolution is unchanged** (`docs/slash-v.md` track B): a
//! `CuePayload::Cas(hash)` cue resolves through [`CasResolver`] off the Bevy
//! main thread via [`CasPrefetch`]'s own tokio runtime, landing back here on
//! a crossbeam channel. This is still fetch-on-cue (the prepare-horizon
//! prefetch is R4, not yet built) — the difference from the pre-R5 sink is
//! only WHAT happens once bytes land: they're handed to the scheduler
//! instead of spawned as a Bevy `AudioPlayer`.
//!
//! **Backdating now applies to every audio/clip cue**, not just the
//! zero-lead case: [`crate::audio_sched::effective_deadline`] mirrors
//! `midi.rs::backdate_events`'s epoch-backdating discipline (`docs/midi.md`
//! "The one timebase"). The deadline is snapshotted at CUE RECEIPT — before
//! any CAS fetch is even dispatched — so fetch latency never folds into
//! audio jitter (`docs/pcm.md` Decision 4): a CAS-resolved cue plays against
//! its *original* deadline once bytes land, even if that's now in the past
//! (the scheduler fires an overdue `PlayAt` immediately rather than
//! re-deriving a fresh one).
//!
//! **The clip renderer (R1)**: a `CLIP_MIME` cue is parsed structurally
//! (`Clip::parse` — the kernel already ran `parse_validated` at commit;
//! `media`-presence failures here are still loud, never a panic), its
//! `media` hash resolved through the SAME `CasResolver` path as a raw audio
//! cue, and scheduled with `src_offset_ms`/`src_len_ms`/`gain_db` applied.
//! Both `Inline` (the JSON record itself, small, the usual case) and `Cas`
//! (a CAS-stored record) clip payloads are supported — the latter is a
//! two-stage resolve: fetch the record, parse it, then fetch its `media`.
//!
//! Non-audio, non-clip mimes (MIDI `text/vnd.abc`) are owned by `midi.rs` off
//! the same message stream; `RENDER_FLUSH_MIME` is consumed by BOTH sinks
//! independently (each keeps its own `MessageReader` cursor), exactly as
//! `midi.rs` already documents.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bevy::prelude::*;
use crossbeam_channel::{Receiver, Sender, unbounded};
use kaijutsu_audio::{
    ABC_MIME, CLIP_MIME, Clip, ClipError, CuePayload, PREPARE_MIME, REF_STALE_MAX,
    RENDER_FLUSH_MIME,
};
use kaijutsu_cas::ContentHash;
use kaijutsu_client::{CasResolver, ServerEvent, SftpClient, SftpError, SshConfig};
use tokio::sync::Mutex as AsyncMutex;

use crate::audio_sched::{self, AudioSchedulerHandle, DeadlineDecision, GRACE, decide_deadline, effective_deadline};
use crate::connection::actor_plugin::{RpcConnectionState, ServerEventMessage};

/// Bridges `ServerEvent::RenderCue` directives to the audio scheduler thread.
pub struct AudioOutPlugin;

impl Plugin for AudioOutPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(audio_sched::spawn())
            .insert_resource(CasPrefetch::new())
            .add_systems(Update, (play_render_cues, drain_prefetch_results));
    }
}

/// What a [`PrefetchOutcome`] represents once its CAS fetch lands — CAS
/// resolution happens in TWO different shapes depending on cue type: a
/// plain audio cue resolves straight to sample bytes, but a `Cas`-backed
/// clip record resolves the record JSON first and must dispatch a SECOND
/// fetch for its `media` before anything can play.
enum PrefetchKind {
    /// A plain `audio/*` cue — the resolved bytes ARE the sample. `stamped`
    /// is whether the originating `RenderCue` carried a non-zero `epoch_ns`
    /// (the crossing always stamps; `kj play`'s cues don't) — R4's
    /// skip-loud gate (`audio_sched::decide_deadline`) fires on it once the
    /// bytes land.
    Audio { deadline: Instant, stamped: bool },
    /// A clip record fetched from CAS (`CuePayload::Cas` + `CLIP_MIME`) —
    /// parse it, then dispatch a second prefetch for its `media` hash.
    /// `stamped` is carried through (not gated here — a record fetch running
    /// long isn't itself "playing" anything late) so the eventual
    /// `ClipMedia` dispatch inherits the right value.
    ClipRecord { deadline: Instant, stamped: bool },
    /// A clip's sample media resolved — ready to schedule with the record's
    /// trim/gain baked in. `stamped` gates R4's skip-loud decision the same
    /// way `Audio` does.
    ClipMedia {
        deadline: Instant,
        stamped: bool,
        src_offset: Option<Duration>,
        src_len: Option<Duration>,
        gain_db: f64,
    },
    /// R4 cache-warm (`PREPARE_MIME`, docs/pcm.md "The prepare horizon") — no
    /// deadline, nothing ever plays: the resolve itself (which writes the XDG
    /// cache as a side effect, `CasResolver::resolve`) IS the whole point.
    /// `started` is stamped at dispatch so the outcome can log end-to-end
    /// warm latency (connect + resolve, including any redial).
    Warm { started: Instant },
}

/// A resolved (or failed) CAS prefetch, tagged with the cue's mime and
/// [`PrefetchKind`], bridged from the SFTP runtime back onto the Bevy main
/// thread for [`drain_prefetch_results`].
struct PrefetchOutcome {
    mime: String,
    /// The object that was fetched — carried through so a skip-loud drop can
    /// name it in the log (`docs/pcm.md` R4: "log the underrun … naming the
    /// label-less hash" — no clip label rides this far down the pipeline,
    /// only the hash does).
    hash: ContentHash,
    kind: PrefetchKind,
    result: Result<Vec<u8>, String>,
}

/// The app's client-side CAS prefetch, owning the **Send** SFTP world.
///
/// The RPC actor's runtime is current-thread + `LocalSet` (Cap'n Proto is
/// `!Send`); SFTP futures are `Send` and must not ride that thread (a blocking
/// cache read would stall RPC). So this owns a *separate* tokio runtime, and the
/// resolver — its own SSH connection + XDG cache — lives entirely here. Results
/// cross back to Bevy on a crossbeam channel, drained each frame.
#[derive(Resource)]
pub struct CasPrefetch {
    /// Dedicated runtime for SFTP + cache IO, off the RPC actor's `!Send` world.
    rt: tokio::runtime::Runtime,
    /// The resolver, connected lazily on the first CAS cue and reused after
    /// (one SSH transport for the session). Cleared on a transport error so the
    /// next cue reconnects.
    resolver: Arc<AsyncMutex<Option<Arc<CasResolver<SftpClient>>>>>,
    tx: Sender<PrefetchOutcome>,
    rx: Receiver<PrefetchOutcome>,
}

impl CasPrefetch {
    fn new() -> Self {
        // One worker: prefetch is latency-tolerant (it runs under the prepare
        // horizon), and a single background thread keeps SFTP + the blocking
        // FileStore read off the render loop. (spawn_blocking for the cache IO
        // is the recorded follow-up, `docs/issues.md`.)
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("kaijutsu-cas-prefetch")
            .enable_all()
            .build()
            .expect("cas-prefetch tokio runtime");
        let (tx, rx) = unbounded();
        Self {
            rt,
            resolver: Arc::new(AsyncMutex::new(None)),
            tx,
            rx,
        }
    }

    /// Kick off an async resolve of `hash`; the outcome (tagged with `mime`
    /// and `kind`) lands on `rx` for [`drain_prefetch_results`]. Lazily
    /// connects the resolver on first use over `config` (the same SSH
    /// key/host the RPC channel used).
    fn dispatch(&self, hash: ContentHash, mime: String, config: SshConfig, kind: PrefetchKind) {
        let slot = self.resolver.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let result = resolve_with_lazy_connect(&slot, &hash, config)
                .await
                .map_err(|e| e.to_string());
            // A closed receiver just means the app is shutting down.
            let _ = tx.send(PrefetchOutcome {
                mime,
                hash,
                kind,
                result,
            });
        });
    }
}

/// A per-object resolve bound (docs/pcm.md R4, "Resolver transport
/// hardening") — a *transfer* bound, not the musical deadline (that gate is
/// `decide_deadline`/`effective_deadline` at delivery, R4 step 4). The live
/// failure this closes: an idle (~11 min) SFTP connection died silently;
/// dead-peer detection took ~70s, well past any reasonable "is this cue
/// still coming" patience. Generous on purpose — this must never trip on a
/// genuinely large, healthy transfer, only on a transport that has gone
/// silent.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Resolve `hash`, connecting the shared resolver on first use. A transport
/// failure OR a [`FETCH_TIMEOUT`] timeout (treated identically — a dead-but-
/// not-yet-RST'd transport looks exactly like a slow one from here) drops
/// the connection so it redials; a per-object failure (NotFound /
/// HashMismatch) leaves the healthy transport in place. Logs the happy path
/// too (`docs/issues.md` "Audio sink follow-ups") — hit/miss and timing, so a
/// live debugging session has something to read instead of silence.
async fn resolve_with_lazy_connect(
    slot: &AsyncMutex<Option<Arc<CasResolver<SftpClient>>>>,
    hash: &ContentHash,
    config: SshConfig,
) -> Result<Vec<u8>, SftpError> {
    // One reconnect-retry: a connection dropped while idle (server timeout) only
    // surfaces as a transport error on the *first* cue after the drop; retrying
    // once — after redialing — makes that flap invisible instead of skipping the
    // cue. A second transport failure is real, so return it.
    let mut redialed = false;
    loop {
        let resolver = get_or_connect(slot, &config).await?;
        let started = Instant::now();
        let outcome = match tokio::time::timeout(FETCH_TIMEOUT, resolver.resolve(hash)).await {
            Ok(inner) => inner,
            // A timeout is not a `SftpError` on its own — fold it into the
            // SAME "transport" bucket the redial logic below already
            // recognizes (`Ssh`/`Protocol`) rather than adding a third
            // never-redialed error class.
            Err(_elapsed) => Err(SftpError::Protocol(format!(
                "resolve timed out after {FETCH_TIMEOUT:?} — treating as a dead transport"
            ))),
        };
        match outcome {
            Ok((bytes, source)) => {
                info!(
                    "media resolved ({}) {} bytes in {}ms",
                    source.label(),
                    bytes.len(),
                    started.elapsed().as_millis()
                );
                return Ok(bytes);
            }
            Err(e) => {
                let transport = matches!(e, SftpError::Ssh(_) | SftpError::Protocol(_));
                if transport {
                    // Drop the dead transport so the next attempt/cue redials —
                    // but ONLY if the slot still holds the resolver that just
                    // failed. A concurrent cue may have already swapped in a
                    // fresh, healthy connection; clearing that blindly would
                    // thrash it.
                    reset_slot_if_same(slot, &resolver).await;
                }
                if transport && !redialed {
                    redialed = true;
                    // The live failure this closes: the previous single
                    // redial-retry was silent, so a placed clip just didn't
                    // sound with nothing in the log (`docs/issues.md`).
                    info!("sftp transport stale; redialing");
                    continue;
                }
                return Err(e);
            }
        }
    }
}

/// Get the shared resolver, connecting it on first use. Holding the slot lock
/// across `connect` also single-flights the dial: concurrent first cues wait,
/// then reuse the one connection.
async fn get_or_connect(
    slot: &AsyncMutex<Option<Arc<CasResolver<SftpClient>>>>,
    config: &SshConfig,
) -> Result<Arc<CasResolver<SftpClient>>, SftpError> {
    let mut guard = slot.lock().await;
    if guard.is_none() {
        let sftp = SftpClient::connect(config.clone()).await?;
        *guard = Some(Arc::new(CasResolver::with_xdg_cache(sftp)));
    }
    Ok(guard.as_ref().expect("resolver present").clone())
}

/// Clear the slot only if it still holds `failed` — so a concurrent cue's fresh
/// reconnection is never wiped by a late loser's error handler (the transport
/// slot-clearing race).
async fn reset_slot_if_same(
    slot: &AsyncMutex<Option<Arc<CasResolver<SftpClient>>>>,
    failed: &Arc<CasResolver<SftpClient>>,
) {
    let mut guard = slot.lock().await;
    if guard.as_ref().is_some_and(|cur| Arc::ptr_eq(cur, failed)) {
        *guard = None;
    }
}

/// `clip.src_offset_ms == 0` maps to `None` rather than `Some(Duration::ZERO)`
/// — functionally identical (`skip_duration(ZERO)` is a no-op) but takes
/// `build_source`'s no-trim fast path instead of a needless wrapper.
fn clip_source_offset(clip: &Clip) -> Option<Duration> {
    (clip.src_offset_ms != 0).then(|| Duration::from_millis(clip.src_offset_ms))
}

/// Pure: parse a clip record and map its fields into what should be
/// dispatched next — the `media` hash/mime to fetch, plus the
/// [`PrefetchKind::ClipMedia`] carrying its baked trim/gain and the
/// (already-backdated) `deadline`. Split out from the dispatch call so
/// parsing + field-mapping is unit-testable with no connection, no CAS, no
/// async machinery at all. `stamped` just rides along into the returned
/// `PrefetchKind` — see `PrefetchKind::ClipMedia`'s doc.
fn clip_media_fetch(
    json: &str,
    deadline: Instant,
    stamped: bool,
) -> Result<(ContentHash, String, PrefetchKind), ClipError> {
    let clip = Clip::parse(json)?;
    Ok((
        clip.media.clone(),
        clip.mime.clone(),
        PrefetchKind::ClipMedia {
            deadline,
            stamped,
            src_offset: clip_source_offset(&clip),
            src_len: clip.src_len_ms.map(Duration::from_millis),
            gain_db: clip.gain_db,
        },
    ))
}

/// Parse a clip record JSON and, given a live connection, dispatch the fetch
/// for its `media`. A parse failure is loud (the kernel already ran
/// `parse_validated` at commit — a bad record reaching the sink is still
/// worth a warning, never a silent drop or a panic). No connection is the
/// same "arrived before we could fetch" edge the plain audio CAS path
/// already has.
fn dispatch_clip_media(
    json: &str,
    deadline: Instant,
    stamped: bool,
    prefetch: &CasPrefetch,
    connection: Option<&RpcConnectionState>,
) {
    let (media, mime, kind) = match clip_media_fetch(json, deadline, stamped) {
        Ok(v) => v,
        Err(e) => {
            warn!("clip record failed to parse; skipping (loud, not silent): {e}");
            return;
        }
    };
    match connection {
        Some(conn) if conn.connected => {
            prefetch.dispatch(media, mime, conn.ssh_config.clone(), kind)
        }
        _ => {
            warn!("clip record parsed but no live connection to fetch its media (media={media:?})")
        }
    }
}

/// A clip cue's payload is either the record JSON inline (the usual case —
/// the record itself is small) or a CAS hash of the record (rarer, but
/// supported): resolve accordingly.
fn handle_clip_cue(
    payload: &CuePayload,
    deadline: Instant,
    stamped: bool,
    prefetch: &CasPrefetch,
    connection: Option<&RpcConnectionState>,
) {
    match payload {
        CuePayload::Inline(bytes) => match std::str::from_utf8(bytes) {
            Ok(json) => dispatch_clip_media(json, deadline, stamped, prefetch, connection),
            Err(_) => warn!("clip cue payload was not UTF-8; skipping"),
        },
        CuePayload::Cas(hash) => match connection {
            Some(conn) if conn.connected => {
                prefetch.dispatch(
                    hash.clone(),
                    CLIP_MIME.to_string(),
                    conn.ssh_config.clone(),
                    PrefetchKind::ClipRecord { deadline, stamped },
                );
            }
            _ => warn!(
                "CAS clip render cue arrived before a live connection — cannot fetch the record \
                 (hash={hash:?})"
            ),
        },
    }
}

/// Consume `RenderCue` directives: flush the scheduler on a transport
/// stop/pause, dispatch `audio/*` and `CLIP_MIME` cues (inline or CAS) to it
/// with their backdated deadline, and leave everything else (ABC) to
/// `midi.rs`'s own cursor on the same message stream.
///
/// `Instant::now()`/`SystemTime::now()` are read ONCE per batch, above the
/// loop — mirrors `midi.rs::play_midi_cues`'s discipline so several cues
/// buffered into one frame age against the SAME receipt instant rather than
/// one drifting per cue as the loop runs.
fn play_render_cues(
    mut messages: MessageReader<ServerEventMessage>,
    scheduler: Res<AudioSchedulerHandle>,
    prefetch: Res<CasPrefetch>,
    connection: Option<Res<RpcConnectionState>>,
) {
    let now = Instant::now();
    let now_epoch_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);

    for ServerEventMessage(event) in messages.read() {
        let ServerEvent::RenderCue { cue, .. } = event else {
            continue;
        };

        if cue.mime == RENDER_FLUSH_MIME {
            scheduler.flush();
            continue;
        }

        if cue.mime == PREPARE_MIME {
            // Not gated by staleness — `PREPARE_MIME`'s own contract
            // (`kaijutsu-audio::lib.rs`) says a sink never rejects a prepare
            // cue as stale; the cache is worth warming even if the cue is
            // "old" by the time it's processed.
            match &cue.payload {
                CuePayload::Cas(hash) => match connection.as_deref() {
                    Some(conn) if conn.connected => {
                        prefetch.dispatch(
                            hash.clone(),
                            cue.mime.clone(),
                            conn.ssh_config.clone(),
                            PrefetchKind::Warm { started: now },
                        );
                    }
                    _ => warn!(
                        "prepare cue arrived before a live connection — cannot warm the cache \
                         (hash={hash:?})"
                    ),
                },
                CuePayload::Inline(_) => {
                    // A prepare cue's whole point is naming a CAS object to
                    // warm ahead of time — an inline payload has nothing to
                    // warm and is a producer bug, not a sink concern to hide.
                    warn!(
                        "prepare cue carried an Inline payload — protocol misuse ({PREPARE_MIME} \
                         must be CuePayload::Cas); skipping"
                    );
                }
            }
            continue;
        }

        if cue.mime == CLIP_MIME {
            let stamped = cue.epoch_ns != 0;
            match effective_deadline(now, cue.lead, cue.epoch_ns, now_epoch_ns) {
                Some(deadline) => handle_clip_cue(
                    &cue.payload,
                    deadline,
                    stamped,
                    &prefetch,
                    connection.as_deref(),
                ),
                None => {
                    kaijutsu_telemetry::record_stale_cue_dropped();
                    warn!(
                        "clip render cue rejected — stale beyond {REF_STALE_MAX:?}; dropping \
                         rather than fire arbitrarily late"
                    );
                }
            }
            continue;
        }

        if !cue.mime.starts_with("audio/") {
            // ABC is midi.rs's own mime, read off its own cursor on the same
            // stream. Anything else Cas-backed and genuinely unrecognized by
            // any known sink stays loud (the bytes would otherwise vanish
            // with no trace) — an unrecognized Inline mime is assumed to be
            // a foreign/future symbolic payload no sink here needs to touch.
            if cue.mime != ABC_MIME {
                if let CuePayload::Cas(hash) = &cue.payload {
                    warn!(
                        "CAS render cue with unrecognized mime not handled by any known sink \
                         (hash={hash:?}, mime={})",
                        cue.mime
                    );
                }
            }
            continue;
        }

        match &cue.payload {
            CuePayload::Inline(bytes) => {
                if cue.lead.is_zero() && cue.epoch_ns == 0 {
                    // True play-now parity: skip the deadline math entirely.
                    scheduler.play_now(bytes.clone());
                    continue;
                }
                match effective_deadline(now, cue.lead, cue.epoch_ns, now_epoch_ns) {
                    Some(deadline) => scheduler.play_at(bytes.clone(), deadline, None, None, 0.0),
                    None => {
                        kaijutsu_telemetry::record_stale_cue_dropped();
                        warn!(
                            "audio render cue rejected — stale beyond {REF_STALE_MAX:?}; dropping \
                             rather than play arbitrarily late (mime={})",
                            cue.mime
                        );
                    }
                }
            }
            CuePayload::Cas(hash) => {
                match effective_deadline(now, cue.lead, cue.epoch_ns, now_epoch_ns) {
                    Some(deadline) => match connection.as_deref() {
                        Some(conn) if conn.connected => {
                            prefetch.dispatch(
                                hash.clone(),
                                cue.mime.clone(),
                                conn.ssh_config.clone(),
                                PrefetchKind::Audio {
                                    deadline,
                                    stamped: cue.epoch_ns != 0,
                                },
                            );
                        }
                        _ => warn!(
                            "CAS render cue arrived before a live connection — cannot prefetch \
                         (hash={hash:?}, mime={})",
                            cue.mime
                        ),
                    },
                    None => {
                        kaijutsu_telemetry::record_stale_cue_dropped();
                        warn!(
                            "CAS audio render cue rejected — stale beyond {REF_STALE_MAX:?}; not even \
                         dispatching the fetch (hash={hash:?}, mime={})",
                            cue.mime
                        );
                    }
                }
            }
        }
    }
}

/// Act on prefetched CAS objects as they resolve — the async tail of the CAS
/// branches in [`play_render_cues`]. Runs on the Bevy main thread, so the
/// off-thread resolve never touches scheduler dispatch directly.
///
/// R4's skip-loud gate (`audio_sched::decide_deadline`) is applied HERE,
/// right before the two places that would otherwise call
/// `scheduler.play_at` — a `DropLate` decision means nothing reaches the
/// scheduler at all, never a late `PlayAt`.
fn drain_prefetch_results(
    prefetch: Res<CasPrefetch>,
    scheduler: Res<AudioSchedulerHandle>,
    connection: Option<Res<RpcConnectionState>>,
) {
    while let Ok(outcome) = prefetch.rx.try_recv() {
        match outcome.kind {
            PrefetchKind::Audio { deadline, stamped } => match outcome.result {
                Ok(bytes) => match decide_deadline(deadline, stamped, Instant::now()) {
                    DeadlineDecision::Fire => scheduler.play_at(bytes, deadline, None, None, 0.0),
                    DeadlineDecision::DropLate { late_by } => {
                        kaijutsu_telemetry::record_stale_cue_dropped();
                        warn!(
                            "audio media landed too late — dropping rather than firing stale \
                             (hash={:?}, {} bytes, {}ms past deadline, grace is {:?})",
                            outcome.hash,
                            bytes.len(),
                            late_by.as_millis(),
                            GRACE
                        );
                    }
                },
                Err(e) => warn!("CAS prefetch failed (mime={}): {e}", outcome.mime),
            },
            PrefetchKind::ClipRecord { deadline, stamped } => match outcome.result {
                Ok(bytes) => match std::str::from_utf8(&bytes) {
                    Ok(json) => dispatch_clip_media(
                        json,
                        deadline,
                        stamped,
                        &prefetch,
                        connection.as_deref(),
                    ),
                    Err(_) => warn!("CAS clip record was not UTF-8; skipping"),
                },
                Err(e) => warn!("CAS clip record prefetch failed: {e}"),
            },
            PrefetchKind::ClipMedia {
                deadline,
                stamped,
                src_offset,
                src_len,
                gain_db,
            } => match outcome.result {
                Ok(bytes) => match decide_deadline(deadline, stamped, Instant::now()) {
                    DeadlineDecision::Fire => {
                        scheduler.play_at(bytes, deadline, src_offset, src_len, gain_db)
                    }
                    DeadlineDecision::DropLate { late_by } => {
                        kaijutsu_telemetry::record_stale_cue_dropped();
                        warn!(
                            "clip media landed too late — dropping rather than firing stale \
                             (hash={:?}, {} bytes, {}ms past deadline, grace is {:?})",
                            outcome.hash,
                            bytes.len(),
                            late_by.as_millis(),
                            GRACE
                        );
                    }
                },
                Err(e) => warn!(
                    "clip media not resolved from CAS; skipping (mime={}): {e}",
                    outcome.mime
                ),
            },
            PrefetchKind::Warm { started } => match outcome.result {
                // The XDG cache write already happened INSIDE the resolve
                // (`CasResolver::resolve`/`fetch_verify_store` stores every
                // verified fetch) — there is nothing left to do with `bytes`
                // here but report that the warm landed.
                Ok(bytes) => info!(
                    "media warmed: {} bytes in {}ms",
                    bytes.len(),
                    started.elapsed().as_millis()
                ),
                Err(e) => warn!("media warm failed (mime={}): {e}", outcome.mime),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_sched::SchedulerCmd;
    use kaijutsu_audio::RenderCue;
    use kaijutsu_cas::ContentHash;
    use kaijutsu_types::ContextId;
    use std::str::FromStr;

    /// Minimal headless app: just enough to run the sink systems with no
    /// real audio device — the scheduler resource is the TEST channel handle
    /// (`audio_sched::test_handle`), never the real rodio thread, so these
    /// tests assert on what got SENT rather than what got heard.
    fn test_app() -> (App, Receiver<SchedulerCmd>) {
        let (scheduler, rx) = audio_sched::test_handle();
        let mut app = App::new();
        app.add_message::<ServerEventMessage>()
            .insert_resource(scheduler)
            .insert_resource(CasPrefetch::new())
            .add_systems(Update, (play_render_cues, drain_prefetch_results));
        (app, rx)
    }

    fn play(app: &mut App, cue: RenderCue) {
        app.world_mut()
            .write_message(ServerEventMessage(ServerEvent::RenderCue {
                context_id: ContextId::new(),
                cue,
            }));
        app.update();
    }

    /// Push a prefetch outcome straight onto the channel (same-module access) —
    /// stands in for the off-thread resolve completing, so the drain system is
    /// testable without a live SFTP server. `hash` is arbitrary — none of
    /// these tests care WHICH object resolved, only that the outcome carries
    /// one to name in a skip-loud log line.
    fn deliver(app: &mut App, mime: &str, kind: PrefetchKind, result: Result<Vec<u8>, String>) {
        app.world()
            .resource::<CasPrefetch>()
            .tx
            .send(PrefetchOutcome {
                mime: mime.to_string(),
                hash: ContentHash::from_data(b"test-object"),
                kind,
                result,
            })
            .unwrap();
        app.update();
    }

    const TEST_WAV: &str = "/home/atobey/src/pawlsa-mcp/pawlsa-test.wav";

    fn test_wav_bytes() -> Option<Vec<u8>> {
        std::fs::read(TEST_WAV).ok()
    }

    // ── Real WAV bytes, play-now / scheduled dispatch ─────────────────────

    /// A zero-lead, unstamped inline `audio/wav` cue is the play-now fast
    /// path — parity with the pre-rodio `AudioPlayer` spawn: exactly one
    /// `PlayNow` with the untouched bytes.
    #[test]
    fn inline_audio_cue_zero_lead_sends_one_play_now() {
        let Some(bytes) = test_wav_bytes() else {
            eprintln!("skipping: {TEST_WAV} not present on this machine");
            return;
        };
        let (mut app, rx) = test_app();
        play(&mut app, RenderCue::now_inline("audio/wav", bytes.clone()));

        match rx.try_recv().expect("one command sent") {
            SchedulerCmd::PlayNow { bytes: sent } => assert_eq!(sent, bytes),
            other => panic!("expected PlayNow, got {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "exactly one command");
    }

    /// R5: a non-zero lead is now honored for ALL audio cues (not just
    /// zero-lead) — an unstamped cue with `lead=500ms` schedules a `PlayAt`
    /// roughly 500ms out, not a `PlayNow`.
    #[test]
    fn inline_audio_cue_with_lead_sends_a_scheduled_play_at() {
        let Some(bytes) = test_wav_bytes() else {
            eprintln!("skipping: {TEST_WAV} not present on this machine");
            return;
        };
        let (mut app, rx) = test_app();
        let before = Instant::now();
        play(
            &mut app,
            RenderCue {
                mime: "audio/wav".into(),
                payload: CuePayload::Inline(bytes.clone()),
                lead: Duration::from_millis(500),
                epoch_ns: 0, // unstamped: lead honored at face value
            },
        );
        let after = Instant::now();

        match rx.try_recv().expect("one command sent") {
            SchedulerCmd::PlayAt {
                bytes: sent,
                deadline,
                src_offset,
                src_len,
                gain_db,
            } => {
                assert_eq!(sent, bytes);
                assert!(src_offset.is_none() && src_len.is_none() && gain_db == 0.0);
                assert!(deadline >= before + Duration::from_millis(500));
                assert!(deadline <= after + Duration::from_millis(500));
            }
            other => panic!("expected PlayAt, got {other:?}"),
        }
    }

    /// A cue stamped stale beyond `REF_STALE_MAX` is rejected outright —
    /// never fired late, never even reaching the scheduler.
    #[test]
    fn a_stale_audio_cue_is_rejected_outright() {
        let Some(bytes) = test_wav_bytes() else {
            eprintln!("skipping: {TEST_WAV} not present on this machine");
            return;
        };
        let (mut app, rx) = test_app();
        let now_epoch_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        let stale_epoch_ns =
            now_epoch_ns.saturating_sub((REF_STALE_MAX + Duration::from_secs(2)).as_nanos() as u64);
        play(
            &mut app,
            RenderCue {
                mime: "audio/wav".into(),
                payload: CuePayload::Inline(bytes),
                lead: Duration::from_millis(50),
                epoch_ns: stale_epoch_ns,
            },
        );
        assert!(
            rx.try_recv().is_err(),
            "a too-stale cue must not reach the scheduler"
        );
    }

    /// A CAS audio cue with no connection can't prefetch — it warns and sends
    /// nothing to the scheduler this frame (the resolve is off-thread and
    /// needs an SSH config).
    #[test]
    fn cas_audio_cue_without_connection_produces_nothing() {
        let (mut app, rx) = test_app();
        play(
            &mut app,
            RenderCue {
                mime: "audio/wav".into(),
                payload: CuePayload::Cas(
                    ContentHash::from_str("00000000000000000000000000000000").unwrap(),
                ),
                lead: Duration::ZERO,
                epoch_ns: 0,
            },
        );
        assert!(rx.try_recv().is_err());
    }

    /// The async tail: a resolved `audio/*` object delivered on the channel
    /// schedules exactly one `PlayAt` at the deadline snapshotted at receipt.
    #[test]
    fn a_resolved_audio_object_is_scheduled() {
        let (mut app, rx) = test_app();
        let deadline = Instant::now();
        deliver(
            &mut app,
            "audio/wav",
            PrefetchKind::Audio {
                deadline,
                stamped: true,
            },
            Ok(vec![1, 2, 3, 4]),
        );

        match rx.try_recv().expect("one command sent") {
            SchedulerCmd::PlayAt {
                bytes,
                deadline: d,
                src_offset,
                src_len,
                gain_db,
            } => {
                assert_eq!(bytes, vec![1, 2, 3, 4]);
                assert_eq!(d, deadline);
                assert!(src_offset.is_none() && src_len.is_none() && gain_db == 0.0);
            }
            other => panic!("expected PlayAt, got {other:?}"),
        }
    }

    /// A failed prefetch is a loud no-op — nothing sent to the scheduler.
    #[test]
    fn a_failed_audio_prefetch_schedules_nothing() {
        let (mut app, rx) = test_app();
        deliver(
            &mut app,
            "audio/wav",
            PrefetchKind::Audio {
                deadline: Instant::now(),
                stamped: true,
            },
            Err(format!(
                "no such path: {}/…",
                kaijutsu_types::paths::CAS_ROOT
            )),
        );
        assert!(rx.try_recv().is_err());
    }

    // ── R4: skip-loud on a stale CAS fetch, integration through drain ──────

    /// A stamped (musically-placed) audio cue whose media lands more than
    /// `GRACE` past its deadline is dropped — NOTHING reaches the scheduler,
    /// not even a late `PlayAt` (closes R5's carried interim behavior,
    /// docs/pcm.md R4).
    #[test]
    fn a_stamped_audio_object_landing_past_grace_is_dropped_not_scheduled() {
        let (mut app, rx) = test_app();
        let deadline = Instant::now() - (GRACE + Duration::from_millis(50));
        deliver(
            &mut app,
            "audio/wav",
            PrefetchKind::Audio {
                deadline,
                stamped: true,
            },
            Ok(vec![1, 2, 3, 4]),
        );
        assert!(
            rx.try_recv().is_err(),
            "a stamped cue landing past GRACE must never reach the scheduler"
        );
    }

    /// An UNSTAMPED audio cue (asap semantics, e.g. `kj play --cas`) still
    /// fires even when its media lands wildly late — there is no musical
    /// placement to violate, so R4's skip-loud gate must never touch it.
    #[test]
    fn an_unstamped_audio_object_landing_late_still_schedules() {
        let (mut app, rx) = test_app();
        let deadline = Instant::now() - Duration::from_secs(5);
        deliver(
            &mut app,
            "audio/wav",
            PrefetchKind::Audio {
                deadline,
                stamped: false,
            },
            Ok(vec![1, 2, 3, 4]),
        );
        match rx.try_recv().expect("unstamped cues still fire however late") {
            SchedulerCmd::PlayAt { bytes, .. } => assert_eq!(bytes, vec![1, 2, 3, 4]),
            other => panic!("expected PlayAt, got {other:?}"),
        }
    }

    /// The same skip-loud gate applies to a clip's resolved media, not just
    /// plain audio — a stamped clip cue landing past grace is dropped, never
    /// scheduled with its trim/gain applied.
    #[test]
    fn a_stamped_clip_media_object_landing_past_grace_is_dropped_not_scheduled() {
        let (mut app, rx) = test_app();
        let deadline = Instant::now() - (GRACE + Duration::from_millis(50));
        deliver(
            &mut app,
            "audio/wav",
            PrefetchKind::ClipMedia {
                deadline,
                stamped: true,
                src_offset: None,
                src_len: None,
                gain_db: 0.0,
            },
            Ok(vec![9, 9, 9]),
        );
        assert!(
            rx.try_recv().is_err(),
            "a stamped clip media landing past GRACE must never reach the scheduler"
        );
    }

    /// A transport flush cue reaches the scheduler as `Flush` — wired to the
    /// same `RENDER_FLUSH_MIME` midi.rs consumes off its own cursor.
    #[test]
    fn a_flush_cue_sends_flush_to_the_scheduler() {
        let (mut app, rx) = test_app();
        play(
            &mut app,
            RenderCue::now_inline(RENDER_FLUSH_MIME, Vec::new()),
        );
        assert!(matches!(rx.try_recv(), Ok(SchedulerCmd::Flush)));
    }

    // ── R4: the prepare-cue cache warm (PREPARE_MIME) ──────────────────────

    /// A `PREPARE_MIME` cue with no live connection can't dispatch a warm —
    /// it warns and touches neither the scheduler nor the internal prefetch
    /// channel (mirrors the plain CAS-audio "no connection" edge).
    #[test]
    fn prepare_cue_with_no_connection_warns_and_dispatches_nothing() {
        let (mut app, rx) = test_app();
        play(
            &mut app,
            RenderCue {
                mime: PREPARE_MIME.into(),
                payload: CuePayload::Cas(ContentHash::from_data(b"warm-me")),
                lead: Duration::ZERO,
                epoch_ns: 0,
            },
        );
        assert!(rx.try_recv().is_err(), "nothing ever reaches the scheduler for a prepare cue");
        assert!(
            app.world().resource::<CasPrefetch>().rx.try_recv().is_err(),
            "no connection means no dispatch was ever made"
        );
    }

    /// An `Inline` payload under `PREPARE_MIME` is a protocol misuse (the
    /// whole point of a prepare cue is naming a CAS object to warm) — warn
    /// and skip, never dispatch, never panic.
    #[test]
    fn inline_prepare_cue_is_a_protocol_misuse_and_dispatches_nothing() {
        let (mut app, rx) = test_app();
        play(
            &mut app,
            RenderCue::now_inline(PREPARE_MIME, vec![1, 2, 3]),
        );
        assert!(rx.try_recv().is_err());
        assert!(
            app.world().resource::<CasPrefetch>().rx.try_recv().is_err(),
            "an Inline prepare payload must never dispatch a fetch"
        );
    }

    /// The async tail of a successful warm: nothing is ever scheduled — the
    /// resolve's side effect (writing the XDG cache) already happened inside
    /// `CasResolver::resolve`, so `drain_prefetch_results` has nothing left
    /// to do but report it landed.
    #[test]
    fn a_resolved_warm_prefetch_schedules_nothing() {
        let (mut app, rx) = test_app();
        deliver(
            &mut app,
            PREPARE_MIME,
            PrefetchKind::Warm {
                started: Instant::now(),
            },
            Ok(vec![1, 2, 3, 4]),
        );
        assert!(rx.try_recv().is_err(), "a warm never plays or schedules anything");
    }

    /// A failed warm is a loud no-op, same as a failed audio/clip prefetch —
    /// nothing sent to the scheduler.
    #[test]
    fn a_failed_warm_prefetch_schedules_nothing() {
        let (mut app, rx) = test_app();
        deliver(
            &mut app,
            PREPARE_MIME,
            PrefetchKind::Warm {
                started: Instant::now(),
            },
            Err("transport died mid-warm".to_string()),
        );
        assert!(rx.try_recv().is_err());
    }

    // ── The clip renderer (R1): flips from "asserts-skip" to "asserts-scheduled" ─

    fn clip_json(media: ContentHash) -> String {
        format!(r#"{{"v":1,"media":"{media}","mime":"audio/wav","label":"rimshot"}}"#)
    }

    /// An inline clip cue with a live-but-absent connection resource can't
    /// fetch its media — it warns and sends nothing, mirroring the plain
    /// CAS-audio "no connection" edge.
    #[test]
    fn inline_clip_cue_without_connection_warns_and_schedules_nothing() {
        let (mut app, rx) = test_app();
        let json = clip_json(ContentHash::from_data(b"rimshot"));
        play(
            &mut app,
            RenderCue::now_inline(CLIP_MIME, json.into_bytes()),
        );
        assert!(
            rx.try_recv().is_err(),
            "no connection means no fetch means nothing scheduled"
        );
    }

    /// A structurally invalid clip record (the old test's bare `{}`) is
    /// rejected loud at parse — before any connection check, before any
    /// fetch — never a panic, never a silent drop.
    #[test]
    fn invalid_inline_clip_record_is_rejected_loud() {
        let (mut app, rx) = test_app();
        play(&mut app, RenderCue::now_inline(CLIP_MIME, b"{}".to_vec()));
        assert!(rx.try_recv().is_err());
    }

    /// R1's concrete proof: once a clip's media resolves, it schedules a
    /// `PlayAt` with the record's source-range trim and gain applied — this
    /// is the test that used to assert "skipped," now asserting "scheduled."
    #[test]
    fn a_resolved_clip_media_object_is_scheduled_with_trim_and_gain() {
        let (mut app, rx) = test_app();
        let deadline = Instant::now();
        deliver(
            &mut app,
            "audio/wav",
            PrefetchKind::ClipMedia {
                deadline,
                stamped: true,
                src_offset: Some(Duration::from_millis(100)),
                src_len: Some(Duration::from_millis(500)),
                gain_db: -6.0,
            },
            Ok(vec![9, 9, 9]),
        );

        match rx.try_recv().expect("one command sent") {
            SchedulerCmd::PlayAt {
                bytes,
                deadline: d,
                src_offset,
                src_len,
                gain_db,
            } => {
                assert_eq!(bytes, vec![9, 9, 9]);
                assert_eq!(d, deadline);
                assert_eq!(src_offset, Some(Duration::from_millis(100)));
                assert_eq!(src_len, Some(Duration::from_millis(500)));
                assert_eq!(gain_db, -6.0);
            }
            other => panic!("expected PlayAt, got {other:?}"),
        }
    }

    /// A stale clip cue is rejected outright, same as a stale audio cue —
    /// never even attempting to parse/fetch.
    #[test]
    fn a_stale_clip_cue_is_rejected_outright() {
        let (mut app, rx) = test_app();
        let now_epoch_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        let stale_epoch_ns =
            now_epoch_ns.saturating_sub((REF_STALE_MAX + Duration::from_secs(2)).as_nanos() as u64);
        let json = clip_json(ContentHash::from_data(b"snare"));
        play(
            &mut app,
            RenderCue {
                mime: CLIP_MIME.into(),
                payload: CuePayload::Inline(json.into_bytes()),
                lead: Duration::from_millis(50),
                epoch_ns: stale_epoch_ns,
            },
        );
        assert!(rx.try_recv().is_err());
    }

    // ── clip_media_fetch: pure parsing/field-mapping, no connection at all ─

    #[test]
    fn clip_media_fetch_maps_the_record_fields_into_a_clip_media_prefetch() {
        let media = ContentHash::from_data(b"rimshot");
        let json = format!(
            r#"{{"v":1,"media":"{media}","mime":"audio/wav","label":"rimshot","src_offset_ms":100,"src_len_ms":500,"gain_db":-6.0}}"#
        );
        let deadline = Instant::now() + Duration::from_millis(50);
        let (got_media, got_mime, kind) =
            clip_media_fetch(&json, deadline, true).expect("valid record");
        assert_eq!(got_media, media);
        assert_eq!(got_mime, "audio/wav");
        match kind {
            PrefetchKind::ClipMedia {
                deadline: d,
                stamped,
                src_offset,
                src_len,
                gain_db,
            } => {
                assert_eq!(d, deadline);
                assert!(stamped, "stamped rides straight through from the caller");
                assert_eq!(src_offset, Some(Duration::from_millis(100)));
                assert_eq!(src_len, Some(Duration::from_millis(500)));
                assert_eq!(gain_db, -6.0);
            }
            _ => panic!("expected ClipMedia"),
        }
    }

    #[test]
    fn clip_media_fetch_defaults_zero_offset_to_none() {
        let media = ContentHash::from_data(b"kick");
        let json = format!(r#"{{"v":1,"media":"{media}","mime":"audio/wav","label":"kick"}}"#);
        let (_, _, kind) =
            clip_media_fetch(&json, Instant::now(), false).expect("valid minimal record");
        match kind {
            PrefetchKind::ClipMedia {
                stamped,
                src_offset,
                src_len,
                gain_db,
                ..
            } => {
                assert!(!stamped, "stamped rides straight through from the caller");
                assert_eq!(
                    src_offset, None,
                    "zero offset maps to None, build_source's fast path"
                );
                assert_eq!(src_len, None);
                assert_eq!(gain_db, 0.0);
            }
            _ => panic!("expected ClipMedia"),
        }
    }

    #[test]
    fn clip_media_fetch_rejects_an_invalid_record() {
        assert!(clip_media_fetch("{}", Instant::now(), true).is_err());
    }
}
