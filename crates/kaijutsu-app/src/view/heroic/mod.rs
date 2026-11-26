//! Full-window 3D Heroic View (`Screen::Heroic`, `docs/newui.md`).
//!
//! Renders the rhythm-game road, notes, performer crystals, sky/sun/hills/fire
//! shaders, MSDF text labels, and performer selection panel on the shared HDR
//! `AppCamera`. Runs alongside `Screen::Room` and `Screen::TimeWell` as a sibling
//! workspace screen entered via `Ctrl+A k` (`Action::GoToHeroic`).

pub mod environment;
pub mod framing;
pub mod live;
pub mod notes;
pub mod performers;
pub mod road;
pub mod selection;
pub mod state;
pub mod text;

use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use kaijutsu_present::action::Action;

use self::state::HeroicState;

use crate::connection::actor_plugin::{RpcActor, RpcResultChannel, RpcResultMessage};
use crate::input::{ActionFired, InputPhase};
use crate::shaders::{HeroicFireMaterial, HeroicHillsMaterial, HeroicSunMaterial};
use crate::ui::screen::Screen;
use crate::ui::theme::Theme;
use crate::view::scene_palette::HeroicPalette;

#[derive(Component)]
pub struct HeroicRoot;

#[derive(Component)]
pub struct HeroicCamera;

#[derive(Resource, Default)]
pub struct HeroicSavedCamera {
    transform: Option<Transform>,
    projection: Option<Projection>,
    clear_color: Option<ClearColorConfig>,
    pub return_screen: Option<Screen>,
    editor_selection: Option<kaijutsu_types::ContextId>,
}

/// Wires the heroic view screen, shaders, drivers, and systems into Bevy.
pub struct HeroicPlugin;

impl Plugin for HeroicPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(MaterialPlugin::<HeroicSunMaterial>::default())
            .add_plugins(MaterialPlugin::<HeroicFireMaterial>::default())
            .add_plugins(MaterialPlugin::<HeroicHillsMaterial>::default())
            .init_resource::<HeroicPalette>()
            .register_type::<HeroicPalette>()
            .init_resource::<state::HeroicState>()
            .init_resource::<live::HeroicLiveCache>()
            .init_resource::<notes::HeroicNotesRuntime>()
            .init_resource::<performers::HeroicPerformerPool>()
            .init_resource::<text::HeroicTextState>()
            .init_resource::<selection::HeroicSelectionRuntime>()
            .init_resource::<HeroicSavedCamera>()
            .add_systems(
                Update,
                (handle_go_to_heroic, handle_reload_theme).after(InputPhase::Dispatch),
            )
            .add_systems(OnEnter(Screen::Heroic), enter_heroic)
            .add_systems(OnExit(Screen::Heroic), exit_heroic)
            .add_systems(
                Update,
                (
                    selection::heroic_keyboard.after(InputPhase::Dispatch),
                    selection::open_board_note,
                    live::poll_heroic_live_data,
                    live::apply_heroic_work_results,
                    live::ingest_heroic_cues,
                    live::sync_heroic_state,
                    sync_heroic_live_tuning,
                    environment::sync_environment,
                    road::sync_road,
                    notes::sync_notes_and_pads,
                    performers::sync_performers,
                    text::sync_heroic_text,
                    selection::sync_selection_ui,
                )
                    .chain()
                    .run_if(in_state(Screen::Heroic)),
            );
    }
}

/// Global `Action::GoToHeroic` handler (`Ctrl+A k`).
pub fn handle_go_to_heroic(
    mut actions: MessageReader<ActionFired>,
    screen: Res<State<Screen>>,
    mut saved: ResMut<HeroicSavedCamera>,
    mut next: ResMut<NextState<Screen>>,
) {
    for ActionFired { action, .. } in actions.read() {
        if matches!(action, Action::GoToHeroic) && *screen.get() != Screen::Heroic {
            saved.return_screen = Some(*screen.get());
            next.set(Screen::Heroic);
        }
    }
}

