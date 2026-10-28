//! The placement keys: the well's `p` and `d`, and `Ctrl+A q`
//! (`docs/input.md`, "The prefix table" and "Placement keys").
//!
//! Each key runs `kj context promote|demote <id>` through `executeKj`
//! against the context it moves, as the tui's picker does. The verb checks
//! placement authority, latches the demote step that archives behind
//! `--confirm`, and runs the archive rc lifecycle, so a key gets the same
//! checks as a typed line.
//!
//! A latched demote moves nothing. The hints line says that the next step
//! archives, and the same key on the same context, while that notice shows,
//! runs the step again with `--confirm`. One press never archives.

use bevy::prelude::*;
use kaijutsu_client::rpc::KjExecutionResult;
use kaijutsu_types::ContextId;

use crate::cell::{ContextSwitchRequested, DocumentCache};
use crate::connection::{RpcActor, RpcResultChannel, RpcResultMessage};
use crate::ui::ask_sheet::{AskNotice, NOTICE_TTL};

/// The key that asked for a placement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlacementKey {
    /// The well's `p`: `kj context promote`.
    Promote,
    /// The well's `d`: `kj context demote`.
    Demote,
    /// `Ctrl+A q`: `kj context demote` on the context on screen, then land
    /// on the previous context once the demote has run.
    CloseAndDemote,
}

impl PlacementKey {
    fn verb(self) -> &'static str {
        match self {
            PlacementKey::Promote => "promote",
            PlacementKey::Demote | PlacementKey::CloseAndDemote => "demote",
        }
    }

    /// The key as the hints line names it.
    fn name(self) -> &'static str {
        match self {
            PlacementKey::Promote => "p",
            PlacementKey::Demote => "d",
            PlacementKey::CloseAndDemote => "Ctrl+A q",
        }
    }
}

/// A placement key press, written by the key's handler and run by
/// [`run_placements`].
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlacementRequested {
    pub context_id: ContextId,
    pub key: PlacementKey,
}

/// The one latched placement the next press may confirm.
#[derive(Resource, Default, Debug)]
pub struct PlacementLatch {
    armed: Option<(ContextId, PlacementKey, f64)>,
}

impl PlacementLatch {
    /// The kernel latched `key` on `context_id` at `now`.
    pub fn arm(&mut self, context_id: ContextId, key: PlacementKey, now: f64) {
        self.armed = Some((context_id, key, now));
    }

    /// Whether a press of `key` on `context_id` at `now` confirms: the
    /// kernel latched the same key on the same context, and its notice still
    /// shows. Every press disarms the latch.
    pub fn take(&mut self, context_id: ContextId, key: PlacementKey, now: f64) -> bool {
        matches!(
            self.armed.take(),
            Some((armed, armed_key, at)) if armed == context_id && armed_key == key && now - at < NOTICE_TTL
        )
    }
}

/// The `kj` argv for one press.
pub fn placement_argv(key: PlacementKey, context_id: ContextId, confirm: bool) -> Vec<String> {
    let mut argv = vec!["context".to_string(), key.verb().to_string(), context_id.to_hex()];
    if confirm {
        argv.push("--confirm".to_string());
    }
    argv
}

/// Run each [`PlacementRequested`] on the kernel in a task; the result comes
/// back as [`RpcResultMessage::PlacementFinished`].
pub fn run_placements(
    mut requests: MessageReader<PlacementRequested>,
    actor: Option<Res<RpcActor>>,
    results: Res<RpcResultChannel>,
    mut latch: ResMut<PlacementLatch>,
    mut notice: ResMut<AskNotice>,
    time: Res<Time>,
) {
    let now = time.elapsed_secs_f64();
    for &PlacementRequested { context_id, key } in requests.read() {
        let confirm = latch.take(context_id, key, now);
        let Some(actor) = actor.as_ref() else {
            notice.say(format!("not connected; {} {} did not run", key.verb(), context_id.short()), now);
            continue;
        };
        let argv = placement_argv(key, context_id, confirm);
        let handle = actor.handle.clone();
        let tx = results.sender();
        let span = tracing::info_span!("app.placement", context.id = %context_id, verb = key.verb(), confirm);
        bevy::tasks::IoTaskPool::get()
            .spawn(tracing::Instrument::instrument(
                async move {
                    let result = handle.execute_kj(context_id, argv).await.map_err(|e| e.to_string());
                    let _ = tx.send(RpcResultMessage::PlacementFinished { context_id, key, result });
                },
                span,
            ))
            .detach();
    }
}

