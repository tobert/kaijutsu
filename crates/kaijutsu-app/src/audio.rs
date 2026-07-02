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
//! `AudioPlayer` per cue). Two things still belong to slice 5c: CAS-backed
//! payloads (client-side prefetch under the speculation lead) and non-audio
//! mimes (MIDI queued into a local ALSA seq port); both warn loudly and skip
//! today rather than silently dropping the cue. `cue.lead` is likewise honored
//! only for the zero-lead play-now case — scheduled playback arrives with 5c.

use bevy::prelude::*;
use kaijutsu_audio::CuePayload;
use kaijutsu_client::ServerEvent;

use crate::connection::actor_plugin::ServerEventMessage;

/// Bridges `ServerEvent::RenderCue` directives into Bevy `AudioPlayer` spawns.
pub struct AudioOutPlugin;

impl Plugin for AudioOutPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, play_render_cues);
    }
}

/// Consume `RenderCue` directives and spawn a one-shot `AudioPlayer` for each
/// inline `audio/*` cue. CAS payloads and non-audio mimes aren't handled yet
/// (slice 5c) — we warn loudly and skip rather than silently dropping the cue.
fn play_render_cues(
    mut messages: MessageReader<ServerEventMessage>,
    mut commands: Commands,
    mut sources: ResMut<Assets<AudioSource>>,
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
                    bytes: std::sync::Arc::from(bytes.as_slice()),
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
            CuePayload::Cas(hash) => {
                warn!(
                    "CAS-backed render cue not yet supported in the app sink — \
                     arrives with slice 5c's client-side prefetch \
                     (hash={hash:?}, mime={})",
                    cue.mime
                );
            }
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

    /// Minimal headless app: just enough to run `play_render_cues`
    /// without a real audio device — `TaskPoolPlugin` (asset IO needs task
    /// pools) + `AssetPlugin` (registers `AssetServer`/`Assets<T>` plumbing)
    /// + `init_asset::<AudioSource>()`, no `AudioPlugin`/window/speakers.
    fn test_app() -> App {
        let mut app = App::new();
        app.add_plugins((TaskPoolPlugin::default(), AssetPlugin::default()))
            .init_asset::<AudioSource>()
            .add_message::<ServerEventMessage>()
            .add_systems(Update, play_render_cues);
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

    /// CAS payloads aren't resolved in this slice — the cue must be skipped
    /// (with a loud warn, not silently), producing no asset and no entity.
    #[test]
    fn cas_cue_is_skipped_not_played() {
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

    /// A non-audio mime (a clip record / MIDI cue) is slice-5c territory — the
    /// app sink skips it today rather than mis-decoding it as a sample.
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