/// Global `Action::ReloadTheme` handler — refetches `theme.toml` over RPC when
/// connected so `HeroicPalette` and scene shaders hot-reload immediately.
pub fn handle_reload_theme(
    mut actions: MessageReader<ActionFired>,
    actor: Option<Res<RpcActor>>,
    result_channel: Res<RpcResultChannel>,
) {
    let mut triggered = false;
    for ActionFired { action, .. } in actions.read() {
        if matches!(action, Action::ReloadTheme) {
            triggered = true;
        }
    }
    if !triggered {
        return;
    }
    let Some(actor) = actor else {
        return;
    };
    let handle = actor.handle.clone();
    let tx = result_channel.sender();
    bevy::tasks::IoTaskPool::get()
        .spawn(async move {
            match handle.get_config("theme.toml".to_string()).await {
                Ok(toml) => {
                    let _ = tx.send(RpcResultMessage::ThemeReceived(toml));
                }
                Err(e) => {
                    log::warn!("heroic: theme reload over RPC failed: {e}");
                }
            }
        })
        .detach();
}

#[derive(SystemParam)]
struct HeroicSpawnAssets<'w> {
    meshes: ResMut<'w, Assets<Mesh>>,
    images: ResMut<'w, Assets<Image>>,
    std_mats: ResMut<'w, Assets<StandardMaterial>>,
    sun_mats: ResMut<'w, Assets<HeroicSunMaterial>>,
    fire_mats: ResMut<'w, Assets<HeroicFireMaterial>>,
    hill_mats: ResMut<'w, Assets<HeroicHillsMaterial>>,
    card_mats: ResMut<'w, Assets<crate::shaders::WellCardMaterial>>,
}

#[derive(SystemParam)]
struct HeroicEntryRuntimes<'w> {
    perf_pool: ResMut<'w, performers::HeroicPerformerPool>,
    text_state: ResMut<'w, text::HeroicTextState>,
    sel_runtime: ResMut<'w, selection::HeroicSelectionRuntime>,
    notes_rt: ResMut<'w, notes::HeroicNotesRuntime>,
}

#[allow(clippy::too_many_arguments)]
fn enter_heroic(
    mut commands: Commands,
    palette: Res<HeroicPalette>,
    mut state: ResMut<HeroicState>,
    actor: Option<Res<RpcActor>>,
    result_channel: Res<RpcResultChannel>,
    mut saved: ResMut<HeroicSavedCamera>,
    mut app_camera: Query<(Entity, &mut Camera, &mut Transform, &mut Projection), With<Camera3d>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    existing: Query<Entity, With<HeroicRoot>>,
    mut assets: HeroicSpawnAssets,
    mut runtimes: HeroicEntryRuntimes,
) {
    if !existing.is_empty() {
        return;
    }

    state.clear_driver_state();
    state.selected_performer = saved.editor_selection.take();
    if actor.is_some() {
        live::request_initial_tracks(actor.as_deref(), &result_channel);
    }

    let aspect = windows
        .iter()
        .next()
        .map(|w| w.width() / w.height().max(1.0))
        .unwrap_or(16.0 / 9.0);

    if let Ok((cam_e, mut cam, mut tf, mut proj)) = app_camera.single_mut() {
        saved.transform = Some(*tf);
        saved.projection = Some(proj.clone());
        saved.clear_color = Some(cam.clear_color);

        commands.entity(cam_e).insert(HeroicCamera);
        cam.clear_color = ClearColorConfig::Custom(Color::LinearRgba(palette.sky_top));
        let (solved_tf, solved_proj) = framing::solve_camera(
            &palette.framing,
            state.road_width(palette.lane_width),
            aspect,
            state.orbit_angle,
        );
        *tf = solved_tf;
        *proj = solved_proj;
    }

    let root = commands
        .spawn((
            HeroicRoot,
            Transform::default(),
            Visibility::Inherited,
            Name::new("HeroicRoot"),
        ))
        .id();

    environment::spawn_environment(
        &mut commands,
        root,
        &palette,
        &mut assets.meshes,
        &mut assets.std_mats,
        &mut assets.sun_mats,
        &mut assets.fire_mats,
        &mut assets.hill_mats,
    );
    road::spawn_road(&mut commands, root, &palette, &mut assets.meshes, &mut assets.std_mats);
    notes::spawn_notes_and_pads(&mut commands, root, &mut assets.meshes, &mut assets.std_mats);
    *runtimes.notes_rt = notes::HeroicNotesRuntime::default();

    runtimes.perf_pool.slots.clear();
    runtimes.perf_pool.root = Some(performers::spawn_performers_root(&mut commands, root));

    runtimes.text_state.root = Some(text::spawn_text_root(&mut commands, root));
    selection::spawn_selection_ui(
        &mut commands,
        root,
        &mut assets.images,
        &mut assets.meshes,
        &mut assets.card_mats,
        &mut assets.std_mats,
        &mut runtimes.sel_runtime,
    );
}

