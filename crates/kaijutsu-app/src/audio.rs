//! Render sink — the app side of `docs/pcm.md` / `docs/midi.md`'s mime-keyed
//! render cue seam.
//!
//! `kaijutsu-audio::RenderSink` is the *conceptual* seam ("one sink, emit a
//! cue"); this module is the app's implementation of that idea, but not as a
//! struct wearing the trait. `RenderSink::emit` takes `&self` deliberately
//! (`docs/pcm.md` "play(&self) vs emit(&mut self)") because the Bevy sink acts
//! through `Commands` — spawning an entity, inserting an asset — rather than
//! mutating sink-owned state. A Bevy system already *is* that shape: world
//! access without `&mut self` on some sink struct. Wrapping it in a
//! `BevyAudioOut` type here would just forward straight through to `Commands`
//! with no seam benefit, so the system reading `ServerEventMessage` plays the
//! sink role directly, per the brief.
//!
//! Slice 5a handles the play-now inline `audio/*` cue (slice-3 parity, one
//! `AudioPlayer` per cue). **Slice 5c / track-B B4** adds the CAS-backed audio
//! path: a `CuePayload::Cas(hash)` audio cue no longer warns-and-skips — it
//! resolves the hash through a [`BlobResolver`] (XDG CAS cache + SFTP fetch from
//! `/v/blobs`, `docs/slash-v.md`) off the Bevy main thread, then plays the
//! resolved bytes. This first cut is **fetch-on-cue**; the two-phase
//! prepare-horizon prefetch (`docs/pcm.md` "Open questions") is a follow-up on
//! the same resolver. `cue.lead` is honored only for the zero-lead play-now case
//! (both inline and CAS) — scheduled playback also arrives with the prepare
//! horizon. Non-audio mimes (MIDI `text/vnd.abc`) are owned by `midi.rs` off the
//! same message stream.

use std::sync::Arc;

use bevy::prelude::*;
use crossbeam_channel::{Receiver, Sender, unbounded};
use kaijutsu_audio::CuePayload;
use kaijutsu_cas::ContentHash;
use kaijutsu_client::{BlobResolver, ServerEvent, SftpClient, SftpError, SshConfig};
use tokio::sync::Mutex as AsyncMutex;

use crate::connection::actor_plugin::{RpcConnectionState, ServerEventMessage};

/// Bridges `ServerEvent::RenderCue` directives into Bevy `AudioPlayer` spawns.
pub struct AudioOutPlugin;

impl Plugin for AudioOutPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(BlobPrefetch::new())
            .add_systems(Update, (play_render_cues, drain_prefetch_results));
    }
}

/// A resolved (or failed) CAS prefetch, tagged with the cue's mime, bridged from
/// the SFTP runtime back onto the Bevy main thread for [`drain_prefetch_results`].
struct PrefetchOutcome {
    mime: String,
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
pub struct BlobPrefetch {
    /// Dedicated runtime for SFTP + cache IO, off the RPC actor's `!Send` world.
    rt: tokio::runtime::Runtime,
    /// The resolver, connected lazily on the first CAS cue and reused after
    /// (one SSH transport for the session). Cleared on a transport error so the
    /// next cue reconnects.
    resolver: Arc<AsyncMutex<Option<Arc<BlobResolver<SftpClient>>>>>,
    tx: Sender<PrefetchOutcome>,
    rx: Receiver<PrefetchOutcome>,
}

impl BlobPrefetch {
    fn new() -> Self {
        // One worker: prefetch is latency-tolerant (it runs under the prepare
        // horizon), and a single background thread keeps SFTP + the blocking
        // FileStore read off the render loop. (spawn_blocking for the cache IO
        // is the recorded follow-up, `docs/issues.md`.)
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("kaijutsu-blob-prefetch")
            .enable_all()
            .build()
            .expect("blob-prefetch tokio runtime");
        let (tx, rx) = unbounded();
        Self {
            rt,
            resolver: Arc::new(AsyncMutex::new(None)),
            tx,
            rx,
        }
    }

    /// Kick off an async resolve of `hash`; the outcome (tagged with `mime`)
    /// lands on `rx` for [`drain_prefetch_results`]. Lazily connects the resolver
    /// on first use over `config` (the same SSH key/host the RPC channel used).
    fn dispatch(&self, hash: ContentHash, mime: String, config: SshConfig) {
        let slot = self.resolver.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let result = resolve_with_lazy_connect(&slot, &hash, config)
                .await
                .map_err(|e| e.to_string());
            // A closed receiver just means the app is shutting down.
            let _ = tx.send(PrefetchOutcome { mime, result });
        });
    }
}

/// Resolve `hash`, connecting the shared resolver on first use. A transport
/// failure drops the connection so it redials; a per-object failure (NotFound /
/// HashMismatch) leaves the healthy transport in place.
async fn resolve_with_lazy_connect(
    slot: &AsyncMutex<Option<Arc<BlobResolver<SftpClient>>>>,
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
    slot: &AsyncMutex<Option<Arc<BlobResolver<SftpClient>>>>,
    config: &SshConfig,
) -> Result<Arc<BlobResolver<SftpClient>>, SftpError> {
    let mut guard = slot.lock().await;
    if guard.is_none() {
        let sftp = SftpClient::connect(config.clone()).await?;
        *guard = Some(Arc::new(BlobResolver::with_xdg_cache(sftp)));
    }
    Ok(guard.as_ref().expect("resolver present").clone())
}

