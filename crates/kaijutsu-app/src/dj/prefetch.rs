//! CAS prefetch — the DJ thread's own **Send** SFTP world (`docs/midi.md`
//! "The DJ thread": "CAS prefetch dispatch (that runtime moves in
//! wholesale)"). Ported from `audio.rs::CasPrefetch` (pre-Task-#3, now
//! deleted) with ONE structural change: the outcome channel is a
//! [`tokio::sync::mpsc`] unbounded pair instead of a `crossbeam_channel` —
//! `crossbeam`'s blocking `recv` cannot ride `tokio::select!`, and the whole
//! point of this task is giving the DJ thread's `select!` a native async arm
//! for [`PrefetchOutcome`] (`dj::thread::run_loop`'s prefetch-outcome arm)
//! instead of a per-frame `try_recv` drain. Everything else — the dedicated
//! single-worker runtime, lazy connect, [`FETCH_TIMEOUT`] redial ladder,
//! [`reset_slot_if_same`]'s same-resolver guard — is unchanged.
//!
//! `CasPrefetch::new` now hands back the receiver half separately (rather
//! than owning it, as the old crossbeam version did): only one task may ever
//! `.recv()` a `tokio::mpsc::UnboundedReceiver` (unlike a crossbeam
//! `Receiver`, which several call sites could `try_recv` from without
//! ceremony), and `dj::thread::run_loop`'s `select!` is that one task —
//! `CasPrefetch` itself only ever needs to *send*.

use std::sync::Arc;
use std::time::{Duration, Instant};

use kaijutsu_cas::ContentHash;
use kaijutsu_client::{CasResolver, SftpClient, SftpError, SshConfig};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tracing::info;

/// What a [`PrefetchOutcome`] represents once its CAS fetch lands — CAS
/// resolution happens in TWO different shapes depending on cue type: a
/// plain audio cue resolves straight to sample bytes, but a `Cas`-backed
/// clip record resolves the record JSON first and must dispatch a SECOND
/// fetch for its `media` before anything can play.
pub(crate) enum PrefetchKind {
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
/// [`PrefetchKind`], bridged from the SFTP runtime back onto the DJ thread's
/// `select!` for [`super::audio::handle_prefetch_outcome`].
pub(crate) struct PrefetchOutcome {
    pub(crate) mime: String,
    /// The object that was fetched — carried through so a skip-loud drop can
    /// name it in the log (`docs/pcm.md` R4: "log the underrun … naming the
    /// label-less hash" — no clip label rides this far down the pipeline,
    /// only the hash does).
    pub(crate) hash: ContentHash,
    pub(crate) kind: PrefetchKind,
    pub(crate) result: Result<Vec<u8>, String>,
}

/// The DJ thread's client-side CAS prefetch, owning the **Send** SFTP world.
///
/// The DJ thread's own runtime is current-thread (mirrors the RPC actor's own
/// current-thread + `LocalSet`, though the DJ has no `!Send` capnp types to
/// force that); SFTP futures are `Send` and must not ride that thread anyway
/// (a blocking cache read would stall the click/cue `select!`). So this owns
/// a *separate* multi-thread runtime, and the resolver — its own SSH
/// connection + XDG cache — lives entirely here. Results cross back to the
/// DJ's `select!` on a `tokio::mpsc` channel (see the module doc for why that
/// replaced the pre-Task-#3 `crossbeam_channel`).
pub(crate) struct CasPrefetch {
    /// Dedicated runtime for SFTP + cache IO, off the DJ thread's own
    /// current-thread runtime.
    rt: tokio::runtime::Runtime,
    /// The resolver, connected lazily on the first CAS cue and reused after
    /// (one SSH transport for the session). Cleared on a transport error so the
    /// next cue reconnects.
    resolver: Arc<AsyncMutex<Option<Arc<CasResolver<SftpClient>>>>>,
    tx: UnboundedSender<PrefetchOutcome>,
}

impl CasPrefetch {
    /// Build the prefetch runtime and its outcome channel — the receiver
    /// half is handed back separately for the one caller allowed to drain it
    /// (`dj::thread::run_loop`'s `select!`; see the module doc).
    pub(crate) fn new() -> (Self, UnboundedReceiver<PrefetchOutcome>) {
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
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                rt,
                resolver: Arc::new(AsyncMutex::new(None)),
                tx,
            },
            rx,
        )
    }

    /// Kick off an async resolve of `hash`; the outcome (tagged with `mime`
    /// and `kind`) lands on the DJ thread's `select!` for
    /// [`super::audio::handle_prefetch_outcome`]. Lazily connects the
    /// resolver on first use over `config` (the same SSH key/host the RPC
    /// channel used).
    pub(crate) fn dispatch(&self, hash: ContentHash, mime: String, config: SshConfig, kind: PrefetchKind) {
        let slot = self.resolver.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let result = resolve_with_lazy_connect(&slot, &hash, config)
                .await
                .map_err(|e| e.to_string());
            // A closed receiver just means the DJ thread is shutting down.
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