fn exit_heroic(
    mut commands: Commands,
    theme: Res<Theme>,
    next_screen: Res<NextState<Screen>>,
    screen: Res<State<Screen>>,
    mut state: ResMut<HeroicState>,
    mut saved: ResMut<HeroicSavedCamera>,
    roots: Query<Entity, With<HeroicRoot>>,
    mut cam_q: Query<(Entity, &mut Camera, &mut Transform, &mut Projection), With<HeroicCamera>>,
    mut perf_pool: ResMut<performers::HeroicPerformerPool>,
    mut text_state: ResMut<text::HeroicTextState>,
    mut sel_runtime: ResMut<selection::HeroicSelectionRuntime>,
) {
    for e in roots.iter() {
        commands.entity(e).despawn();
    }
    saved.editor_selection = if *screen.get() == Screen::Editor { state.selected_performer } else { None };
    state.clear_driver_state();
    perf_pool.root = None;
    perf_pool.slots.clear();
    *text_state = text::HeroicTextState::default();
    *sel_runtime = selection::HeroicSelectionRuntime::default();

    let going_room = matches!(&*next_screen, NextState::Pending(Screen::Room));
    if let Ok((cam_e, mut cam, mut tf, mut proj)) = cam_q.single_mut() {
        commands.entity(cam_e).remove::<HeroicCamera>();
        cam.clear_color = saved
            .clear_color
            .take()
            .unwrap_or(ClearColorConfig::Custom(theme.bg));
        if going_room {
            if let Some(old_tf) = saved.transform.take() {
                *tf = old_tf;
            }
            if let Some(old_proj) = saved.projection.take() {
                *proj = old_proj;
            }
        } else {
            // Canonical pose for Conversation/other screens so exiting from Room->Heroic->Esc
            // never leaves the room's camera orientation active over Conversation.
            let _ = saved.transform.take();
            let _ = saved.projection.take();
            *tf = Transform::IDENTITY;
            *proj = Projection::default();
        }
    }
}