/// What a finished placement means for the screen.
#[derive(Debug, PartialEq, Eq)]
enum Landed {
    /// The verb ran and moved the context.
    Moved(String),
    /// The step archives and waits for the same key again.
    Latched(String),
    /// Nothing moved.
    Failed(String),
}

fn landed(context_id: ContextId, key: PlacementKey, result: &Result<KjExecutionResult, String>) -> Landed {
    let short = context_id.short();
    let verb = key.verb();
    match result {
        Err(e) => Landed::Failed(format!("{verb} {short} failed: {e}")),
        Ok(result) if result.latch.is_some() => Landed::Latched(format!(
            "{short} is already demoted, so the next demote archives it; {} again confirms",
            key.name()
        )),
        Ok(result) if result.exit_code != 0 => {
            let why = result.stderr.lines().next().unwrap_or("").trim();
            Landed::Failed(format!("{verb} {short} failed (exit {}): {why}", result.exit_code))
        }
        Ok(result) => Landed::Moved(result.stdout.lines().next().unwrap_or("done").to_string()),
    }
}

/// Land each [`RpcResultMessage::PlacementFinished`]: say what happened on
/// the hints line, arm the latch for a latched step, and for `Ctrl+A q`
/// land on the previous context once the demote has run, only while the
/// demoted context is still on screen. A step that moved nothing frees the
/// well's in-flight guard for that context, so the confirming press is not
/// ignored.
pub fn land_placements(
    mut results: MessageReader<RpcResultMessage>,
    doc_cache: Res<DocumentCache>,
    mut latch: ResMut<PlacementLatch>,
    mut notice: ResMut<AskNotice>,
    mut switch: MessageWriter<ContextSwitchRequested>,
    well: Option<ResMut<crate::view::time_well::scene::TimeWellState>>,
    time: Res<Time>,
) {
    let now = time.elapsed_secs_f64();
    let mut well = well;
    for message in results.read() {
        let RpcResultMessage::PlacementFinished { context_id, key, result } = message else {
            continue;
        };
        let (context_id, key) = (*context_id, *key);
        match landed(context_id, key, result) {
            Landed::Moved(said) => {
                if key == PlacementKey::CloseAndDemote && doc_cache.active_id() == Some(context_id) {
                    match doc_cache.mru_ids().get(1).copied() {
                        Some(previous) => {
                            switch.write(ContextSwitchRequested { context_id: previous });
                            info!("close-and-demote {} -> {}", context_id.short(), previous.short());
                        }
                        None => {
                            notice.say(format!("{said}; no previous context to land on"), now);
                            continue;
                        }
                    }
                }
                notice.say(said, now);
            }
            Landed::Latched(said) => {
                latch.arm(context_id, key, now);
                if let Some(well) = well.as_mut() {
                    well.placement_pending.remove(&context_id);
                }
                notice.say(said, now);
            }
            Landed::Failed(said) => {
                warn!("{said}");
                if let Some(well) = well.as_mut() {
                    well.placement_pending.remove(&context_id);
                }
                notice.say(said, now);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{Action, ActionFired, InputContext};

    fn executed(exit_code: i32, stdout: &str, stderr: &str, latched: bool) -> KjExecutionResult {
        KjExecutionResult {
            exit_code,
            stdout: stdout.into(),
            stderr: stderr.into(),
            command_block_id: None,
            latch: latched.then(|| kaijutsu_client::rpc::KjLatch {
                command: "kj context demote".into(),
                target: "x".into(),
                message: "kj context demote x --confirm".into(),
            }),
            data: None,
        }
    }

    #[test]
    fn a_press_runs_the_kj_verb_on_the_context_it_moves() {
        let id = ContextId::new();
        assert_eq!(placement_argv(PlacementKey::Promote, id, false), vec!["context", "promote", &id.to_hex()]);
        assert_eq!(placement_argv(PlacementKey::Demote, id, false), vec!["context", "demote", &id.to_hex()]);
        assert_eq!(
            placement_argv(PlacementKey::CloseAndDemote, id, true),
            vec!["context", "demote", &id.to_hex(), "--confirm"]
        );
    }

    #[test]
    fn only_the_same_key_on_the_same_context_confirms_while_its_notice_shows() {
        let (a, b) = (ContextId::new(), ContextId::new());
        let mut latch = PlacementLatch::default();
        assert!(!latch.take(a, PlacementKey::Demote, 0.0), "nothing latched, nothing confirms");

        latch.arm(a, PlacementKey::Demote, 10.0);
        assert!(latch.take(a, PlacementKey::Demote, 11.0));
        assert!(!latch.take(a, PlacementKey::Demote, 11.5), "one confirm per latch");

        for (id, key) in [(b, PlacementKey::Demote), (a, PlacementKey::CloseAndDemote)] {
            latch.arm(a, PlacementKey::Demote, 10.0);
            assert!(!latch.take(id, key, 11.0));
            assert!(!latch.take(a, PlacementKey::Demote, 11.0), "any other press disarms it");
        }

        latch.arm(a, PlacementKey::Demote, 10.0);
        assert!(!latch.take(a, PlacementKey::Demote, 10.0 + NOTICE_TTL), "it expires with its notice");
    }

    #[test]
    fn a_latch_is_told_apart_from_a_failure_by_its_latch_not_its_exit_code() {
        let id = ContextId::new();
        let latched = landed(id, PlacementKey::Demote, &Ok(executed(2, "", "confirmation required", true)));
        assert!(matches!(&latched, Landed::Latched(said) if said.contains("d again confirms")), "{latched:?}");
        let failed = landed(id, PlacementKey::Demote, &Ok(executed(1, "", "kj context demote: lacks the 'operator' capability\n", false)));
        assert!(matches!(&failed, Landed::Failed(said) if said.contains("lacks the 'operator' capability")), "{failed:?}");
        let moved = landed(id, PlacementKey::Demote, &Ok(executed(0, "demoted 01a1\n", "", false)));
        assert_eq!(moved, Landed::Moved("demoted 01a1".into()));
    }

    fn prefix_app(active: ContextId, previous: ContextId) -> App {
        let mut app = App::new();
        app.add_message::<ActionFired>()
            .add_message::<PlacementRequested>()
            .add_message::<ContextSwitchRequested>()
            .init_resource::<crate::connection::drift::DriftState>()
            .init_resource::<DocumentCache>()
            .add_systems(Update, super::super::conversation::handle_prefix_context_verbs);
        let mut cache = app.world_mut().resource_mut::<DocumentCache>();
        cache.set_active(previous);
        cache.set_active(active);
        app
    }

    fn requests(app: &App) -> Vec<PlacementRequested> {
        let msgs = app.world().resource::<Messages<PlacementRequested>>();
        msgs.iter_current_update_messages().copied().collect()
    }

    fn switches(app: &App) -> Vec<ContextId> {
        let msgs = app.world().resource::<Messages<ContextSwitchRequested>>();
        msgs.iter_current_update_messages().map(|m| m.context_id).collect()
    }

    /// `Ctrl+A q` asks for `kj context demote` on the context on screen and
    /// does not move until the demote has run: a latched or refused demote
    /// leaves the player where the hints line speaks about.
    #[test]
    fn ctrl_a_q_asks_for_the_kj_demote_and_waits_for_it() {
        let (here, before) = (ContextId::new(), ContextId::new());
        let mut app = prefix_app(here, before);
        app.world_mut().write_message(ActionFired {
            action: Action::CloseAndDemoteContext,
            context: InputContext::Navigation,
        });
        app.update();
        assert_eq!(requests(&app), vec![PlacementRequested { context_id: here, key: PlacementKey::CloseAndDemote }]);
        assert!(switches(&app).is_empty(), "the view moves only once the demote has run");
    }

    /// The well's `p` and `d` ask for the kj verb on the selected card.
    #[test]
    fn the_wells_p_and_d_ask_for_the_kj_verb_on_the_selection() {
        let card = ContextId::new();
        let mut app = App::new();
        app.add_plugins(bevy::state::app::StatesPlugin)
            .init_state::<crate::ui::screen::Screen>()
            .add_message::<ActionFired>()
            .add_message::<PlacementRequested>()
            .add_message::<ContextSwitchRequested>()
            .init_resource::<crate::connection::drift::DriftState>()
            .init_resource::<crate::view::room::RoomState>()
            .init_resource::<crate::view::time_well::scene::TimeWellState>()
            .add_systems(Update, crate::view::time_well::scene::well_keyboard);
        app.world_mut().resource_mut::<crate::view::time_well::scene::TimeWellState>().selected = Some(card);
        for (action, key) in [(Action::Promote, PlacementKey::Promote), (Action::Demote, PlacementKey::Demote)] {
            app.world_mut().write_message(ActionFired { action, context: InputContext::WellZoomed });
            app.update();
            assert_eq!(requests(&app), vec![PlacementRequested { context_id: card, key }]);
            app.world_mut()
                .resource_mut::<crate::view::time_well::scene::TimeWellState>()
                .placement_pending
                .clear();
        }
    }

    fn landing_app(active: ContextId, previous: ContextId) -> App {
        let mut app = App::new();
        app.add_message::<RpcResultMessage>()
            .add_message::<ContextSwitchRequested>()
            .init_resource::<DocumentCache>()
            .init_resource::<PlacementLatch>()
            .init_resource::<AskNotice>()
            .init_resource::<Time>()
            .init_resource::<crate::view::time_well::scene::TimeWellState>()
            .add_systems(Update, land_placements);
        let mut cache = app.world_mut().resource_mut::<DocumentCache>();
        cache.set_active(previous);
        cache.set_active(active);
        app
    }

    fn finish(app: &mut App, context_id: ContextId, key: PlacementKey, result: Result<KjExecutionResult, String>) {
        app.world_mut().write_message(RpcResultMessage::PlacementFinished { context_id, key, result });
        app.update();
    }

    fn notice(app: &App) -> String {
        app.world().resource::<AskNotice>().current(0.0).unwrap_or_default().to_string()
    }

    #[test]
    fn a_demote_that_ran_lands_ctrl_a_q_on_the_previous_context() {
        let (here, before) = (ContextId::new(), ContextId::new());
        let mut app = landing_app(here, before);
        finish(&mut app, here, PlacementKey::CloseAndDemote, Ok(executed(0, "demoted here\n", "", false)));
        assert_eq!(switches(&app), vec![before]);
        assert_eq!(notice(&app), "demoted here");
    }

    #[test]
    fn a_demote_of_a_context_no_longer_on_screen_does_not_move_the_view() {
        let (here, before, gone) = (ContextId::new(), ContextId::new(), ContextId::new());
        let mut app = landing_app(here, before);
        finish(&mut app, gone, PlacementKey::CloseAndDemote, Ok(executed(0, "demoted gone\n", "", false)));
        assert!(switches(&app).is_empty());
    }

    /// The step that archives latches: nothing moves, the hints line names
    /// the key that confirms, the latch is armed for that key and context,
    /// and the well's in-flight guard lets the confirming press through.
    #[test]
    fn a_latched_demote_stays_says_which_key_confirms_and_arms_it() {
        let (here, before) = (ContextId::new(), ContextId::new());
        let mut app = landing_app(here, before);
        app.world_mut()
            .resource_mut::<crate::view::time_well::scene::TimeWellState>()
            .begin_placement(here);
        finish(&mut app, here, PlacementKey::CloseAndDemote, Ok(executed(2, "", "confirmation required", true)));
        assert!(switches(&app).is_empty(), "a latched demote moves nothing");
        assert!(notice(&app).contains("Ctrl+A q again confirms"), "{}", notice(&app));
        assert!(app.world_mut().resource_mut::<PlacementLatch>().take(here, PlacementKey::CloseAndDemote, 0.0));
        assert!(
            app.world_mut()
                .resource_mut::<crate::view::time_well::scene::TimeWellState>()
                .begin_placement(here),
            "the latch freed the in-flight guard"
        );
    }

    #[test]
    fn a_refused_demote_stays_and_says_why() {
        let (here, before) = (ContextId::new(), ContextId::new());
        let mut app = landing_app(here, before);
        finish(&mut app, here, PlacementKey::CloseAndDemote, Err("not connected".into()));
        assert!(switches(&app).is_empty());
        assert!(notice(&app).contains("not connected"), "{}", notice(&app));
        assert!(!app.world_mut().resource_mut::<PlacementLatch>().take(here, PlacementKey::CloseAndDemote, 0.0));
    }
}
