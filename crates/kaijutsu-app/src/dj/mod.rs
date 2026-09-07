//! Optional in-process audio runtime and its visual traffic mirror.
use bevy::prelude::*;
use kaijutsu_audio_runtime::{Engine, Options};
use crate::connection::actor_plugin::{RpcActor, RpcConnectionState};

#[derive(Message)]
pub struct RenderPortTraffic;

#[derive(Resource, Default)]
struct AudioRuntime {
    engine: Option<Engine>,
    generation: Option<u64>,
}

pub struct DjPlugin {
    pub enabled: bool,
}

impl Plugin for DjPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<RenderPortTraffic>();
        if self.enabled {
            app.init_resource::<AudioRuntime>()
                .add_systems(Update, (connect_audio, mirror_traffic).chain());
        }
    }
}

fn connect_audio(actor: Option<Res<RpcActor>>, connection: Res<RpcConnectionState>, client_id: Res<crate::connection::client_id::ClientId>, mut audio: ResMut<AudioRuntime>) {
    let Some(actor) = actor else { return };
    if !connection.connected || connection.context_id.is_none() || audio.generation == Some(actor.generation) { return; }
    audio.engine.take();
    audio.generation = Some(actor.generation);
    let options = Options { config_client: Some(client_id.0.to_string()), ..Options::default() };
    match Engine::start(actor.handle.clone(), connection.ssh_config.clone(), connection.context_id, options) {
        Ok(engine) => audio.engine = Some(engine),
        Err(e) => error!("local audio did not start: {e}"),
    }
}

fn mirror_traffic(mut audio: ResMut<AudioRuntime>, mut traffic: MessageWriter<RenderPortTraffic>) {
    let Some(engine) = &mut audio.engine else { return };
    if engine.is_finished() {
        if let Err(e) = engine.shutdown() { error!("local audio stopped: {e}"); }
        audio.engine.take();
        return;
    }
    let mut active = false;
    while engine.pulses.try_recv().is_ok() { active = true; }
    if active { traffic.write(RenderPortTraffic); }
}