/// Apply `[heroic]` palette/framing changes to the live camera and material
/// clear color whenever `HeroicPalette` or `HeroicState` lane count changes.
pub fn sync_heroic_live_tuning(
    palette: Res<HeroicPalette>,
    state: Res<HeroicState>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut cam_q: Query<(&mut Camera, &mut Transform, &mut Projection), With<HeroicCamera>>,
) {
    let aspect = windows
        .iter()
        .next()
        .map(|w| w.width() / w.height().max(1.0))
        .unwrap_or(16.0 / 9.0);
    if let Ok((mut cam, mut tf, mut proj)) = cam_q.single_mut() {
        cam.clear_color = ClearColorConfig::Custom(Color::LinearRgba(palette.sky_top));
        let (solved_tf, solved_proj) = framing::solve_camera(
            &palette.framing,
            state.road_width(palette.lane_width),
            aspect,
            state.orbit_angle,
        );
        *tf = solved_tf;
        *proj = solved_proj;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_tuning_updates_camera_when_heroic_palette_changes() {
        let mut world = World::new();
        let mut pal = HeroicPalette::default();
        world.insert_resource(pal.clone());
        let mut st = state::HeroicState::default();
        st.lanes.push(state::HeroicLane {
            track_id: "main".into(),
            score_context_id: kaijutsu_types::ContextId::new(),
            period_secs: 0.5,
            beats_per_phrase: 4,
            playhead_beats: 0.0,
            playing: true,
            commit_margin_ticks: 1,
            performers: Vec::new(),
        });
        world.insert_resource(st);

        let cam_e = world
            .spawn((
                HeroicCamera,
                Camera::default(),
                Transform::default(),
                Projection::default(),
            ))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems(sync_heroic_live_tuning);
        schedule.run(&mut world);
        let initial_tf = *world.get::<Transform>(cam_e).unwrap();

        pal.framing.horizon = 0.72;
        pal.framing.fov_degrees = 42.0;
        world.insert_resource(pal);
        schedule.run(&mut world);

        let updated_tf = *world.get::<Transform>(cam_e).unwrap();
        assert!(
            (updated_tf.translation - initial_tf.translation).length() > 0.1,
            "camera did not update after HeroicPalette change"
        );
    }

    #[test]
    fn live_tuning_updates_sun_and_fire_material_uniforms() {
        let mut world = World::new();
        let mut pal = HeroicPalette::default();
        world.insert_resource(pal.clone());
        world.insert_resource(HeroicState::default());
        world.insert_resource(Time::<()>::default());
        world.insert_resource(Assets::<Mesh>::default());
        world.insert_resource(Assets::<HeroicSunMaterial>::default());
        world.insert_resource(Assets::<HeroicFireMaterial>::default());
        world.insert_resource(Assets::<HeroicHillsMaterial>::default());

        let sun_h = world
            .resource_mut::<Assets<HeroicSunMaterial>>()
            .add(HeroicSunMaterial {
                sun_top: Vec4::ZERO,
                sun_mid: Vec4::ZERO,
                sun_bot: Vec4::ZERO,
                params: Vec4::ZERO,
            });
        world.spawn((
            environment::HeroicSun,
            MeshMaterial3d(sun_h.clone()),
            Transform::default(),
        ));

        let mut schedule = Schedule::default();
        schedule.add_systems(environment::sync_environment);
        schedule.run(&mut world);
        let first_top = world
            .resource::<Assets<HeroicSunMaterial>>()
            .get(&sun_h)
            .unwrap()
            .sun_top;

        pal.sun[0] = LinearRgba::rgb(0.1, 0.8, 0.2);
        pal.intensity = 1.0;
        world.insert_resource(pal);
        schedule.run(&mut world);

        let updated_mat = world
            .resource::<Assets<HeroicSunMaterial>>()
            .get(&sun_h)
            .unwrap();
        assert!((updated_mat.sun_top - first_top).length() > 0.2);
        assert!((updated_mat.params.x - 1.0).abs() < 1e-4);
    }

    #[test]
    fn idle_schedule_keeps_lanes_empty_and_decays_energy() {
        let mut world = World::new();
        world.insert_resource(HeroicPalette::default());
        let mut state = HeroicState::default();
        state.band_energies[0] = 1.0;
        world.insert_resource(state);
        world.insert_resource(live::HeroicLiveCache::default());
        world.insert_resource(crate::view::time_well::rays::WellTracks::default());
        world.insert_resource(crate::connection::drift::DriftState::default());
        world.insert_resource(crate::view::time_well::live::ContextTails::default());
        world.insert_resource(crate::view::time_well::live::WellBeatsRes::default());
        world.insert_resource(notes::HeroicNotesRuntime::default());
        world.insert_resource(Assets::<Mesh>::default());

        let mut time = Time::<()>::default();
        let mut schedule = Schedule::default();
        schedule.add_systems((live::sync_heroic_state, notes::sync_notes_and_pads).chain());
        for _ in 0..10 {
            time.advance_by(std::time::Duration::from_millis(25));
            world.insert_resource(time.clone());
            schedule.run(&mut world);
        }
        let st = world.resource::<HeroicState>();
        assert!(st.lanes.is_empty());
        assert!(st.band_energies[0] < 0.9, "expected energy to decay when idle");
    }
}