/// Clear the slot only if it still holds `failed` — so a concurrent cue's fresh
/// reconnection is never wiped by a late loser's error handler (the transport
/// slot-clearing race).
async fn reset_slot_if_same(
    slot: &AsyncMutex<Option<Arc<BlobResolver<SftpClient>>>>,
    failed: &Arc<BlobResolver<SftpClient>>,
) {
    let mut guard = slot.lock().await;
    if guard.as_ref().is_some_and(|cur| Arc::ptr_eq(cur, failed)) {
        *guard = None;
    }
}

/// Consume `RenderCue` directives: play an inline `audio/*` cue now, and
/// dispatch a CAS-backed `audio/*` cue to the prefetch resolver. Non-audio and
/// unconnected cases are handled explicitly (never a silent drop).
fn play_render_cues(
    mut messages: MessageReader<ServerEventMessage>,
    mut commands: Commands,
    mut sources: ResMut<Assets<AudioSource>>,
    prefetch: Res<BlobPrefetch>,
    connection: Option<Res<RpcConnectionState>>,
) {
    for ServerEventMessage(event) in messages.read() {
        let ServerEvent::RenderCue { cue, .. } = event else {
            continue;
        };

        match &cue.payload {
            CuePayload::Inline(bytes) if cue.mime.starts_with("audio/") => {
                // One copy: `Arc::from(&[u8])` clones the slice straight into the
                // Arc allocation. (`bytes.clone().into()` would allocate a Vec
                // copy *then* reallocate it into the Arc — two copies of the
                // sample.) Inline is the small-sample path, but even so.
                let handle = sources.add(AudioSource {
                    bytes: Arc::from(bytes.as_slice()),
                });
                // DESPAWN (not the ONCE default): a fire-and-forget sample
                // shouldn't leave a drained-sink entity sitting in the world
                // forever — see `PlaybackMode::Despawn` in bevy_audio.
                commands.spawn((AudioPlayer(handle), PlaybackSettings::DESPAWN));
            }
            CuePayload::Inline(_) => {
                // A non-audio inline cue (e.g. `text/vnd.abc` → the MIDI sink in
                // `midi.rs`, or a future clip renderer). Another sink handles it
                // off the same message stream; nothing to do here.
            }
            CuePayload::Cas(hash) if cue.mime.starts_with("audio/") => {
                // Resolve off-thread from the XDG cache / SFTP, then play when
                // the bytes land (drain_prefetch_results). Needs a live SSH
                // config — cues only arrive after connect, so an unconnected
                // state is the pre-connect edge, not the normal path.
                match connection.as_deref() {
                    Some(conn) if conn.connected => {
                        prefetch.dispatch(hash.clone(), cue.mime.clone(), conn.ssh_config.clone());
                    }
                    _ => warn!(
                        "CAS render cue arrived before a live connection — cannot prefetch \
                         (hash={hash:?}, mime={})",
                        cue.mime
                    ),
                }
            }
            CuePayload::Cas(hash) => {
                // A CAS blob with a non-audio mime (e.g. a rendered MIDI blob) —
                // no sink here. The clip-record path (parse Shape A, resolve the
                // media hash) is the next slice; this stays loud, not silent.
                warn!(
                    "CAS render cue with non-audio mime not handled by the audio sink \
                     (hash={hash:?}, mime={})",
                    cue.mime
                );
            }
        }
    }
}

