//! PCM sample playback sink — the app side of `docs/pcm.md`'s audio render
//! seam (slice 3).
//!
//! `kaijutsu-audio::AudioRenderTarget` is the *conceptual* seam ("one sink,
//! play a sample"); this module is the app's implementation of that idea, but
//! not as a struct wearing the trait. `AudioRenderTarget::play` takes `&self`
//! deliberately (`docs/pcm.md` "play(&self) vs emit(&mut self)") because the
//! Bevy sink acts through `Commands` — spawning an entity, inserting an asset
//! — rather than mutating sink-owned state. A Bevy system already *is* that
//! shape: world access without `&mut self` on some sink struct. Wrapping it in
//! a `BevyAudioOut` type here would just forward straight through to
//! `Commands` with no seam benefit, so the system reading
//! `ServerEventMessage` plays the sink role directly, per the brief.

use bevy::prelude::*;
use kaijutsu_audio::AudioRef;
use kaijutsu_client::ServerEvent;

use crate::connection::actor_plugin::ServerEventMessage;

/// Bridges `ServerEvent::PlayAudio` directives into Bevy `AudioPlayer` spawns.
pub struct AudioOutPlugin;

impl Plugin for AudioOutPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, play_audio_directives);
    }
}

/// Consume `PlayAudio` directives and spawn a one-shot `AudioPlayer` for each
/// `Encoded` sample. `Cas` refs aren't resolved yet — that's slice 5's
/// client-side CAS prefetch (`docs/pcm.md` "How it converges") — so we warn
/// loudly and skip rather than silently dropping the sound.
fn play_audio_directives(
    mut messages: MessageReader<ServerEventMessage>,
    mut commands: Commands,
    mut sources: ResMut<Assets<AudioSource>>,
) {
    for ServerEventMessage(event) in messages.read() {
        let ServerEvent::PlayAudio { audio, .. } = event else {
            continue;
        };

        match audio {
            AudioRef::Encoded { bytes, .. } => {
                let handle = sources.add(AudioSource {
                    bytes: bytes.clone().into(),
                });
                // DESPAWN (not the ONCE default): a fire-and-forget sample
                // shouldn't leave a drained-sink entity sitting in the world
                // forever — see `PlaybackMode::Despawn` in bevy_audio.
                commands.spawn((AudioPlayer(handle), PlaybackSettings::DESPAWN));
            }
            AudioRef::Cas { hash, format } => {
                warn!(
                    "CAS-backed audio not yet supported in the app sink — \
                     arrives with slice 5's client-side prefetch \
                     (hash={hash:?}, format={format:?})"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_audio::AudioFormatHint;
    use kaijutsu_cas::ContentHash;
    use kaijutsu_types::ContextId;
    use std::str::FromStr;

    /// Minimal headless app: just enough to run `play_audio_directives`
    /// without a real audio device — `TaskPoolPlugin` (asset IO needs task
    /// pools) + `AssetPlugin` (registers `AssetServer`/`Assets<T>` plumbing)
    /// + `init_asset::<AudioSource>()`, no `AudioPlugin`/window/speakers.
    fn test_app() -> App {
        let mut app = App::new();
        app.add_plugins((TaskPoolPlugin::default(), AssetPlugin::default()))
            .init_asset::<AudioSource>()
            .add_message::<ServerEventMessage>()
            .add_systems(Update, play_audio_directives);
        app
    }

    fn player_count(app: &mut App) -> usize {
        let world = app.world_mut();
        let mut query = world.query::<&AudioPlayer>();
        query.iter(world).count()
    }

    /// Real WAV bytes (pawlsa's test fixture, docs/pcm.md "Verification")
    /// through `ServerEvent::PlayAudio` should produce exactly one
    /// `AudioSource` asset and one `AudioPlayer` entity.
    #[test]
    fn encoded_wav_spawns_one_audio_player() {
        let path = "/home/atobey/src/pawlsa-mcp/pawlsa-test.wav";
        let Ok(bytes) = std::fs::read(path) else {
            eprintln!("skipping: {path} not present on this machine");
            return;
        };

        let mut app = test_app();
        app.world_mut().write_message(ServerEventMessage(ServerEvent::PlayAudio {
            context_id: ContextId::new(),
            audio: AudioRef::Encoded {
                bytes,
                format: AudioFormatHint::Wav,
            },
        }));

        app.update();

        assert_eq!(
            app.world().resource::<Assets<AudioSource>>().len(),
            1,
            "expected exactly one AudioSource asset to be added"
        );
        assert_eq!(player_count(&mut app), 1, "expected exactly one AudioPlayer entity");
    }

    /// `Cas` refs aren't resolved in this slice — the directive must be
    /// skipped (with a loud warn, not silently), producing no asset and no
    /// entity.
    #[test]
    fn cas_ref_is_skipped_not_played() {
        let mut app = test_app();
        app.world_mut().write_message(ServerEventMessage(ServerEvent::PlayAudio {
            context_id: ContextId::new(),
            audio: AudioRef::Cas {
                hash: ContentHash::from_str("00000000000000000000000000000000").unwrap(),
                format: AudioFormatHint::Wav,
            },
        }));

        app.update();

        assert_eq!(app.world().resource::<Assets<AudioSource>>().len(), 0);
        assert_eq!(player_count(&mut app), 0);
    }
}
