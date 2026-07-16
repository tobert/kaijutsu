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
    ABC_MIME, CLIP_MIME, Clip, ClipError, CuePayload, REF_STALE_MAX, RENDER_FLUSH_MIME,
};
use kaijutsu_cas::ContentHash;
use kaijutsu_client::{CasResolver, ServerEvent, SftpClient, SftpError, SshConfig};
use tokio::sync::Mutex as AsyncMutex;

use crate::audio_sched::{self, AudioSchedulerHandle, effective_deadline};
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
    /// A plain `audio/*` cue — the resolved bytes ARE the sample.
    Audio { deadline: Instant },
    /// A clip record fetched from CAS (`CuePayload::Cas` + `CLIP_MIME`) —
    /// parse it, then dispatch a second prefetch for its `media` hash.
    ClipRecord { deadline: Instant },
    /// A clip's sample media resolved — ready to schedule with the record's
    /// trim/gain baked in.
    ClipMedia {
        deadline: Instant,
        src_offset: Option<Duration>,
        src_len: Option<Duration>,
        gain_db: f64,
    },
}

/// A resolved (or failed) CAS prefetch, tagged with the cue's mime and
/// [`PrefetchKind`], bridged from the SFTP runtime back onto the Bevy main
/// thread for [`drain_prefetch_results`].
struct PrefetchOutcome {
    mime: String,
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
            let _ = tx.send(PrefetchOutcome { mime, kind, result });
        });
    }
}

/// Resolve `hash`, connecting the shared resolver on first use. A transport
/// failure drops the connection so it redials; a per-object failure (NotFound /
/// HashMismatch) leaves the healthy transport in place.
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
        match resolver.resolve(hash).await {
            Ok(bytes) => return Ok(bytes),
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
/// async machinery at all.
fn clip_media_fetch(
    json: &str,
    deadline: Instant,
) -> Result<(ContentHash, String, PrefetchKind), ClipError> {
    let clip = Clip::parse(json)?;
    Ok((
        clip.media.clone(),
        clip.mime.clone(),
        PrefetchKind::ClipMedia {
            deadline,
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
    prefetch: &CasPrefetch,
    connection: Option<&RpcConnectionState>,
) {
    let (media, mime, kind) = match clip_media_fetch(json, deadline) {
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
    prefetch: &CasPrefetch,
    connection: Option<&RpcConnectionState>,
) {
    match payload {
        CuePayload::Inline(bytes) => match std::str::from_utf8(bytes) {
            Ok(json) => dispatch_clip_media(json, deadline, prefetch, connection),
            Err(_) => warn!("clip cue payload was not UTF-8; skipping"),
        },
        CuePayload::Cas(hash) => match connection {
            Some(conn) if conn.connected => {
                prefetch.dispatch(
                    hash.clone(),
                    CLIP_MIME.to_string(),
                    conn.ssh_config.clone(),
                    PrefetchKind::ClipRecord { deadline },
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

        if cue.mime == CLIP_MIME {
            match effective_deadline(now, cue.lead, cue.epoch_ns, now_epoch_ns) {
                Some(deadline) => {
                    handle_clip_cue(&cue.payload, deadline, &prefetch, connection.as_deref())
                }
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
                                PrefetchKind::Audio { deadline },
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
fn drain_prefetch_results(
    prefetch: Res<CasPrefetch>,
    scheduler: Res<AudioSchedulerHandle>,
    connection: Option<Res<RpcConnectionState>>,
) {
    while let Ok(outcome) = prefetch.rx.try_recv() {
        match outcome.kind {
            PrefetchKind::Audio { deadline } => match outcome.result {
                Ok(bytes) => scheduler.play_at(bytes, deadline, None, None, 0.0),
                Err(e) => warn!("CAS prefetch failed (mime={}): {e}", outcome.mime),
            },
            PrefetchKind::ClipRecord { deadline } => match outcome.result {
                Ok(bytes) => match std::str::from_utf8(&bytes) {
                    Ok(json) => {
                        dispatch_clip_media(json, deadline, &prefetch, connection.as_deref())
                    }
                    Err(_) => warn!("CAS clip record was not UTF-8; skipping"),
                },
                Err(e) => warn!("CAS clip record prefetch failed: {e}"),
            },
            PrefetchKind::ClipMedia {
                deadline,
                src_offset,
                src_len,
                gain_db,
            } => match outcome.result {
                Ok(bytes) => scheduler.play_at(bytes, deadline, src_offset, src_len, gain_db),
                Err(e) => warn!(
                    "clip media not resolved from CAS; skipping (mime={}): {e}",
                    outcome.mime
                ),
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
    /// testable without a live SFTP server.
    fn deliver(app: &mut App, mime: &str, kind: PrefetchKind, result: Result<Vec<u8>, String>) {
        app.world()
            .resource::<CasPrefetch>()
            .tx
            .send(PrefetchOutcome {
                mime: mime.to_string(),
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
            PrefetchKind::Audio { deadline },
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
            },
            Err(format!(
                "no such path: {}/…",
                kaijutsu_types::paths::CAS_ROOT
            )),
        );
        assert!(rx.try_recv().is_err());
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
        let (got_media, got_mime, kind) = clip_media_fetch(&json, deadline).expect("valid record");
        assert_eq!(got_media, media);
        assert_eq!(got_mime, "audio/wav");
        match kind {
            PrefetchKind::ClipMedia {
                deadline: d,
                src_offset,
                src_len,
                gain_db,
            } => {
                assert_eq!(d, deadline);
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
        let (_, _, kind) = clip_media_fetch(&json, Instant::now()).expect("valid minimal record");
        match kind {
            PrefetchKind::ClipMedia {
                src_offset,
                src_len,
                gain_db,
                ..
            } => {
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
        assert!(clip_media_fetch("{}", Instant::now()).is_err());
    }
}