/// Play prefetched CAS blobs as they resolve — the async tail of the CAS branch
/// in [`play_render_cues`]. Runs on the Bevy main thread (world access), so the
/// off-thread resolve never touches `Commands`/`Assets` directly.
fn drain_prefetch_results(
    prefetch: Res<BlobPrefetch>,
    mut commands: Commands,
    mut sources: ResMut<Assets<AudioSource>>,
) {
    while let Ok(outcome) = prefetch.rx.try_recv() {
        match outcome.result {
            Ok(bytes) if outcome.mime.starts_with("audio/") => {
                let handle = sources.add(AudioSource {
                    bytes: Arc::from(bytes.as_slice()),
                });
                commands.spawn((AudioPlayer(handle), PlaybackSettings::DESPAWN));
            }
            Ok(bytes) => {
                // Resolved fine but nothing here decodes it — loud, not silent.
                warn!(
                    "resolved a non-audio CAS blob ({} bytes, mime={}); no sink in the audio path",
                    bytes.len(),
                    outcome.mime
                );
            }
            Err(e) => warn!("CAS prefetch failed (mime={}): {e}", outcome.mime),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_audio::RenderCue;
    use kaijutsu_cas::ContentHash;
    use kaijutsu_types::ContextId;
    use std::str::FromStr;

    /// Minimal headless app: just enough to run the sink systems without a real
    /// audio device — `TaskPoolPlugin` (asset IO needs task pools) +
    /// `AssetPlugin` (registers `AssetServer`/`Assets<T>` plumbing) +
    /// `init_asset::<AudioSource>()`, plus the `BlobPrefetch` resource. No
    /// `AudioPlugin`/window/speakers, and no `ConnectionActor` (so CAS dispatch
    /// hits the unconnected edge).
    fn test_app() -> App {
        let mut app = App::new();
        app.add_plugins((TaskPoolPlugin::default(), AssetPlugin::default()))
            .init_asset::<AudioSource>()
            .add_message::<ServerEventMessage>()
            .insert_resource(BlobPrefetch::new())
            .add_systems(Update, (play_render_cues, drain_prefetch_results));
        app
    }

    fn player_count(app: &mut App) -> usize {
        let world = app.world_mut();
        let mut query = world.query::<&AudioPlayer>();
        query.iter(world).count()
    }

    fn play(app: &mut App, cue: RenderCue) {
        app.world_mut().write_message(ServerEventMessage(ServerEvent::RenderCue {
            context_id: ContextId::new(),
            cue,
        }));
        app.update();
    }

    /// Push a prefetch outcome straight onto the channel (same-module access) —
    /// stands in for the off-thread resolve completing, so the drain system is
    /// testable without a live SFTP server.
    fn deliver(app: &mut App, mime: &str, result: Result<Vec<u8>, String>) {
        app.world()
            .resource::<BlobPrefetch>()
            .tx
            .send(PrefetchOutcome {
                mime: mime.to_string(),
                result,
            })
            .unwrap();
        app.update();
    }

    /// Real WAV bytes (pawlsa's test fixture, docs/pcm.md "Verification")
    /// through a play-now inline `audio/wav` `RenderCue` should produce exactly
    /// one `AudioSource` asset and one `AudioPlayer` entity.
    #[test]
    fn inline_audio_cue_spawns_one_audio_player() {
        let path = "/home/atobey/src/pawlsa-mcp/pawlsa-test.wav";
        let Ok(bytes) = std::fs::read(path) else {
            eprintln!("skipping: {path} not present on this machine");
            return;
        };

        let mut app = test_app();
        play(&mut app, RenderCue::now_inline("audio/wav", bytes));

        assert_eq!(
            app.world().resource::<Assets<AudioSource>>().len(),
            1,
            "expected exactly one AudioSource asset to be added"
        );
        assert_eq!(player_count(&mut app), 1, "expected exactly one AudioPlayer entity");
    }

    /// A CAS audio cue with no connection can't prefetch — it warns and produces
    /// nothing this frame (the resolve is off-thread and needs an SSH config).
    #[test]
    fn cas_audio_cue_without_connection_produces_nothing() {
        let mut app = test_app();
        play(
            &mut app,
            RenderCue {
                mime: "audio/wav".into(),
                payload: CuePayload::Cas(
                    ContentHash::from_str("00000000000000000000000000000000").unwrap(),
                ),
                lead: std::time::Duration::ZERO,
            },
        );

        assert_eq!(app.world().resource::<Assets<AudioSource>>().len(), 0);
        assert_eq!(player_count(&mut app), 0);
    }

    /// The async tail: a resolved `audio/*` blob delivered on the channel spawns
    /// exactly one `AudioPlayer`. (Dummy bytes suffice — `AudioSource` stores
    /// them; bevy_audio decodes lazily at play, which the headless app skips.)
    #[test]
    fn a_resolved_audio_blob_is_played() {
        let mut app = test_app();
        deliver(&mut app, "audio/wav", Ok(vec![1, 2, 3, 4]));

        assert_eq!(app.world().resource::<Assets<AudioSource>>().len(), 1);
        assert_eq!(player_count(&mut app), 1);
    }

    /// A failed prefetch is a loud no-op — no asset, no entity.
    #[test]
    fn a_failed_prefetch_plays_nothing() {
        let mut app = test_app();
        deliver(&mut app, "audio/wav", Err("no such path: /v/blobs/…".into()));

        assert_eq!(app.world().resource::<Assets<AudioSource>>().len(), 0);
        assert_eq!(player_count(&mut app), 0);
    }

    /// A resolved but non-audio blob has no sink in the audio path — warn, don't
    /// mis-decode it as a sample.
    #[test]
    fn a_resolved_non_audio_blob_plays_nothing() {
        let mut app = test_app();
        deliver(&mut app, "application/octet-stream", Ok(vec![0, 1, 2]));

        assert_eq!(app.world().resource::<Assets<AudioSource>>().len(), 0);
        assert_eq!(player_count(&mut app), 0);
    }

    /// A non-audio inline mime (a clip record / MIDI cue) is another sink's
    /// territory — the audio sink skips it rather than mis-decoding a sample.
    #[test]
    fn non_audio_inline_cue_is_skipped() {
        let mut app = test_app();
        play(
            &mut app,
            RenderCue::now_inline("application/vnd.kaijutsu.clip+json", b"{}".to_vec()),
        );

        assert_eq!(app.world().resource::<Assets<AudioSource>>().len(), 0);
        assert_eq!(player_count(&mut app), 0);
    }
}
